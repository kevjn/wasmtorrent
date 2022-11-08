#![feature(slice_as_chunks)]
#![feature(split_array)]
#![feature(int_roundings)]

extern crate serde;
extern crate serde_bencode;
#[macro_use]
extern crate serde_derive;
extern crate serde_bytes;

use bincode::Options;
use bit_vec::BitVec;
use futures::{StreamExt};
use sha1::{Sha1, Digest};
use serde_bytes::ByteBuf;
use tokio::{io::{AsyncRead, AsyncWriteExt, AsyncReadExt, AsyncWrite, AsyncSeek, AsyncSeekExt}};
use std::{io::{SeekFrom}, net::{IpAddr, SocketAddr}, rc::Rc, collections::{HashMap}, cell::{RefCell}, error::Error, fmt::Debug, panic::Location, task::Poll};
use tokio::sync::Mutex;
use std::io::{Result as IoResult};

use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use byteorder::{ByteOrder, BigEndian};
use crossbeam_channel::{Sender, Receiver};

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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BencodeInfo {
    pieces: ByteBuf,
    #[serde(rename = "piece length")]
    piece_len: i64,
    #[serde(default)]
    length: Option<i64>,
    pub name: String,
    private: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct BencodeTorrent {
    pub info: BencodeInfo,
    #[serde(default)]
    announce: Option<String>, // tracker url
}

// A flat structure for working with single torrent files
#[derive(Debug, Clone)]
pub struct Torrent {
    pub metadata: Option<BencodeInfo>,
    pub announce: Option<String>,
    pub info_hash: [u8; 20],
    pub bitfield: BitVec,    
    piece_trx: Option<(Sender<PieceWork>, Receiver<PieceWork>)>,
    piece_len: Option<i64>,
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

#[derive(Debug, Serialize, Deserialize)]
struct ExtMsg {
    msg_type: i32,
    piece: i32,
    total_size: Option<i32>,
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

#[derive(Debug)]
pub struct PeerState {
    bitfield: BitVec,
    choked: bool,
    metadata_size: usize,
    message_id: u8,
}

pub struct LocatedError<T> {
    inner: T,
    location: &'static Location<'static>
}

impl std::fmt::Debug for LocatedError<Box<dyn Error>> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}, {}", self.inner, self.location)
    }
}

impl<E: Error + 'static> From<E> for LocatedError<Box<dyn Error>> {
    #[track_caller]
    fn from(err: E) -> Self {
        return LocatedError { 
            inner: Box::new(err), 
            location: std::panic::Location::caller() 
        }
    }
}

#[derive(Clone, Debug)]
pub enum Task {
    DownloadMetadata,
    DownloadPieces,
    SeedPieces,
    EnqueuePieces,
}

impl Torrent {
    pub fn from_torrent_file(bytes: Vec<u8>) -> Self {
        let torrent: BencodeTorrent = serde_bencode::from_bytes(&bytes).unwrap();

        // calculate sha1 hash for Torrent info
        let bytes = serde_bencode::to_bytes(&torrent.info).unwrap();
        let info_hash = Sha1::digest(bytes).into();

        // split pieces into slice of hashes where each slice is 20 bytes
        let pieces: &[[u8; 20]] = torrent.info.pieces.as_chunks().0;

        // initialize empty bitfield
        let bitfield = BitVec::from_elem(pieces.len(), false);

        Torrent {
            metadata: Some(torrent.info),
            announce: torrent.announce,
            info_hash,
            bitfield,
            piece_trx: None,
            piece_len: None,
        }
    }

    pub fn from_info_hash(bytes: [u8; 20])  -> Self {
        Torrent {
            metadata: None,
            announce: None,
            info_hash: bytes,
            bitfield: BitVec::default(),
            piece_trx: None,
            piece_len: None,
        }
    }

    fn build_tracker_url(&self, peer_id: &[u8; 20], port: i64) -> String {
        let torrent = self.metadata.as_ref().unwrap();

        // represent binary data as url-encoded strings
        let info_hash = percent_encode(&self.info_hash, NON_ALPHANUMERIC);
        let peer_id = percent_encode(peer_id, NON_ALPHANUMERIC);

        let query = [
            ("compact", "1"),
            ("downloaded", "0"),
            ("info_hash", &info_hash.to_string()),
            ("left", &torrent.length.unwrap().to_string()),
            ("peer_id", &peer_id.to_string()),
            ("port", &port.to_string()),
            ("uploaded", "0"),
        ]
            .into_iter()
            .map(|(key, value)| format!("{}={}", key, value))
            .collect::<Vec<String>>()
            .join("&");

        format!("{}?{}", self.announce.as_ref().unwrap(), query)
    }

