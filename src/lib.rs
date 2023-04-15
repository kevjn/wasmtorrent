#![feature(slice_as_chunks)]
#![feature(split_array)]
#![feature(int_roundings)]

extern crate serde;
pub extern crate serde_bencode;
#[macro_use]
extern crate serde_derive;
extern crate serde_bytes;

use bincode::Options;
use bit_vec::BitVec;
use futures::{StreamExt, stream::FuturesUnordered, FutureExt, Future};
use sha1::{Sha1, Digest};
use serde_bytes::ByteBuf;
use tokio::{io::{AsyncRead, AsyncWriteExt, AsyncReadExt, AsyncWrite, AsyncSeek, AsyncSeekExt}};
use std::{io::{SeekFrom}, net::{IpAddr, SocketAddr}, rc::Rc, collections::{HashMap}, cell::{RefCell}, error::Error, fmt::Debug, panic::Location, pin::Pin};
use tokio::sync::Mutex;
use tokio::io::{Result as IoResult};

use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use byteorder::{ByteOrder, BigEndian};
// use crossbeam_channel::{Sender, Receiver};

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

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct BencodeInfo {
    pub pieces: ByteBuf,
    #[serde(rename = "piece length")]
    piece_len: i64,
    #[serde(default)]
    pub length: Option<i64>,
    pub name: String,
    private: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BencodeTorrent {
    pub info: BencodeInfo,
    #[serde(default)]
    announce: Option<String>, // tracker url
}

// A flat structure for working with single torrent files
#[derive(Debug, Clone)]
pub struct Torrent {
    pub metadata: Rc<RefCell<Option<BencodeInfo>>>,
    pub announce: Option<String>,
    pub info_hash: [u8; 20],
    pub bitfield: BitVec,
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

#[derive(Debug)]
enum Download {
    Result(PieceResult),
    Work(PieceWork)
}

#[derive(Debug, Serialize, Deserialize)]
struct Handshake {
    len: u8,
    protocol: [u8; 19],
    reserved: [u8; 8],
    info_hash: [u8; 20],
    peer_id: [u8; 20],
}

#[derive(PartialEq)]
struct Message {
    id: u8,
    payload: Vec<u8>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id.to_opcode_name())
            .field("payload", &format!("<payload with {} bytes>", &self.payload.len()))
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ExtMsg {
    msg_type: i32,
    piece: i32,
    total_size: Option<i32>,
}

#[cfg(target_arch = "wasm32")]
pub mod wasm {
    use wasm_bindgen::{prelude::*, JsCast};
    use std::task::Poll;
    use crate::IoResult;

    // wrapper for websys datachannel
    pub struct DataStream {
        inner: web_sys::RtcDataChannel,
        rx_inbound: futures::channel::mpsc::Receiver<Vec<u8>>,
    }

    impl std::fmt::Debug for DataStream {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.debug_struct("DataStream")
                .field("peer", &self.inner.label())
                .finish()
        }
    }
    
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

    impl tokio::io::AsyncRead for DataStream {
        fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> Poll<IoResult<()>> {
            match futures::StreamExt::poll_next_unpin(&mut self.as_mut().rx_inbound, cx) {
                 Poll::Ready(Some(x)) => {
                    buf.put_slice(&x[..]);
                    Poll::Ready(Ok(()))
                 }
                 Poll::Ready(None) => Poll::Ready(Ok(())),
                 Poll::Pending => Poll::Pending,
            }
        }
    }
    
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
}

#[derive(Debug, Default)]
pub struct PeerState {
    bitfield: BitVec,
    choked: bool,
    metadata_size: Option<u32>,
    message_id: u8, // can be global state
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

#[derive(Debug, Deserialize)]
struct BencodeTrackerResp {
    peers: ByteBuf,
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
            metadata: Rc::new(RefCell::new(Some(torrent.info))),
            announce: torrent.announce,
            info_hash,
            bitfield,
        }
    }

    pub fn from_metadata(metadata: Vec<u8>) -> Self {
        let info: BencodeInfo = serde_bencode::from_bytes(&metadata).unwrap();

        // calculate sha1 hash for Torrent info
        let info_hash = Sha1::digest(metadata).into();

        // split pieces into slice of hashes where each slice is 20 bytes
        let pieces: &[[u8; 20]] = info.pieces.as_chunks().0;

        // initialize empty bitfield
        let bitfield = BitVec::from_elem(pieces.len(), false);

        Torrent {
            metadata: Rc::new(RefCell::new(Some(info))),
            announce: None,
            info_hash,
            bitfield,
        }
    }

    pub fn from_info_hash(bytes: [u8; 20])  -> Self {
        Torrent {
            metadata: Rc::new(RefCell::new(None)),
            announce: None,
            info_hash: bytes,
            bitfield: BitVec::default(),
        }
    }

    pub fn from_magnet_link(magnet: &str) -> Self {
        if let Some((_, magnet)) = magnet.split_once("xt=urn:btih:") {
            info!("parsing info hash: {:?}", magnet);
            let mut bytes = [0; 20];
            hex::decode_to_slice(magnet, &mut bytes).expect("Decoding failed");
            return Self::from_info_hash(bytes)
        }
        panic!("invalid magnet link")
    }

    pub fn from_file(filename: String, file: Vec<u8>) -> Self {
        const PIECE_LEN: usize = 262144;

        let pieces = file.chunks(PIECE_LEN).flat_map(|x| -> [u8;20] {
             Sha1::digest(x).into() 
        }).collect::<Vec<u8>>();

        let pieces = serde_bytes::ByteBuf::from(pieces);

        let torrent = BencodeTorrent {
            info: BencodeInfo { 
                pieces, 
                piece_len: PIECE_LEN as i64, 
                length: Some(file.len() as i64), 
                name: filename, 
                private: None 
            },
            announce: None
        };

        let bytes = serde_bencode::to_bytes(&torrent).unwrap();
        Self::from_torrent_file(bytes)
    }

    fn build_tracker_url(&self, peer_id: &[u8; 20], port: i64) -> String {
        let metadata = self.metadata.borrow();
        let torrent = metadata.as_ref().unwrap();

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

    pub fn build_magnet_link(&self) -> String {
        format!("xt=urn:btih:{}", hex::encode(self.info_hash))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub async fn request_peers(&self) -> FuturesUnordered<Pin<Box<dyn Future<Output = IoResult<(impl AsyncRead + AsyncWrite + Debug)>> + 'static>>> {

        let tracker_url = self.build_tracker_url(&PEER_ID, 8080);
        // announce our presence to the tracker, TODO: don't use unwrap here
        let bytes = reqwest::get(tracker_url).await.unwrap().bytes().await.unwrap();
        let response: BencodeTrackerResp = serde_bencode::from_bytes(&bytes).unwrap();

        (0..response.peers.len()).step_by(6).map(move |i| {
            let x = &response.peers[i..i + 6];
            let (&ip, port) = x.split_array_ref::<4>();
            let s = SocketAddr::new(IpAddr::from(ip), BigEndian::read_u16(port));
            tokio::net::TcpStream::connect(s).boxed_local()
        })
            .collect::<futures::stream::FuturesUnordered<_>>()
    }

    pub fn handshake_stream<'a, A: AsyncWrite + AsyncRead + Unpin + Debug + 'a>(
        self,
        potential_peers: impl futures::stream::Stream<Item = IoResult<A>> + Unpin + 'a,
    ) -> (impl futures::stream::Stream<Item = (PeerState, A)> + Unpin + 'a) {

        let mut connected_peers = FuturesUnordered::new();
        let mut potential_peers = potential_peers.fuse();

        async_stream::stream! {
            loop {
                tokio::select! {
                    res = potential_peers.next() => {
                        match res {
                            Some(Ok(peer)) => {
                                let b = self.metadata.borrow();
                                let metadata_size = b.as_ref().map(|x| serde_bencode::to_bytes(x).unwrap().len() as u32);
                                drop(b);
                                let handshake = Torrent::handshake(self.info_hash, peer, metadata_size);
                                connected_peers.push(handshake);
                            }
                            Some(Err(err)) => info!("failed to connect to peer with error: {:?}", err),
                            None => { break; }
                        }
                    }
                    Some(res) = connected_peers.next() => {
                        match res {
                            Ok(peer) => yield peer,
                            Err(err) => info!("failed to handshake peer with error: {:?}", err)
                        }
                    }
                }
            };
        }.boxed_local()
    }

    pub async fn handshake<A: AsyncWrite + AsyncRead + Unpin + Debug>(
        info_hash: [u8; 20],
        mut peer: A,
        metadata_size: Option<u32>,
    ) -> Result<(PeerState, A), Box<dyn std::error::Error>> {
        let req = Handshake {
            len: 19,
            protocol: b"BitTorrent protocol".to_owned(),
            reserved: [0, 0, 0, 0, 0, 0x10, 0, 0],
            info_hash: info_hash,
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
            metadata_size,
        };
        let mut bytes = serde_bencode::to_bytes(&req)?;
        bytes.insert(0, 0u8);
        send_message(&mut peer, opcode::EXTENDED, Some(bytes)).await?;

        let mut ps = PeerState::default();

        let msg = Message::read(&mut peer, &[opcode::EXTENDED], &mut ps).await?;
        let (&id, payload) = msg.payload.split_first().unwrap();

        if id != 0u8 {
            return Err("wrong id".into())
        }
        let res: ExtHandshake = serde_bencode::from_bytes(&payload)?;

        ps.metadata_size = res.metadata_size;
        ps.message_id = res.m["ut_metadata"] as u8;

        Ok((ps, peer))
    }

    pub async fn download_metadata<A: AsyncWrite + AsyncRead + Unpin + Debug + 'static>(
        &self,
        potential_peers: &mut (impl futures::stream::Stream<Item = (PeerState, A)> + Unpin),
        connected_peers: &mut Vec<Rc<RefCell<(PeerState, A)>>>,
    ) -> Vec<u8> {
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);

        let fun = |peer: Rc<RefCell<(PeerState, A)>>| -> Pin<Box<dyn Future<Output = _>>> {
            let result_tx = result_tx.clone();
            let info_hash = self.info_hash.clone();
            async move {
                let (ref mut state, ref mut peer) = *peer.borrow_mut();

                const METADATA_PIECE_LEN: usize = 16384;
                let metadata_size = state.metadata_size.ok_or("no metadata size set")? as usize;
                let mut metadata = vec![0u8; metadata_size];

                // request pieces
                let num_pieces = metadata_size.div_ceil(METADATA_PIECE_LEN);
                for i in 0..num_pieces {
                    let req = ExtMsg {
                        msg_type: 0, piece: i as i32, total_size: None
                    };
                    let mut bytes = serde_bencode::to_bytes(&req)?;
                    bytes.insert(0, state.message_id);
                    send_message(peer, opcode::EXTENDED, Some(bytes)).await?;

                    let msg = Message::read(peer, &[opcode::EXTENDED], state).await?;
                    let (_id, payload) = msg.payload.split_first().ok_or("Unable to split extension message")?;

                    let data: ExtMsg = serde_bencode::from_bytes(&payload)?;
                    let begin = (data.piece as usize) * METADATA_PIECE_LEN;
                    let size = (begin + METADATA_PIECE_LEN).min(metadata_size) - begin;

                    metadata[begin..begin+size].clone_from_slice(&msg.payload[msg.payload.len()-size..]);
                }

                if Sha1::digest(&metadata).as_slice() != info_hash {
                    return Err("invalid info hash".into())
                }

                result_tx.send(metadata).await?;

                Ok(())
            }.boxed_local()
        };

        let result_fun = async {
            result_rx.recv().await.unwrap()
        };

        self.start(potential_peers, connected_peers, fun, result_fun).await
    }

    pub async fn enqueue_pieces(&self, file: Option<Box<dyn AsyncRead + Unpin>>) -> (crossbeam_channel::Sender<PieceWork>, crossbeam_channel::Receiver<PieceWork>) {
        let b = self.metadata.borrow();
        let torrent: &BencodeInfo = b.as_ref().expect("Unable to enqueue pieces without metadata");
        let pieces: &[[u8; 20]] = torrent.pieces.as_chunks().0;
        let (piece_tx, piece_rx) = crossbeam_channel::bounded(pieces.len());

        let mut file_pieces = Vec::new();
        let mut file_pieces = if let Some(mut file) = file {
            file.read_to_end(&mut file_pieces).await.unwrap();
            Some(file_pieces.chunks(self.metadata.borrow().as_ref().unwrap().piece_len as usize))
        } else {
            None
        };

        for (i, &piece_hash) in pieces.iter().enumerate() {
            if let Some(file_piece) = file_pieces.as_mut().and_then(|x| x.next()) {
                let file_piece_hash: [u8; 20] = Sha1::digest(file_piece).into();
                if file_piece_hash == piece_hash {
                    // self.bitfield.set(i, true)
                    continue;
                }
            }
            let begin = (i as i64) * torrent.piece_len;
            let piece_len = (begin + torrent.piece_len).min(torrent.length.unwrap()) - begin;
            piece_tx.send(PieceWork {
                index: i,
                hash: piece_hash,
                len: piece_len as u32,
            }).unwrap();
        }
        debug!("filled work queue with {:?} pieces", pieces.len());
        (piece_tx, piece_rx)
    }

    pub async fn download_pieces<A: AsyncWrite + AsyncRead + Unpin + Debug + 'static, F: AsyncWrite + AsyncSeek + Unpin>(
        &self,
        potential_peers: &mut (impl futures::stream::Stream<Item = (PeerState, A)> + Unpin),
        connected_peers: &mut Vec<Rc<RefCell<(PeerState, A)>>>,
        file: &mut F,
        piece_queue: (crossbeam_channel::Sender<PieceWork>, crossbeam_channel::Receiver<PieceWork>)
    ) {
        let (piece_tx, piece_rx) = piece_queue;
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<Download>(piece_rx.len());

        let fun = |peer: Rc<RefCell<(PeerState, A)>>| -> Pin<Box<dyn Future<Output = _>>> {
            let piece_rx = piece_rx.clone();
            let result_tx = result_tx.clone();
            async move {
                let (ref mut state, ref mut peer) = *peer.borrow_mut();

                if state.bitfield.is_empty() {
                    Message::read(peer, &[opcode::BITFIELD], state).await?;
                }

                send_message(peer, opcode::UNCHOKE, None).await?;
                send_message(peer, opcode::INTEREST, None).await?;

                while let Ok(pw) = piece_rx.try_recv() {
                    if !state.bitfield[pw.index] {
                        result_tx.send(Download::Work(pw)).await?;
                        continue;
                    }

                    debug!("attempting to download piece {:?} from peer {:?} with state {:?}", pw, peer, state);
                    let buf = match attempt_download_piece(peer, state, &pw).await {
                        Ok(buf) => buf,
                        Err(error) => {
                            result_tx.send(Download::Work(pw)).await?;
                            return Err(error)
                        }
                    };
                    
                    // check integrity of the piece
                    let hash: [u8; 20] = Sha1::digest(&buf).into();
                    if hash != pw.hash {
                        result_tx.send(Download::Work(pw)).await?;
                        panic!("hash does not match")
                    }

                    // notify peer that we have the piece
                    send_message(peer, opcode::HAVE, Some((pw.index as u32).to_be_bytes().to_vec())).await?;
                    result_tx.send(Download::Result(PieceResult {
                        index: pw.index,
                        buf,
                    })).await.unwrap();
                }

                Ok(())
            }.boxed_local()
        };

        let piece_len = self.metadata.borrow().as_ref().unwrap().piece_len;

        let result_fun = async {
            let mut downloaded_pieces = 0;
            loop {
                let res = result_rx.recv().await.unwrap();
                match res {
                    Download::Result(pr) => {
                        let begin = (pr.index as i64) * piece_len;
                        file.seek(SeekFrom::Start(begin as u64)).await.unwrap();
                        file.write_all(&pr.buf).await.unwrap();
                        downloaded_pieces += 1;
                        let percent = (downloaded_pieces as f64 / piece_rx.capacity().unwrap() as f64) * 100.0;
                        info!("({:.2}%) Downloaded piece #{:?}", percent, pr.index);
                        if downloaded_pieces == piece_rx.capacity().unwrap() { 
                            break
                        }
                    }
                    Download::Work(pw) => {
                        piece_tx.send(pw).unwrap();
                    }
                };
            }
        };

        self.start(potential_peers, connected_peers, fun, result_fun).await;

    }

    async fn start<R, A: AsyncWrite + AsyncRead + Unpin + Debug>(
        &self, 
        potential_peers: &mut (impl futures::stream::Stream<Item = (PeerState, A)> + Unpin),
        connected_peers: &mut Vec<Rc<RefCell<(PeerState, A)>>>,
        work_fun: impl Fn(Rc<RefCell<(PeerState, A)>>) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>>>>,
        result_fun: impl Future<Output = R>
    ) -> R {
        info!("connected peers size: {}", connected_peers.len());

        let mut in_progress: FuturesUnordered<Pin<Box<dyn Future<Output = _>>>> = FuturesUnordered::new();

        for peer in std::mem::take(connected_peers) {
            connected_peers.push(peer.clone());
            in_progress.push(work_fun(peer));
        }

        tokio::pin!(result_fun);

        loop {
            info!("active peers: {}", in_progress.len());
            tokio::select! {
                Some(peer) = potential_peers.next() => {
                    let peer = Rc::new(RefCell::new(peer));
                    connected_peers.push(peer.clone());
                    in_progress.push(work_fun(peer));
                }
                Some(m) = in_progress.next() => {
                    match m {
                        Err(err) => error!("disconnected from peer with error: {:?}", err),
                        Ok(_) => info!("disconnected from peer successfully")
                    }
                }
                result = &mut result_fun => {
                    in_progress.clear();
                    return result
                }
                else => {
                    panic!("errors")
                }
            }
        }
    }

    async fn _seed_pieces<A: AsyncWrite + AsyncRead + Unpin + Debug, F: AsyncRead + AsyncSeek + Unpin>(
        state: &mut PeerState,
        peer: &mut A,
        file: Rc<Mutex<F>>,
        piece_len: i64,
        bitfield: &BitVec,
        metadata: Vec<u8>
    ) -> Result<(), Box<dyn Error>>{
        debug!("starting seed to peer {:?}", peer);

        send_message(peer, opcode::BITFIELD, Some(bitfield.to_bytes())).await?;
        send_message(peer, opcode::UNCHOKE, None).await?;

        const METADATA_PIECE_LEN: usize = 16384;

        loop {
            let msg = Message::read(peer, &[opcode::REQUEST, opcode::EXTENDED], state).await?;
            if msg.id == opcode::EXTENDED {
                let (_id, payload) = msg.payload.split_first().unwrap();
                let data: ExtMsg = serde_bencode::from_bytes(&payload).unwrap();
                if data.msg_type == 0 {
                    // request
                    let begin = (data.piece as usize) * METADATA_PIECE_LEN;
                    let end = (begin + METADATA_PIECE_LEN).min(metadata.len());
                    let mut piece = metadata[begin..end].to_vec();
                    let msg = ExtMsg { msg_type: 1, piece: data.piece, total_size: Some(metadata.len() as i32) };
                    let mut bytes = serde_bencode::to_bytes(&msg)?;
                    bytes.append(&mut piece);
                    bytes.insert(0, 0u8);
                    send_message(peer, opcode::EXTENDED, Some(bytes)).await?;
                    continue;
                }
            }

            let pr: PieceRequest = bincode::DefaultOptions::new()
                .with_big_endian()
                .with_fixint_encoding()
                .deserialize(&msg.payload)?;

            debug!("seeder received new piece request: {:?}", pr);
            
            let begin = (pr.index * piece_len as u32) + pr.requested;
            let mut buf = vec![0u8; pr.len as usize];
            {
                let mut file = file.lock().await;
                file.seek(SeekFrom::Start(begin as u64)).await?;
                file.read_exact(&mut buf).await?;
            }
            send_message(peer, opcode::PIECE, Some([
                &pr.index.to_be_bytes(),
                &pr.requested.to_be_bytes(),
                buf.as_slice(),
            ].concat())).await?;
        }
    }

    pub async fn seed_pieces<A: AsyncWrite + AsyncRead + Unpin + Debug, F: AsyncRead + AsyncSeek + Unpin>(
        &self, 
        peers: &mut (impl futures::stream::Stream<Item = (PeerState, A)> + Unpin),
        file: F
    ) {
        let mut in_progress: FuturesUnordered<Pin<Box<dyn Future<Output = _>>>> = FuturesUnordered::new();
        let piece_len = self.metadata.borrow().as_ref().unwrap().piece_len;
        let file = Rc::new(Mutex::new(file));

        let metadata = serde_bencode::to_bytes(&*self.metadata).unwrap();

        // TODO: use start instead
        // self.start(potential_peers, connected_peers, fun, result_fun).await;

        loop {
            tokio::select! {
                Some((mut state, mut peer)) = peers.next() => {
                    let file = file.clone();
                    let metadata = metadata.clone();
                    in_progress.push(async move {
                        Torrent::_seed_pieces(&mut state, &mut peer, file, piece_len, &self.bitfield, metadata).await
                    }.boxed_local())
                }
                Some(_) = in_progress.next() => {}
                else => {
                    panic!("errors")
                }
            }
        }
    }
}

