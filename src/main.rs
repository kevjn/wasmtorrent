#![feature(slice_as_chunks)]
#![feature(array_chunks)]
#![feature(iterator_try_collect)]
#![feature(split_array)]

extern crate serde;
extern crate serde_bencode;
#[macro_use]
extern crate serde_derive;
extern crate serde_bytes;

use bit_vec::BitVec;
use sha1::{Sha1, Digest};
use serde_bencode::de;
use serde_bytes::ByteBuf;
use std::{time::Duration, net::{TcpStream, IpAddr, SocketAddr}, sync::{Arc, Mutex}};
use std::io::{Read, Write, Seek, SeekFrom, Result as IoResult};

use reqwest::Url;
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use rand::Rng;
use byteorder::{ByteOrder, BigEndian, WriteBytesExt, ReadBytesExt};
use crossbeam_channel::{bounded, Sender, Receiver};

#[macro_use]
extern crate lazy_static;

#[macro_use] 
extern crate log;

// https://github.com/rust-webplatform/rust-todomvc/blob/51cbd62e906a6274d951fd7a8f5a6c33fcf8e7ea/src/main.rs#L34-L41
macro_rules! enclose {
    ( ($( $x:ident ),*) $y:expr ) => {
        {
            $(let $x = $x.clone();)*
            $y
        }
    };
}

#[derive(Debug, Deserialize, Serialize)]
struct BencodeInfo {
    pieces: ByteBuf,
    #[serde(rename = "piece length")]
    piece_length: i64,
    #[serde(default)]
    length: Option<i64>,
    name: String,
}

#[derive(Debug, Deserialize)]
struct BencodeTorrent {
    info: BencodeInfo,
    #[serde(default)]
    announce: Option<String>,
}

// A flat structure for working with single torrent files
#[derive(Debug)]
struct Torrent {
    announce: String,
    info_hash: [u8; 20],
    pieces: Vec<[u8; 20]>,
    piece_len: i64,
    file_len: i64,
    name: String,
}

impl<'de> serde::Deserialize<'de> for Torrent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
        {
        let torrent: BencodeTorrent = serde::Deserialize::deserialize(deserializer)?;

        // calculate sha1 hash for Torrent info
        let bytes = serde_bencode::ser::to_bytes(&torrent.info).unwrap();
        let info_hash = Sha1::digest(bytes).into();

        // split pieces into slice of hashes where each slice is 20 bytes
        let pieces: &[[u8; 20]] = torrent.info.pieces.as_chunks().0;

        Ok(Torrent {
            announce: torrent.announce.unwrap(),
            info_hash,
            pieces: pieces.into(),
            piece_len: torrent.info.piece_length,
            file_len: torrent.info.length.unwrap(),
            name: torrent.info.name,
        })
    }
}

fn build_query_string<'a, I>(pairs: I) -> String 
where I: IntoIterator<Item = (&'a str, &'a str)> {
    use std::fmt::Write as _;

    let mut query = String::new();
    let mut iter = pairs.into_iter();
    if let Some((first_key, first_value)) = iter.next() {
        let _ = write!(query, "{}={}", first_key, first_value);
        iter.for_each(|(key, value)| {
            let _ = write!(query, "&{}={}", key, value);
        });
    }
    query
}

impl Torrent {
    fn build_tracker_url(&self, peer_id: &[u8; 20], port: i64) -> String {

        // represent binary data as url-encoded strings
        let info_hash = percent_encode(&self.info_hash, NON_ALPHANUMERIC);
        let peer_id = percent_encode(peer_id, NON_ALPHANUMERIC);

        let mut url = Url::parse(&self.announce).unwrap();
        url.set_query(Some(&build_query_string([
            ("compact", "1"),
            ("downloaded", "0"),
            ("info_hash", &info_hash.to_string()),
            ("left", &self.file_len.to_string()),
            ("peer_id", &peer_id.to_string()),
            ("port", &port.to_string()),
            ("uploaded", "0"),
        ])));
        url.to_string()
    }