    #[cfg(not(target_arch = "wasm32"))]
        pub async fn request_peers(&self) -> impl futures::stream::Stream<Item = impl futures::Future<Output = IoResult<impl AsyncRead + AsyncWrite + Debug>>> {
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

    pub async fn start(
        self, 
        peers: impl futures::stream::Stream<Item = impl futures::Future<Output = IoResult<impl AsyncWrite + AsyncRead + Unpin + Debug>>>,
        tasks: &[Task],
        file: impl AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    ) {

        // setup global state
        let state = Rc::new(RefCell::new(self));

        let (tx_task, rx_task) = tokio::sync::watch::channel(tasks.to_vec());
        let tx_task_ref = &tx_task;

        let file = Rc::new(Mutex::new(file));

        peers.for_each_concurrent(None, |peer_fut| {
            let rx_task = rx_task.clone();
            let tx_task = tx_task_ref.clone();
            let state = state.clone();
            let file = file.clone();
            async move {
                if let Ok(mut peer) = peer_fut.await {
                    Torrent::handle_peer(&mut peer, rx_task, tx_task, state, file).await
                        .unwrap_or_else(|err | {
                            warn!("Disconnecting from peer with error: {:?}", err);
                        });
                }
            }
        }).await;

    }

    pub async fn handle_peer(
        mut peer: impl AsyncWrite + AsyncRead + Unpin + Debug,
        mut rx_task: tokio::sync::watch::Receiver<Vec<Task>>,
        tx_task: &tokio::sync::watch::Sender<Vec<Task>>,
        state: Rc<RefCell<Torrent>>,
        file: Rc<Mutex<impl AsyncRead + AsyncWrite + AsyncSeek + Unpin>>
    ) -> Result<(), LocatedError<Box<dyn std::error::Error>>> {
        let info_hash = state.borrow().info_hash;
        // setup local state
        let mut ps = PeerState { bitfield: BitVec::default(), choked: true, metadata_size: 0, message_id: 0 };

        // handshake
        let req = Handshake {
            len: 19,
            protocol: b"BitTorrent protocol".to_owned(),
            reserved: [0, 0, 0, 0, 0, 0x10, 0, 0],
            info_hash,
            peer_id: *PEER_ID,
        };

        let mut bytes = bincode::serialize(&req)?;
        peer.write_all(&bytes).await?;
        peer.read_exact(&mut bytes).await?;
        let res: Handshake = bincode::deserialize(&bytes)?;

        // verify info hash
        if res.info_hash != info_hash {
            panic!("invalid infohash")
        }

        #[derive(Debug, Deserialize, Serialize)]
        struct ExtHandshake {
            m: HashMap<String, i32>,
            metadata_size: Option<u32>
        }

        let req = ExtHandshake {
            m: HashMap::from([("ut_metadata".to_owned(), 1)]),
            metadata_size: None,
        };
        let mut bytes = serde_bencode::to_bytes(&req)?;
        bytes.insert(0, 0u8);
        send_message(&mut peer, opcode::EXTENDED, Some(bytes)).await?;

        let msg = Message::read(&mut peer, &[opcode::EXTENDED], &mut ps).await?;
        let (&id, payload) = msg.payload.split_first().unwrap();

        if id != 0u8 {
            return Err(LocatedError::from(std::io::Error::new(std::io::ErrorKind::Other, "wrong id")));
        }
        let res: ExtHandshake = serde_bencode::from_bytes(&payload)?;
        ps.metadata_size = res.metadata_size.unwrap() as usize;
        ps.message_id = res.m["ut_metadata"] as u8;

        // complete tasks
        while let Some(current_task) = { let x = rx_task.borrow_and_update().first().map(|x| x.clone()); x } {
            match current_task {
                Task::DownloadMetadata => {
                    tokio::select! {
                        biased;
                        _ = rx_task.changed() => {}
                        metadata = async {
                            // move this code to function
                            const METADATA_PIECE_LEN: usize = 16384;
                            let mut metadata = vec![0u8; ps.metadata_size];

                            // request pieces
                            let num_pieces = ps.metadata_size.div_ceil(METADATA_PIECE_LEN);
                            for i in 0..num_pieces {
                                let req = ExtMsg {
                                    msg_type: 0, piece: i as i32, total_size: None
                                };
                                let mut bytes = serde_bencode::to_bytes(&req).unwrap();
                                bytes.insert(0, ps.message_id);
                                send_message(&mut peer, opcode::EXTENDED, Some(bytes)).await.unwrap();

                                let msg = Message::read(&mut peer, &[opcode::EXTENDED], &mut ps).await.unwrap();
                                let (_id, payload) = msg.payload.split_first().unwrap();

                                let data: ExtMsg = serde_bencode::from_bytes(&payload).unwrap();
                                let begin = (data.piece as usize) * METADATA_PIECE_LEN;
                                let end = (begin + METADATA_PIECE_LEN).min(ps.metadata_size) - begin;

                                metadata[begin..begin+end].clone_from_slice(&msg.payload[msg.payload.len()-end..]);
                            }

                            metadata

                        } => {
                            if Sha1::digest(&metadata).as_slice() != info_hash {
                                return Err(LocatedError::from(std::io::Error::new(std::io::ErrorKind::Other, "invalid info hash")));
                            }
                            let torrent: BencodeInfo = serde_bencode::from_bytes(&metadata)?;
                            let mut state = state.borrow_mut();
                            state.metadata = Some(torrent);
                            tx_task.send_modify(|tasks| { tasks.remove(0); });
                        }
                    }
                }

                Task::EnqueuePieces => {
                    // this is done synchronous, so there should not be any interleaving
                    let mut state = state.borrow_mut();
                    let torrent: &BencodeInfo = state.metadata.as_ref().expect("Unable to enqueue pieces without metadata");
                    // split pieces into slice of hashes where each slice is 20 bytes
                    let pieces: &[[u8; 20]] = torrent.pieces.as_chunks().0;
                    let (piece_tx, piece_rx) = crossbeam_channel::bounded(pieces.len());
                    for (i, &piece_hash) in pieces.iter().enumerate() {
                        let begin = (i as i64) * torrent.piece_len;
                        let piece_len = (begin + torrent.piece_len).min(torrent.length.unwrap()) - begin;
                        piece_tx.send(PieceWork {
                            index: i,
                            hash: piece_hash,
                            len: piece_len as u32,
                        })?;
                    }
                    state.piece_len = Some(torrent.piece_len); // remove?
                    state.piece_trx = Some((piece_tx, piece_rx));
                    
                    tx_task.send_modify(|tasks| { tasks.remove(0); });
                }

                Task::DownloadPieces => {
                    if ps.bitfield.is_empty() {
                        Message::read(&mut peer, &[opcode::BITFIELD], &mut ps).await?;
                    }

                    let state = state.borrow();
                    // assume the work queue has been filled
                    let (piece_tx, piece_rx) = state.piece_trx.as_ref().expect("No pieces are enqueued");

                    send_message(&mut peer, opcode::UNCHOKE, None).await?; // this should be done on demand instead
                    send_message(&mut peer, opcode::INTEREST, None).await?;

                    while let Ok(pw) = piece_rx.try_recv() {
                        if !ps.bitfield[pw.index] {
                            piece_tx.send(pw)?;
                            continue;
                        }

                        let buf = match attempt_download_piece(&mut peer, &mut ps, &pw).await {
                            Ok(buf) => buf,
                            Err(error) => {
                                piece_tx.send(pw)?;
                                return Err(LocatedError::from(std::io::Error::new(std::io::ErrorKind::Other, error.to_string())));
                                
                            }
                        };
                       
                        // check integrity of the piece
                        let hash: [u8; 20] = Sha1::digest(&buf).into();
                        if hash != pw.hash {
                            piece_tx.send(pw)?;
                            panic!("hash does not match");
                        }

                        // notify peer that we have the piece
                        send_message(&mut peer, opcode::HAVE, Some((pw.index as u32).to_be_bytes().to_vec())).await?;
                        {
                            let mut file = file.lock().await;
                            file.seek(SeekFrom::Start(pw.index as u64)).await.unwrap();
                            file.write_all(&buf).await.unwrap();
                            let percent = (1.0 - (piece_rx.len() as f64 / piece_rx.capacity().unwrap() as f64)) * 100.0;
                            info!("({:.2}%) Downloaded piece #{:?} from {:?} peers", percent, pw.index, tx_task.receiver_count());
                        }
                    }

                    if !rx_task.has_changed()? {
                        tx_task.send_modify(|tasks| { tasks.remove(0); });
                    }
                }

                Task::SeedPieces => {
                    send_message(&mut peer, opcode::BITFIELD, Some(ps.bitfield.to_bytes())).await?;
                    send_message(&mut peer, opcode::UNCHOKE, None).await?;
                    let piece_len = state.borrow().piece_len.unwrap();
                    loop {
                        let msg = Message::read(&mut peer, &[opcode::REQUEST], &mut ps).await?;
                        let pr: PieceRequest = bincode::DefaultOptions::new()
                            .with_big_endian()
                            .with_fixint_encoding()
                            .deserialize(&msg.payload)?;
                        
                        let begin = (pr.index * piece_len as u32) + pr.requested;
                        let mut buf = vec![0u8; pr.len as usize];
                        {
                            let mut file = file.lock().await;
                            file.seek(SeekFrom::Start(begin as u64)).await?;
                            file.read_exact(&mut buf).await?;
                        }
                        send_message(&mut peer, opcode::PIECE, Some([
                            &pr.index.to_be_bytes(),
                            &pr.requested.to_be_bytes(),
                            buf.as_slice(),
                        ].concat())).await?;
                    }
                }
            }
        }

        Ok(())
    }
}

async fn attempt_download_piece(mut peer: impl AsyncRead + AsyncWrite + Unpin, ps: &mut PeerState, pw: &PieceWork) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut buf = vec![0u8; pw.len as usize];
    const MAX_BACKLOG: i32 = 5;
    const MAX_BLOCKSIZE: u32 = 16384;
    let mut downloaded = 0;
    let mut backlog = 0;
    let mut requested = 0;

