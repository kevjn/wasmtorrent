#![feature(slice_as_chunks)]
#![feature(array_chunks)]

extern crate serde;
extern crate serde_bencode;
#[macro_use]
extern crate serde_derive;
extern crate serde_bytes;

use bincode::{DefaultOptions, Options};
use bit_vec::BitVec;
use sha1::{Sha1, Digest};
use serde_bencode::de;
use serde_bytes::ByteBuf;
use std::{time::Duration, net::{TcpStream, IpAddr, SocketAddr}, sync::{Arc, Mutex}};
use std::io::{Read, Write, Error as IoError, ErrorKind, Seek, SeekFrom, Result as IoResult};

use reqwest::Url;
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use rand::Rng;
use byteorder::{ByteOrder, BigEndian, WriteBytesExt, ReadBytesExt};
use crossbeam_channel::{bounded, Sender, Receiver};
use num_enum::{TryFromPrimitive, IntoPrimitive};

#[macro_use]
extern crate lazy_static;

#[macro_use] 
extern crate log;

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
    pieces: Box<[[u8; 20]]>,
    piece_length: i64,
    length: i64,
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
        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        let info_hash = hasher.finalize().into();

        // split pieces into slice of hashes where each slice is 20 bytes
        let pieces: &[[u8; 20]] = torrent.info.pieces.as_chunks().0;

        Ok(Torrent {
            announce: torrent.announce.unwrap(),
            info_hash,
            pieces: pieces.into(),
            piece_length: torrent.info.piece_length,
            length: torrent.info.length.unwrap(),
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
            ("left", &self.length.to_string()),
            ("peer_id", &peer_id.to_string()),
            ("port", &port.to_string()),
            ("uploaded", "0"),
        ])));
        url.to_string()
    }

    fn download<W: Write + Seek>(self, writer: &mut W) -> IoResult<()> {
        info!("Starting download for {}", self.name);

        // create a multi-producer, multi-consumer queue with specified capacity
        let (tx, rx) = bounded::<PieceWork>(self.pieces.len());

        // fill work queue
        for (i, piece) in self.pieces.iter().enumerate() {
            let begin = (i as i64) * self.piece_length;
            let end = std::cmp::min(begin + self.piece_length, self.length);
            let piece_size = end - begin;

            tx.send(PieceWork {
                index: i,
                hash: *piece,
                length: piece_size as u32,
            }).unwrap();
        }

        // store the result in a multi-producer, single-consumer queue
        let (tx_result, rx_result) = std::sync::mpsc::channel::<PieceResult>();
        
        // identifies the file we want to download
        let tracker_url = self.build_tracker_url(&RANDOM_ID, 6882);

        let bytes = reqwest::blocking::get(tracker_url).unwrap().bytes().unwrap();
        let response: BencodeTrackerResp = de::from_bytes(&bytes).unwrap();

        // start workers
        let num_peers = Arc::new(Mutex::new(0));

        response.peers.array_chunks().for_each(|x: &[u8; 6]| {
            let ip: [u8; 4] = x[0..4].try_into().unwrap();
            let peer = SocketAddr::new(IpAddr::from(ip), BigEndian::read_u16(&x[4..]));

            let tx = tx.clone();
            let tx_result= tx_result.clone();
            let rx = rx.clone();
            let num_peers = num_peers.clone();
            *num_peers.lock().unwrap() += 1;
            std::thread::spawn(move || {
                match start_download_worker(peer, tx, rx, tx_result, &self.info_hash) {
                    Ok(()) => info!("success"),
                    Err(error) => {
                        *num_peers.lock().unwrap() -= 1;
                        info!("Disconnecting from {:?} with error ({:?})", peer, error);
                    }
                }
            });
        });

        // collect results
        let mut done_pieces = 0;
        while done_pieces < self.pieces.len() {
            let res = rx_result.recv().unwrap();
            let begin = (res.index as i64) * self.piece_length;
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
    pstr: String,           // protocol identifier
    reserved: [u8; 8],      // 8 reserved bytes
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
            pstr: "BitTorrent protocol".to_string(), 
            reserved: [0u8; 8],
            info_hash: *info_hash, 
            peer_id: *RANDOM_ID,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, TryFromPrimitive, IntoPrimitive)] #[repr(u8)]
enum MessageId {
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have,
    Bitfield,       // bitfield represents the pieces that a peer has
    Request,
    Piece,
    Cancel,
}