async fn attempt_download_piece<A: AsyncRead + AsyncWrite + Unpin + Debug>(peer: &mut A, ps: &mut PeerState, pw: &PieceWork) -> Result<Vec<u8>, Box<dyn Error>> {
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
                send_message(peer, opcode::REQUEST, Some([
                    (pw.index as u32).to_be_bytes(),
                    requested.to_be_bytes(),
                    blocksize.to_be_bytes(),
                ].concat())).await?;

                backlog += 1;
                requested += blocksize;
            }
        }

        // read message
        let msg = Message::read(peer, &[opcode::PIECE, opcode::CHOKE, opcode::UNCHOKE, opcode::HAVE], ps).await?;
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

trait OpcodeName {
    fn to_opcode_name(&self) -> &'static str;
}

impl OpcodeName for u8 {
    fn to_opcode_name(&self) -> &'static str {
        match *self {
            opcode::CHOKE => "CHOKE",
            opcode::UNCHOKE => "UNCHOKE",
            opcode::INTEREST => "INTEREST",
            opcode::UNINTEREST => "UNINTEREST",
            opcode::HAVE => "HAVE",
            opcode::BITFIELD => "BITFIELD",
            opcode::REQUEST => "REQUEST",
            opcode::PIECE => "PIECE",
            opcode::CANCEL => "CANCEL",
            opcode::EXTENDED => "EXTENDED",
            _ => panic!("Unknown opcode: {}", self),
        }
    }
}

impl Message {
    async fn serialize<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> IoResult<()>
    {
        writer.write_u32((self.payload.len() + 1) as u32).await?;
        writer.write_u8(self.id).await?;
        writer.write_all(&self.payload).await?;

        Ok(())
    }

    async fn read<R: AsyncRead + Unpin + Debug>(reader: &mut R, listen: &[u8], state: &mut PeerState) -> IoResult<Self>
    {
        loop {
            let len = reader.read_u32().await?;
            if len == 0 {
                // keep-alive message
                continue
            }
            debug!("attempting to read message of size {:?} from peer {:?}", len, reader);
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
                let msg = Message { id, payload: payload.to_vec() };
                debug!("reading message: {:?} from peer {:?}", msg, reader);
                return Ok(msg)
            } else {
                debug!("ignoring {} message from {:?}", id.to_opcode_name(), reader);
            }
        }
    }
}

async fn send_message<W: AsyncWrite + Unpin + Debug>(peer: &mut W, id: u8, payload: Option<Vec<u8>>) -> IoResult<()> {
    let msg = Message {
        id,
        payload: payload.unwrap_or_default()
    };
    debug!("sending {:?} to peer {:?}", msg, peer);
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
