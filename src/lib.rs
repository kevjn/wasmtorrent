#![feature(slice_as_chunks)]
#![feature(array_chunks)]
#![feature(iterator_try_collect)]
#![feature(split_array)]
#![feature(exact_size_is_empty)]
#![feature(trait_alias)]

extern crate serde;
extern crate serde_bencode;
#[macro_use]
extern crate serde_derive;
extern crate serde_bytes;

use bincode::Options;
use bit_vec::BitVec;
use futures::{StreamExt, FutureExt};
use sha1::{Sha1, Digest};
use serde_bytes::ByteBuf;
use tokio::{io::{AsyncRead, AsyncWriteExt, AsyncReadExt, AsyncWrite}, sync::mpsc};
use std::{time::Duration, io::{ErrorKind as IoErrorKind, Read, Seek, SeekFrom, Write}, net::{IpAddr, SocketAddr, TcpListener, TcpStream}, sync::{Arc, Mutex}, task::Poll, rc::Rc};
use std::io::{Result as IoResult};

use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use byteorder::{ByteOrder, BigEndian, WriteBytesExt, ReadBytesExt};
use crossbeam_channel::{bounded, Sender, Receiver};

#[macro_use] 
extern crate log;

#[macro_use]
extern crate lazy_static;

// https://github.com/rust-webplatform/rust-todomvc/blob/51cbd62e906a6274d951fd7a8f5a6c33fcf8e7ea/src/main.rs#L34-L41
macro_rules! enclose {
    ( ($( $x:ident ),*) $y:expr ) => {
        {
            $(let $x = $x.clone();)*
            $y
        }
    };
}

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::*, JsCast};

#[derive(Debug, Deserialize, Serialize)]
struct BencodeInfo {
    pieces: ByteBuf,
    #[serde(rename = "piece length")]
    piece_length: i64,
    #[serde(default)]
    length: Option<i64>,
    name: String,
    private: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct BencodeTorrent {
    info: BencodeInfo,
    #[serde(default)]
    announce: Option<String>,
}

// A flat structure for working with single torrent files
#[derive(Debug)]
pub struct Torrent {
    pub announce: String,
    pub info_hash: [u8; 20],
    pub pieces: Vec<[u8; 20]>,
    pub piece_len: i64,
    pub file_len: i64,
    pub name: String,
    pub bitfield: BitVec,
    work_tx: Sender<PieceWork>,
    work_rx: Receiver<PieceWork>,
}

#[derive(Debug)]
pub struct PieceWork {
    pub index: usize,
    pub hash: [u8; 20],
    pub len: u32,
}

#[derive(Debug, Deserialize)]
struct PieceRequest {
    pub index: u32,
    pub requested: u32,
    pub len: u32,
}

#[derive(Debug)]
pub struct PieceResult {
    pub index: usize,
    pub buf: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Handshake {
    len: u8,
    protocol: [u8; 19],
    reserved: [u8; 8],
    info_hash: [u8; 20],
    peer_id: [u8; 20],
}

#[derive(Debug, PartialEq)]
struct Message {
    id: u8,
    payload: Vec<u8>,
}

// wrapper for websys datachannel
#[cfg(target_arch = "wasm32")]
pub struct DataStream {
    inner: web_sys::RtcDataChannel,
    rx_inbound: futures::channel::mpsc::Receiver<Vec<u8>>,
}

#[cfg(target_arch = "wasm32")]
impl DataStream {
    pub fn new(inner: web_sys::RtcDataChannel) -> Self {
        inner.set_binary_type(web_sys::RtcDataChannelType::Arraybuffer);
        let (mut tx, rx_inbound) = futures::channel::mpsc::channel(32);
        let onmessage = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::MessageEvent| {
            let data: Vec<u8> = match ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                Ok(data) => {
                    let bytes: Vec<u8> = js_sys::Uint8Array::new(&data).to_vec();
                    bytes
                }
                Err(data) => {
                    panic!("error reading from RtcDataChannel");
                }
            };
            if let Err(e) = tx.try_send(data) {
                error!("Error sending via channel: {:?}", e);
            }
        });
        inner.add_event_listener_with_callback("message", onmessage.as_ref().unchecked_ref()).unwrap();
        onmessage.forget();
        Self { inner, rx_inbound }
    }
}