    while downloaded < pw.len {
        if !ps.choked {
            // send requests untill we have enough unfulfilled requests (this should be done in send_message instead)
            while backlog < MAX_BACKLOG && requested < pw.len {
                // last block might be shorter than the max blocksize
                let blocksize = pw.len.min(requested + MAX_BLOCKSIZE) - requested;
                send_message(&mut peer, opcode::REQUEST, Some([
                    (pw.index as u32).to_be_bytes(),
                    requested.to_be_bytes(),
                    blocksize.to_be_bytes(),
                ].concat())).await?;

                backlog += 1;
                requested += blocksize;
            }
        }

        // read message
        let msg = Message::read(&mut peer, &[opcode::PIECE, opcode::CHOKE, opcode::UNCHOKE, opcode::HAVE], ps).await?;
        if msg.id == opcode::PIECE {
            let index = u32::from_be_bytes(msg.payload[..4].try_into()?);
            if index as usize != pw.index {
                return Err(format!("Expected index {}, got {}", index, pw.index).into());
            }
            let begin = u32::from_be_bytes(msg.payload[4..8].try_into()?) as usize;
            if begin > buf.len() {
                return Err("Begin offset to high".into());
            }
            let data = &msg.payload[8..];
            if begin + data.len() > buf.len() {
                return Err(format!("Data to long ({}) for offset {} with len {}", data.len(), begin, buf.len()).into());
            }
            buf[begin..begin+data.len()].clone_from_slice(data);
            downloaded += data.len() as u32;
            backlog -= 1;
        }
    }