#[derive(Debug, PartialEq)]
struct Message {
    id: MessageId,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct ReadMessageError {
    message: String,
}

impl std::error::Error for ReadMessageError {}

impl std::fmt::Display for ReadMessageError {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
     write!(f, "{}", self.message)
  }
}

impl From<IoError> for ReadMessageError {
    fn from(error: IoError) -> Self {
        ReadMessageError {
            message: error.to_string(),
        }
    }
}

impl Message {
    fn serialize<W: Write>(&self, writer: &mut W) -> IoResult<()>
    {
        writer.write_u32::<BigEndian>((self.payload.len() + 1) as u32)?;
        writer.write_u8(self.id.into())?;
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
                id: buf[0].try_into().unwrap(),
                payload: buf[1..].to_vec(),
            })
        )
    }

    fn format_request(index: u32, begin: u32, length: u32) -> Message {
        Message {
            id: MessageId::Request,
            payload: [
                index.to_be_bytes(),
                begin.to_be_bytes(),
                length.to_be_bytes(),
            ].concat()
        }
    }

}

struct Client {
    conn: TcpStream, // contains connection to peer
    choked: bool,
    bitfield: BitVec,
}

impl Client {
    fn new(peer: SocketAddr, info_hash: &[u8; 20]) -> Result<Client, Box<dyn std::error::Error>> {
        // create tcp conn with peer
        let mut conn = TcpStream::connect_timeout(&peer, Duration::from_secs(3))?;

        // use u8 as length encoding
        let opt = DefaultOptions::new()
            .with_varint_encoding();

        // send handshake
        let req = Handshake::new(info_hash);
        opt.serialize_into(&conn, &req)?;

        // recieve handshake
        let res: Handshake = opt.deserialize_from(&conn)?;

        // verify infohash
        if res.info_hash != *info_hash {
            return Err(Box::new(IoError::new(ErrorKind::InvalidData, "invalid info hash")))
        }

        // recieve bitfield
        let req = Message::read(&mut conn)?.unwrap();
        if req.id != MessageId::Bitfield {
            let msg = format!("Expected bitfield but got {:?}", req.id);
            return Err(Box::new(IoError::new(ErrorKind::InvalidData, msg)))
        }

        Ok(Client {
            conn,
            choked: true,
            bitfield: BitVec::from_bytes(&req.payload),
        })

    }

    fn send_unchoke(&mut self) {
        let msg = Message {
            id: MessageId::Unchoke,
            payload: vec![],
        };

        msg.serialize(&mut self.conn).unwrap();
    }

    fn send_interested(&mut self) {
        let msg = Message {
            id: MessageId::Interested,
            payload: vec![],
        };

        msg.serialize(&mut self.conn).unwrap();
    }

    fn send_request(&mut self, index: usize, begin: u32, length: u32) -> Result<(), std::io::Error>{
        let msg = Message::format_request(index as u32, begin, length);
        msg.serialize(&mut self.conn)?;
        Ok(())
    }

}

fn start_download_worker(
    peer: SocketAddr, 
    tx: Sender<PieceWork>, 
    rx: Receiver<PieceWork>, 
    tx_result: std::sync::mpsc::Sender<PieceResult>, 
    info_hash: &[u8; 20]
) -> Result<(), Box<dyn std::error::Error>> {

    let mut client = Client::new(peer, info_hash)?;

    client.send_unchoke();
    client.send_interested();

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
                warn!("Failed to download piece {:?}, Exiting", error);
                return Err(error);
            }
        };

        // check integrity of the piece
        let mut hasher = Sha1::new();
        hasher.update(&buf);
        let hash: [u8; 20] = hasher.finalize().into();
        if hash != pw.hash {
            tx.send(pw).unwrap();
            warn!("hashes do not match");
            continue;
        }

        // notify peer that we have the piece
        Message {
            id: MessageId::Have,
            payload: (pw.index as u32).to_be_bytes().to_vec(),
        }.serialize(&mut client.conn).unwrap();
        
        // add piece to result
        tx_result.send(PieceResult {
            buf,
            index: pw.index,
        }).unwrap();
    }
    Ok(())

}