#[cfg(target_arch = "wasm32")]
impl tokio::io::AsyncRead for DataStream {
    fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<IoResult<()>> {
        match self.as_mut().rx_inbound.poll_next_unpin(cx) {
             Poll::Ready(Some(x)) => {
                buf.put_slice(&x[..]);
                Poll::Ready(Ok(()))
             }
             Poll::Ready(None) => Poll::Ready(Ok(())),
             Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl tokio::io::AsyncWrite for DataStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.as_ref().inner.send_with_u8_array(buf) {
            Ok(()) => {
                Poll::Ready(Ok(buf.len()))
            }
            Err(e) => {
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e))))
            }
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl<'de> serde::Deserialize<'de> for Torrent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
        {
        let torrent: BencodeTorrent = serde::Deserialize::deserialize(deserializer)?;

        // calculate sha1 hash for Torrent info
        let bytes = serde_bencode::to_bytes(&torrent.info).unwrap();
        let info_hash = Sha1::digest(bytes).into();

        // split pieces into slice of hashes where each slice is 20 bytes
        let pieces: &[[u8; 20]] = torrent.info.pieces.as_chunks().0;

        // initialize empty bitfield
        let bitfield = BitVec::from_elem(pieces.len(), false);

        // initialize work queue
        let (work_tx, work_rx) =  bounded::<PieceWork>(pieces.len());

        Ok(Torrent {
            announce: torrent.announce.unwrap_or_default(),
            info_hash,
            pieces: pieces.into(),
            piece_len: torrent.info.piece_length,
            file_len: torrent.info.length.unwrap(),
            name: torrent.info.name,
            bitfield,
            work_tx,
            work_rx
        })
    }
}

impl From<Vec<u8>> for Torrent {
    fn from(bytes: Vec<u8>) -> Self {
        serde_bencode::from_bytes(&bytes).unwrap()
    }
}

pub struct ToChunks<R> {
    reader: R,
    chunk_size: usize,
}

impl<R: Read> Iterator for ToChunks<R> {
    type Item = IoResult<Vec<u8>>;
    
    fn next(&mut self) -> Option<Self::Item> {
        let mut buffer = vec![0u8; self.chunk_size];
        match self.reader.read_exact(&mut buffer) {
            Ok(()) => Some(Ok(buffer)),
            Err(e) if e.kind() == IoErrorKind::UnexpectedEof => None,
            Err(e) => Some(Err(e)),
        }
    }
}

pub trait IterChunks {
    type Output;
    
    fn iter_chunks(self, len: usize) -> Self::Output;
}

impl<R: Read> IterChunks for R {
    type Output = ToChunks<R>;
    
    fn iter_chunks(self, len: usize) -> Self::Output {
        ToChunks {
            reader: self,
            chunk_size: len,
        }
    }
}

fn build_query_string<'a, I>(pairs: I) -> String 
where I: IntoIterator<Item = (&'a str, &'a str)> {
    pairs.into_iter()
        .map(|(key, value)| format!("{}={}", key, value))
        .collect::<Vec<String>>()
        .join("&")
}

impl Torrent {
    fn build_tracker_url(&self, peer_id: &[u8; 20], port: i64) -> String {

        // represent binary data as url-encoded strings
        let info_hash = percent_encode(&self.info_hash, NON_ALPHANUMERIC);
        let peer_id = percent_encode(peer_id, NON_ALPHANUMERIC);

        let query = build_query_string([
            ("compact", "1"),
            ("downloaded", "0"),
            ("info_hash", &info_hash.to_string()),
            ("left", &self.file_len.to_string()),
            ("peer_id", &peer_id.to_string()),
            ("port", &port.to_string()),
            ("uploaded", "0"),
        ]);

        format!("{}?{}", self.announce, query)
    }

    #[cfg(not(target_arch = "wasm32"))]
        pub async fn request_peers(&self) -> impl futures::stream::Stream<Item = impl futures::Future<Output = IoResult<impl AsyncRead + AsyncWrite + Unpin>>> {
        // identifies the file we want to download
        let tracker_url = self.build_tracker_url(&PEER_ID, 8080);
        // announce our presence to the tracker, TODO: don't use unwrap here
        let bytes = reqwest::get(tracker_url).await.unwrap().bytes().await.unwrap();
        let response: BencodeTrackerResp = serde_bencode::from_bytes(&bytes).unwrap();

        futures::stream::iter((0..response.peers.len()).step_by(6).map(move |i| {
            let x = &response.peers[i..i + 6];
            let (&ip, port) = x.split_array_ref::<4>();
            let s = SocketAddr::new(IpAddr::from(ip), BigEndian::read_u16(port));
            tokio::net::TcpStream::connect(s)
        }))
    }