    fn start<W: Write + Seek>(self, writer: &mut W) -> IoResult<()> {
        info!("Starting download for {}", self.name);

        // create a multi-producer, multi-consumer queue with specified capacity
        let (tx, rx) = bounded::<PieceWork>(self.pieces.len());

        // fill work queue
        for (i, piece) in self.pieces.iter().enumerate() {
            let begin = (i as i64) * self.piece_len;
            let piecesize = (begin+self.piece_len).min(self.file_len) - begin;

            tx.send(PieceWork {
                index: i,
                hash: *piece,
                length: piecesize as u32,
            }).unwrap();
        }

        // store the result in a multi-producer, single-consumer queue
        let (tx_result, rx_result) = std::sync::mpsc::channel::<PieceResult>();
        
        // identifies the file we want to download
        let tracker_url = self.build_tracker_url(&RANDOM_ID, 6882);
        // announce our presence to the tracker
        let bytes = reqwest::blocking::get(tracker_url).unwrap().bytes().unwrap();
        let response: BencodeTrackerResp = de::from_bytes(&bytes).unwrap();

        // start workers
        let num_peers = Arc::new(Mutex::new(0));

        response.peers.array_chunks().for_each(|x: &[u8; 6]| {
            let (ip, port) = x.split_array_ref::<4>();
            let peer = SocketAddr::new(IpAddr::from(*ip), BigEndian::read_u16(port));

            *num_peers.lock().unwrap() += 1;
            std::thread::spawn(enclose! { (tx, rx, tx_result, num_peers) move || {
                match spawn_connector_task(peer, tx, rx, tx_result, &self.info_hash) {
                    Ok(()) => info!("success"),
                    Err(error) => {
                        *num_peers.lock().unwrap() -= 1;
                        info!("Disconnecting from {:?} with error ({:?})", peer, error);
                    }
                }
            } });
        });

        // collect download results
        let mut done_pieces = 0;
        while done_pieces < self.pieces.len() {
            let res = rx_result.recv().unwrap();
            let begin = (res.index as i64) * self.piece_len;
            writer.seek(SeekFrom::Start(begin as u64))?;
            writer.write_all(&res.buf)?;
            done_pieces += 1;
            let percent = (done_pieces as f64 / self.pieces.len() as f64) * 100.0;
            info!("({:.3}%) Downloaded piece #{:?} from {:?} peers", percent, res.index, *num_peers.lock().unwrap());
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PieceWork {
    index: usize,
    hash: [u8; 20],
    length: u32,
}

struct PieceResult {
    index: usize,
    buf: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct BencodeTrackerResp {
    peers: ByteBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Handshake {
    len: u8,
    protocol: [u8; 19],
    reserved: [u8; 8],
    info_hash: [u8; 20],
    peer_id: [u8; 20],
}

lazy_static! {
    /// This is an random id generated once at runtime
    static ref RANDOM_ID: [u8; 20] = {
        rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
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
            peer_id: *RANDOM_ID,
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

#[derive(Debug, PartialEq)]
struct Message {
    id: u8,
    payload: Vec<u8>,
}

impl Message {
    fn serialize<W: Write>(&self, writer: &mut W) -> IoResult<()>
    {
        writer.write_u32::<BigEndian>((self.payload.len() + 1) as u32)?;
        writer.write_u8(self.id)?;
        writer.write_all(&self.payload)?;

        Ok(())
    }

    fn read<R: Read>(reader: &mut R) -> IoResult<Option<Self>>
    {
        let len = reader.read_u32::<BigEndian>()?;

        // keep-alive message
        if len == 0 {
            return Ok(None);
        }

        let mut buf = vec![0u8; len as usize];
        reader.read_exact(&mut buf)?;

        Ok(
            Some(Message {
                id: buf[0],
                payload: buf[1..].to_vec(),
            })
        )
    }
}

struct Client {
    conn: TcpStream, // contains connection to peer
    choked: bool,
    bitfield: BitVec,
}

impl Client {
    fn new(peer: SocketAddr, info_hash: &[u8; 20]) -> bincode::Result<Client> {
        // create tcp connection with peer
        let mut conn = TcpStream::connect_timeout(&peer, Duration::from_secs(3))?;

        // send handshake
        let req = Handshake::new(info_hash);
        bincode::serialize_into(&conn, &req)?;

        // recieve handshake
        let res: Handshake = bincode::deserialize_from(&conn)?;

        // verify infohash
        if res.info_hash != *info_hash {
            return Err(bincode::ErrorKind::Custom("invalid info hash".to_string()).into());
        }

        // recieve bitfield
        let res = Message::read(&mut conn)?.unwrap();
        if res.id != btid::BITFIELD {
            let error_message = format!("Expected bitfield but got {:?}", res.id);
            return Err(bincode::ErrorKind::Custom(error_message).into());
        }

        Ok(Client {
            conn,
            choked: true,
            bitfield: BitVec::from_bytes(&res.payload),
        })

    }

    fn send_message(&mut self, id: u8, payload: Option<Vec<u8>>) -> IoResult<()> {
        Message {
            id,
            payload: payload.unwrap_or_default()
        }.serialize(&mut self.conn)
    }

}

fn spawn_connector_task(
    peer: SocketAddr, 
    tx: Sender<PieceWork>, 
    rx: Receiver<PieceWork>, 
    tx_result: std::sync::mpsc::Sender<PieceResult>, 
    info_hash: &[u8; 20]
) -> Result<(), Box<dyn std::error::Error>> {

    let mut client = Client::new(peer, info_hash)?;

    client.send_message(btid::UNCHOKE, None)?;
    client.send_message(btid::INTEREST, None)?;

    for pw in rx {
        if !client.bitfield[pw.index] {
            tx.send(pw).unwrap(); // put piece back on the queue
            continue;
        }

        // Download the piece
        let buf = match attempt_download_piece(&mut client, &pw) {
            Ok(buf) => buf,
            Err(error) => {
                tx.send(pw).unwrap();
                error!("Failed to download piece {:?}, Exiting", error);
                return Err(error);
            }
        };
        
        // check integrity of the piece
        let hash: [u8; 20] = Sha1::digest(&buf).into();
        if hash != pw.hash {
            tx.send(pw).unwrap();
            error!("hashes do not match");
            continue;
        }

        // notify peer that we have the piece
        client.send_message(btid::HAVE, Some((pw.index as u32).to_be_bytes().to_vec()))?;
        
        // add piece to result
        tx_result.send(PieceResult {
            buf,
            index: pw.index,
        })?;
    }
    Ok(())

}

fn attempt_download_piece(client: &mut Client, pw: &PieceWork) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const MAX_BACKLOG: i32 = 5;
    const MAX_BLOCKSIZE: u32 = 16384;

    let mut downloaded = 0;
    let mut backlog = 0;
    let mut requested = 0;

    let mut buf =  vec![0u8; pw.length as usize]; 

    while downloaded < pw.length {
        if !client.choked {
            // if unchoked, send requests untill we have enough unfulfilled requests
            while backlog < MAX_BACKLOG && requested < pw.length {
                // Last block might be shorter than the max blocksize
                let blocksize = pw.length.min(requested+MAX_BLOCKSIZE) - requested;

                client.send_message(btid::REQUEST, Some([
                    (pw.index as u32).to_be_bytes(), 
                    requested.to_be_bytes(), 
                    blocksize.to_be_bytes()
                ].concat()))?;

                backlog += 1;
                requested += blocksize;
            }
        }

        // read message
        let msg = Message::read(&mut client.conn)?;
        if msg == None {
            // keep-alive
            continue;
        }
        let msg = msg.unwrap();
        match msg.id {
            btid::CHOKE => { 
                client.choked = true; 
            },
            btid::UNCHOKE => { 
                client.choked = false; 
            },
            btid::HAVE => {
                let index = u32::from_be_bytes(msg.payload.try_into().unwrap());
                client.bitfield.set(index as usize, true);
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
                    return Err("Data too long".into());
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

fn main() -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_line_number(true)
        .without_time()
        .init();

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = std::fs::read(filename)?;

    let torrent = de::from_bytes::<Torrent>(&bytes).unwrap();
    let mut f = std::fs::File::create(&torrent.name)?;
    torrent.start(&mut f)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    #[ignore]
    fn test_download_localhost() {
        tracing_subscriber::fmt()
            .with_thread_ids(true)
            .with_line_number(true)
            .without_time()
            .init();

        let bytes = std::fs::read("./testdata/debian-11.4.0-amd64-netinst.iso.torrent").unwrap();
        let torrent = de::from_bytes::<Torrent>(&bytes).unwrap();
        
        // the peer we want to connect to
        let peer = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)), 31916);

        // fill work queue
        let (tx, rx) = bounded::<PieceWork>(torrent.pieces.len());
        for (i, piece) in torrent.pieces.iter().enumerate() {
            let begin = (i as i64) * torrent.piece_len;
            let end = std::cmp::min(begin + torrent.piece_len, torrent.file_len);
            let piece_size = end - begin;

            tx.send(PieceWork {
                index: i,
                hash: *piece,
                length: piece_size as u32,
            }).unwrap();
        }

        // store the result in a multi-producer, single-consumer queue
        let (tx_result, rx_result) = std::sync::mpsc::channel::<PieceResult>();

        match spawn_connector_task(peer, tx, rx, tx_result, &torrent.info_hash) {
            Ok(()) => info!("success"),
            Err(error) => info!("Disconnecting from {:?} with error ({:?})", peer, error),
        }

    }

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

        let response: BencodeTrackerResp = de::from_bytes(str.as_bytes()).unwrap();

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