fn attempt_download_piece(client: &mut Client, pw: &PieceWork) -> Result<Vec<u8>, Box<dyn std::error::Error>> {

    // set deadline for reading/writing to tcp connection to 30 seconds
    client.conn.set_read_timeout(Some(Duration::from_secs(30)))?;
    client.conn.set_write_timeout(Some(Duration::from_secs(30)))?;

    let max_backlog = 5;
    let max_blocksize = 16384;

    let mut downloaded = 0;
    let mut backlog = 0;
    let mut requested = 0;

    let mut buf =  vec![0u8; pw.length as usize]; 

    while downloaded < pw.length {
        if !client.choked {
            // if unchoked, send requests untill we have enough unfulfilled requests
            while backlog < max_backlog && requested < pw.length {
                let mut blocksize = max_blocksize;

                // Last block might be shorter than the typical block
                if pw.length - requested < blocksize {
                    blocksize = pw.length - requested
                }

                client.send_request(pw.index, requested, blocksize)?;

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
            MessageId::Choke => { 
                client.choked = true; 
            },
            MessageId::Unchoke => { 
                client.choked = false; 
            },
            MessageId::Have => {
                let index = u32::from_be_bytes(msg.payload.try_into().unwrap());
                client.bitfield.set(index as usize, true);
            },
            MessageId::Piece => {
                let index = u32::from_be_bytes(msg.payload[..4].try_into().unwrap());
                // compare both index
                if index as usize != pw.index {
                    return Err(Box::new(IoError::new(ErrorKind::InvalidData, "Wrong index")));
                }

                let begin = u32::from_be_bytes(msg.payload[4..8].try_into().unwrap()) as usize;
                if begin > buf.len() {
                    return Err(Box::new(IoError::new(ErrorKind::InvalidData, "Begin offset to high")));
                }
                let data = &msg.payload[8..];
                if begin + data.len() > buf.len() {
                    return Err(Box::new(IoError::new(ErrorKind::InvalidData, "Data too long")));
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

    // TODO: read from stdin
    let mut f = std::fs::File::open("./testdata/debian-11.4.0-amd64-netinst.iso.torrent")?;
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer)?;

    let torrent = de::from_bytes::<Torrent>(&buffer).unwrap();

    let mut f = std::fs::File::create("foo.txt")?;
    torrent.download(&mut f).unwrap();

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
            let begin = (i as i64) * torrent.piece_length;
            let end = std::cmp::min(begin + torrent.piece_length, torrent.length);
            let piece_size = end - begin;

            tx.send(PieceWork {
                index: i,
                hash: *piece,
                length: piece_size as u32,
            }).unwrap();
        }

        // store the result in a multi-producer, single-consumer queue
        let (tx_result, _) = std::sync::mpsc::channel::<PieceResult>();

        match start_download_worker(peer, tx, rx, tx_result, &torrent.info_hash) {
            Ok(()) => info!("success"),
            Err(error) => info!("Disconnecting from {:?} with error ({:?})", peer, error),
        }

    }

    #[test]
    fn test_build_tracker_url() {
        let to = Torrent {
            announce: "http://bttracker.debian.org:6969/announce".to_string(),
            info_hash: [216, 247, 57, 206, 195, 40, 149, 108, 204, 91, 191, 31, 134, 217, 253, 207, 219, 168, 206, 182],
            pieces: Box::new([
                [49, 50, 51, 52, 53, 54, 55, 56, 57, 48, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106],
                [97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 49, 50, 51, 52, 53, 54, 55, 56, 57, 48]
            ]),
            piece_length: 262144,
            length: 351272960,
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
            pstr: "BitTorrent protocol".to_string(),
            reserved: [0, 0, 0, 0, 0, 0, 0, 0],
            info_hash: [134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116],
            peer_id: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]
        };

        let encoded = DefaultOptions::new()
            .with_varint_encoding()
            .serialize(&handshake)
            .unwrap();

        assert_eq!(encoded, [19, 66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99, 111, 108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]);
    }

    #[test]
    fn test_format_request() {
        let msg = Message::format_request(4, 567, 4321);
        assert_eq!(msg, Message {
            id: MessageId::Request,
            payload: vec![
                0x00, 0x00, 0x00, 0x04, // Index
                0x00, 0x00, 0x02, 0x37, // Begin
			    0x00, 0x00, 0x10, 0xe1, // Length
            ]
        });
    }

    #[test]
    fn test_parse_message() {
        // Have Message
        let mut input: &[u8] = &[0, 0, 0, 5, 4, 1, 2, 3, 4];
        let msg = Message::read(&mut input).unwrap();
        assert_eq!(msg, Some(Message {
            id: MessageId::Have,
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
            id: MessageId::Unchoke,
            payload: vec![],
        });
    }

}