    pub fn init_state<R: Read>(&mut self, file: &mut R) -> IoResult<()> {
        let mut file_pieces = file.iter_chunks(self.piece_len as usize);

        for (i, &piece_hash) in self.pieces.iter().enumerate() {
            if let Some(file_piece) = file_pieces.next() {
                let file_piece_hash: [u8; 20] = Sha1::digest(file_piece?).into();
                if file_piece_hash == piece_hash {
                    // add piece to our bitfield
                    self.bitfield.set(i, true);
                    continue;
                }
            }
            // add piece as work item
            let begin = (i as i64) * self.piece_len;
            let piece_len = (begin+self.piece_len).min(self.file_len) - begin;
            self.work_tx.send(PieceWork {
                index: i,
                hash: piece_hash,
                len: piece_len as u32,
            }).expect("Unable to fill work queue");
        }

        Ok(())

    }

    pub async fn download<W: Write + Seek>(mut self, file: &mut W, peers: impl futures::stream::Stream<Item = impl futures::Future<Output = IoResult<impl AsyncRead + AsyncWrite + Unpin>>>) -> IoResult<()> {
        info!("Starting download for {}", self.name);

        if self.work_tx.is_empty() {
            // fill work queue
            for (i, &piece_hash) in self.pieces.iter().enumerate() {
                let begin = (i as i64) * self.piece_len;
                let piece_len = (begin+self.piece_len).min(self.file_len) - begin;
                self.work_tx.send(PieceWork {
                    index: i,
                    hash: piece_hash,
                    len: piece_len as u32,
                }).expect("Unable to fill work queue");
            }
        }
        
        // store the result in a multi-producer, single-consumer queue
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<PieceResult>(self.pieces.len());

        // a multi-producer, multi-consumer queue with specified capacity
        let (work_tx, work_rx) = (&self.work_tx, &self.work_rx);
        
        // counter for number of active peers
        let num_peers= Rc::new(Mutex::new(0));

        let workers = peers.for_each_concurrent(None, |f| {
            enclose! { (work_tx, work_rx, result_tx, num_peers) async move {
                if let Ok(peer) = f.await {
                    *num_peers.lock().unwrap() += 1;
                    if let Err(e) = start_download_task(peer, work_tx, work_rx, result_tx, &self.info_hash).await {
                        info!("Disconnecting from peer with error: {:?}", e);
                        *num_peers.lock().unwrap() -= 1;
                    } else {
                        info!("Done with download task");
                    }
                }
            } }
        }).boxed_local();

        let mut downloaded_pieces = self.bitfield.iter().fold(0, |acc, x| acc + x as usize);

        // collect download results
        let result = async {
            while downloaded_pieces < self.pieces.len() {
                let res = result_rx.recv().await.unwrap();
                let begin = (res.index as i64) * self.piece_len;
                file.seek(SeekFrom::Start(begin as u64)).unwrap();
                file.write_all(&res.buf).unwrap();
                downloaded_pieces += 1;
                let percent = (downloaded_pieces as f64 / self.pieces.len() as f64) * 100.0;
                info!("({:.2}%) Downloaded piece #{:?} from {:?} peers", percent, res.index, *num_peers.lock().unwrap());
                self.bitfield.set(res.index, true);
            }
            info!("done with download for {:?}", self.name);
        }.boxed_local();

        // wait for either one of two futures to complete.
        futures::future::select(workers, result).await;

        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn seed<R: Read + Seek + Send + 'static>(&self, file: R, listen_addr: SocketAddr) -> IoResult<()> {
        info!("start listening on {:?}", listen_addr);
        let listener = TcpListener::bind(listen_addr)?;
        let file = Arc::new(Mutex::new(file));
        
        for socket in listener.incoming() {
            match socket {
                Ok(socket) => {
                    info!("new connection from {:?}", socket.peer_addr().unwrap());
                    let f = file.clone();
                    let info_hash = self.info_hash.clone();
                    let bitfield = self.bitfield.clone();
                    let piece_len = self.piece_len.clone();
                    std::thread::spawn(move || {
                        todo!()
                        // start_upload_task(socket, piece_len, &info_hash, &bitfield, f);
                    });
                }
                Err(e) => println!("couldn't get client: {:?}", e),
            }
        }

        Ok(())
    }

