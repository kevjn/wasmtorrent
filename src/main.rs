use std::{io::{Result as IoResult}, fs::OpenOptions};

fn main() -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_line_number(true)
        .without_time()
        .init();

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = std::fs::read(filename)?;
    let torrent = serde_bencode::de::from_bytes::<torrentclient::Torrent>(&bytes).unwrap();

    let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&torrent.name)?;

    let peers: Vec<std::net::SocketAddr> = torrent.request_peers()?;
    torrent.download(&mut file, &peers)?;

    Ok(())
}