    Ok(buf)
}

#[derive(Debug, Deserialize)]
struct BencodeTrackerResp {
    peers: ByteBuf,
}

// const PEER_ID: &[u8; 20] = b"wasmtorrent-12345678";

lazy_static! {
    /// This is a random id generated once at runtime
    static ref PEER_ID: [u8; 20] = {
        rand::Rng::sample_iter(rand::thread_rng(), &rand::distributions::Alphanumeric)
                .take(20)
                .collect::<Vec<u8>>().try_into().unwrap()
    };
}

mod opcode {
    pub const CHOKE: u8 = 0;
    pub const UNCHOKE: u8 = 1;
    pub const INTEREST: u8 = 2;
    pub const UNINTEREST: u8 = 3;
    pub const HAVE: u8 = 4;
    pub const BITFIELD: u8 = 5;
    pub const REQUEST: u8 = 6;
    pub const PIECE: u8 = 7;
    pub const CANCEL: u8 = 8;

    pub const EXTENDED: u8 = 20;
}

impl Message {
    async fn serialize<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> IoResult<()>
    {
        writer.write_u32((self.payload.len() + 1) as u32).await?;
        writer.write_u8(self.id).await?;
        writer.write_all(&self.payload).await?;

        Ok(())
    }

    async fn read<R: AsyncRead + Unpin>(reader: &mut R, listen: &[u8], state: &mut PeerState) -> IoResult<Self>
    {
        loop {
            let len = reader.read_u32().await?;
            if len == 0 {
                // keep-alive message
                continue
            }

            let mut buf = vec![0u8; len as usize];
            reader.read_exact(&mut buf).await?;
            let (&id, payload) = buf.split_first().unwrap();
            match id {
                opcode::CHOKE => state.choked = true,
                opcode::UNCHOKE => state.choked = false,
                opcode::BITFIELD => state.bitfield = BitVec::from_bytes(payload),
                _ => {}
            }
            if listen.contains(&id) {
                return Ok(Message { id, payload: payload.to_vec() })
            }
        }
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
            id: opcode::HAVE,
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
            id: opcode::UNCHOKE,
            payload: vec![],
        });
    }

}
