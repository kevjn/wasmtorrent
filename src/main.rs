use std::{io::{Result as IoResult}, fs::OpenOptions, net::ToSocketAddrs};

use futures::{stream::{self, Stream}, StreamExt};
use wasmtorrent::Torrent;

#[tokio::main]
async fn main() -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_line_number(true)
        .without_time()
        .init();

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = tokio::fs::read(filename).await?;
    let mut torrent = Torrent::from(bytes);
    
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&torrent.name)?;
    
    let peers = torrent.request_peers().await;

    torrent.init_state(&mut file)?;

    torrent.download(&mut file, peers).await?;

    // torrent.seed(file, "0.0.0.0:8080".parse().unwrap())?;

    Ok(())
}