    pub async fn seed_to_connections<R: Read + Seek + Send + 'static>(mut self, file: R, peers: impl futures::stream::Stream<Item = impl futures::Future<Output = IoResult<impl AsyncRead + AsyncWrite + Unpin>>>) {
        let file = Arc::new(Mutex::new(file));

        // assume the file is complete
        self.bitfield.set_all();
        let bitfield = &self.bitfield;

        peers.for_each_concurrent(None, |peer| {
            enclose! { (file) async move {
                if let Ok(peer) = peer.await {
                    if let Err(e) = start_upload_task(peer, self.piece_len, &self.info_hash, &bitfield, file).await {
                        info!("error: {:?}", e.to_string());
                    }
                }
            } }
        }).await;
    }

}

#[derive(Debug, Deserialize)]
struct BencodeTrackerResp {
    peers: ByteBuf,
}

// const PEER_ID: &[u8; 20] = b"wasmtorrent-12345678";

lazy_static! {
    /// This is an random id generated once at runtime
    static ref PEER_ID: [u8; 20] = {
        rand::Rng::sample_iter(rand::thread_rng(), &rand::distributions::Alphanumeric)
                .take(20)
                .collect::<Vec<u8>>().try_into().unwrap()
    };
}

impl Handshake {
    fn new(info_hash: &[u8; 20]) -> Handshake {
        Handshake {
            len: 19,
            protocol: b"BitTorrent protocol".to_owned(),
            reserved: [0u8; 8],
            info_hash: *info_hash, 
            peer_id: *PEER_ID,
        }
    }
}

mod btid {
    pub const CHOKE: u8 = 0;
    pub const UNCHOKE: u8 = 1;
    pub const INTEREST: u8 = 2;
    pub const UNINTEREST: u8 = 3;
    pub const HAVE: u8 = 4;
    pub const BITFIELD: u8 = 5;
    pub const REQUEST: u8 = 6;
    pub const PIECE: u8 = 7;
    pub const CANCEL: u8 = 8;
}

impl Message {
    async fn serialize<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> IoResult<()>
    {
        writer.write_u32((self.payload.len() + 1) as u32).await?;
        writer.write_u8(self.id).await?;
        writer.write_all(&self.payload).await?;

        Ok(())
    }

    async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> IoResult<Option<Self>>
    {
        let len = reader.read_u32().await?;

        // keep-alive message
        if len == 0 {
            return Ok(None);
        }

        let mut buf = vec![0u8; len as usize];
        reader.read_exact(&mut buf).await?;

        Ok(
            Some(Message {
                id: buf[0],
                payload: buf[1..].to_vec(),
            })
        )
    }
}

async fn send_message<W: AsyncWrite + Unpin>(peer: &mut W, id: u8, payload: Option<Vec<u8>>) -> IoResult<()> {
    let msg = Message {
        id,
        payload: payload.unwrap_or_default()
    };
    debug!("sending {:?}", msg);
    msg.serialize(peer).await
}

pub async fn start_download_task<A: AsyncRead + AsyncWriteExt + Unpin>(
    mut peer: A, 
    work_tx: Sender<PieceWork>, 
    work_rx: Receiver<PieceWork>, 
    result_tx: tokio::sync::mpsc::Sender<PieceResult>, 
    info_hash: &[u8; 20]
) -> Result<(), Box<dyn std::error::Error>> {

    // send handshake
    let req = Handshake::new(info_hash);
    let mut bytes = bincode::serialize(&req).unwrap();
    peer.write_all(&bytes).await.unwrap();

    // recieve handshake
    peer.read_exact(&mut bytes).await?;
    let res: Handshake = bincode::deserialize(&bytes).unwrap();

    // verify infohash
    if res.info_hash != *info_hash {
        return Err(bincode::ErrorKind::Custom("invalid info hash".to_string()).into());
    }

    // receive bitfield
    let res = Message::read(&mut peer).await?.unwrap();
    if res.id != btid::BITFIELD {
        let error_message = format!("Expected bitfield but got {:?}", res.id);
        return Err(bincode::ErrorKind::Custom(error_message).into());
    }

    let mut bitfield = BitVec::from_bytes(&res.payload);
    let mut choked = true;

    send_message(&mut peer, btid::UNCHOKE, None).await?;
    send_message(&mut peer, btid::INTEREST, None).await?;

    while let Ok(pw) = work_rx.try_recv() {
        if !bitfield[pw.index] {
            work_tx.send(pw)?; // put piece back on the queue
            continue;
        }

        // Download the piece
        let buf = match attempt_download_piece(&mut peer, &mut choked, &mut bitfield, &pw).await {
            Ok(buf) => buf,
            Err(error) => {
                work_tx.send(pw)?;
                error!("Failed to download piece {:?}, Exiting", error);
                return Err(error);
            }
        };
        
        // check integrity of the piece
        let hash: [u8; 20] = Sha1::digest(&buf).into();
        if hash != pw.hash {
            work_tx.send(pw)?;
            error!("hashes do not match");
            continue;
        }

        // notify peer that we have the piece
        send_message(&mut peer, btid::HAVE, Some((pw.index as u32).to_be_bytes().to_vec())).await?;
        
        // add piece to result
        result_tx.send(PieceResult { buf, index: pw.index }).await?;
    }
    Ok(())

}

