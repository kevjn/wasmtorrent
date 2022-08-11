use std::{io::{Result as IoResult}, fs::OpenOptions, net::ToSocketAddrs};

use wasmtorrent::Torrent;

fn main() -> IoResult<()> {

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = std::fs::read(filename)?;
    let mut torrent = Torrent::from(bytes);

    let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&torrent.name)?;

    let peers: Vec<std::net::SocketAddr> = torrent.request_peers()?;

    torrent.init_state(&mut file)?;

    // torrent.download(&mut file, peers)?;

    torrent.seed(file, "0.0.0.0:8080".parse().unwrap())?;

    Ok(())
}