async fn attempt_download_piece<A: AsyncRead + AsyncWrite + Unpin>(peer: &mut A, choked: &mut bool, bitfield: &mut BitVec, pw: &PieceWork) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const MAX_BACKLOG: i32 = 5;
    const MAX_BLOCKSIZE: u32 = 16384;

    let mut downloaded = 0;
    let mut backlog = 0;
    let mut requested = 0;

    let mut buf =  vec![0u8; pw.len as usize]; 

    while downloaded < pw.len {
        if !*choked {
            // if unchoked, send requests untill we have enough unfulfilled requests
            while backlog < MAX_BACKLOG && requested < pw.len {
                // Last block might be shorter than the max blocksize
                let blocksize = pw.len.min(requested+MAX_BLOCKSIZE) - requested;

                send_message(peer, btid::REQUEST, Some([
                    (pw.index as u32).to_be_bytes(), 
                    requested.to_be_bytes(), 
                    blocksize.to_be_bytes()
                ].concat())).await?;

                backlog += 1;
                requested += blocksize;
            }
        }

        // read message
        let msg = Message::read(peer).await?;
        if msg == None {
            // keep-alive
            continue;
        }
        let msg = msg.unwrap();
        match msg.id {
            btid::CHOKE => { 
                *choked = true; 
            },
            btid::UNCHOKE => { 
                *choked = false; 
            },
            btid::HAVE => {
                let index = u32::from_be_bytes(msg.payload.try_into().unwrap());
                bitfield.set(index as usize, true);
            },
            btid::PIECE => {
                let index = u32::from_be_bytes(msg.payload[..4].try_into().unwrap());
                // compare both index
                if index as usize != pw.index {
                    return Err(format!("Expected index {}, got {}", index, pw.index).into());
                }
                let begin = u32::from_be_bytes(msg.payload[4..8].try_into().unwrap()) as usize;
                if begin > buf.len() {
                    return Err("Begin offset to high".into());
                }
                let data = &msg.payload[8..];
                if begin + data.len() > buf.len() {
                    return Err(format!("Data too long [{}] for offset {} with len {}", data.len(), begin, buf.len()).into());
                }
                buf[begin..begin+data.len()].clone_from_slice(data);
                downloaded += data.len() as u32;
                backlog -= 1;
            },
            _ => panic!("unexpected message: {:?}", msg.id)
        };
    }

    Ok(buf)
}

async fn start_upload_task<R: Read + Seek, A: AsyncRead + AsyncWriteExt + Unpin>(
    mut peer: A, 
    piece_len: i64, 
    info_hash: &[u8; 20], 
    bitfield: &BitVec, 
    file: Arc<Mutex<R>>
) -> Result<(), Box<dyn std::error::Error>> {

    // recieve handshake
    let mut buf = vec![0; 68];
    peer.read_exact(&mut buf).await?;
    let res: Handshake = bincode::deserialize(&buf).unwrap();
    
    // verify infohash
    if res.info_hash != *info_hash {
        return Err(bincode::ErrorKind::Custom("invalid info hash".to_string()).into());
    }
    
    // send handshake
    let req = Handshake::new(info_hash);
    let bytes = bincode::serialize(&req).unwrap();
    peer.write_all(&bytes).await.unwrap();

    // send bitfield
    info!("sending bitfield");
    send_message(&mut peer, btid::BITFIELD, Some(bitfield.to_bytes())).await?;

    // send unchoke
    info!("sending unchoke");
    send_message(&mut peer, btid::UNCHOKE, None).await?;

    // wait for client to request piece
    loop {
        let msg = match Message::read(&mut peer).await {
            Ok(Some(msg)) => msg,
            Ok(None) => continue,
            Err(_) => break,
        };

        info!("new message: {:?}", msg);

        match msg.id {
            btid::REQUEST => {
                let pr: PieceRequest = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize(&msg.payload)?;

                let begin = (pr.index * piece_len as u32) + pr.requested;
                let mut buf = vec![0u8; pr.len as usize];
                {
                    let mut file = file.lock().unwrap();
                    file.seek(std::io::SeekFrom::Start(begin as u64))?;
                    file.read_exact(&mut buf)?;
                }
                send_message(&mut peer, btid::PIECE, Some([
                    &pr.index.to_be_bytes(),
                    &pr.requested.to_be_bytes(),
                    buf.as_slice(),
                ].concat())).await?;
            }
            _ => continue
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_build_tracker_url() {
        let to = Torrent {
            announce: "http://bttracker.debian.org:6969/announce".to_string(),
            info_hash: [216, 247, 57, 206, 195, 40, 149, 108, 204, 91, 191, 31, 134, 217, 253, 207, 219, 168, 206, 182],
            pieces: [
                [49, 50, 51, 52, 53, 54, 55, 56, 57, 48, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106],
                [97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 49, 50, 51, 52, 53, 54, 55, 56, 57, 48]
            ].into(),
            piece_len: 262144,
            file_len: 351272960,
            name: "debian-10.2.0-amd64-netinst.iso".to_string(),
        };

        let peer_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20];
        let port = 6881;

        let url = to.build_tracker_url(&peer_id, port);
        let expected = "http://bttracker.debian.org:6969/announce?compact=1&downloaded=0&info_hash=%D8%F79%CE%C3%28%95l%CC%5B%BF%1F%86%D9%FD%CF%DB%A8%CE%B6&left=351272960&peer_id=%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F%10%11%12%13%14&port=6881&uploaded=0";
        assert_eq!(url, expected);
    }

    #[test]
    fn test_request_peers() {
        let str = unsafe { 
            format!("d8:intervali900e5:peers12:{}{}e", 
                std::str::from_utf8_unchecked(&[192, 0, 2, 123, 0x1A, 0xE1]), 
                std::str::from_utf8_unchecked(&[127, 0, 0, 1, 0x1A, 0xE9])
            )
        };

        let response: BencodeTrackerResp = serde_bencode::from_bytes(str.as_bytes()).unwrap();

        let r = response.peers.array_chunks().map(|x: &[u8; 6]| {
            let ip: [u8; 4] = x[0..4].try_into().unwrap();
            SocketAddr::new(IpAddr::from(ip), BigEndian::read_u16(&x[4..])).to_string()
        }).collect::<Vec<String>>();

        assert_eq!(r, ["192.0.2.123:6881", "127.0.0.1:6889"]);
    }

    #[test]
    fn test_handshake() {
        let handshake = Handshake {
            len: 19,
            protocol: b"BitTorrent protocol".to_owned(),
            reserved: [0, 0, 0, 0, 0, 0, 0, 0],
            info_hash: [134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116],
            peer_id: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]
        };

        let encoded = bincode::serialize(&handshake).unwrap();

        assert_eq!(encoded, [19, 66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99, 111, 108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
    }

    #[test]
    fn test_parse_message() {
        // Have Message
        let mut input: &[u8] = &[0, 0, 0, 5, 4, 1, 2, 3, 4];
        let msg = Message::read(&mut input).unwrap();
        assert_eq!(msg, Some(Message {
            id: btid::HAVE,
            payload: [1,2,3,4].to_vec(),
        }));

        // keep-alive Message
        let mut input: &[u8] = &[0,0,0,0];
        let msg = Message::read(&mut input).unwrap();
        assert_eq!(msg, None);

        // Choke Message
        let mut input: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x01];
        let msg = Message::read(&mut input).unwrap().unwrap();

        assert_eq!(msg, Message {
            id: btid::UNCHOKE,
            payload: vec![],
        });
    }

}
