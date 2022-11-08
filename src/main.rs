use std::io::Result as IoResult;
use tokio::fs::OpenOptions;
use wasmtorrent::{Torrent, Task};

#[tokio::main]
async fn main() -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_line_number(true)
        .without_time()
        .init();

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = tokio::fs::read(filename).await?;

    // from file
    let torrent = Torrent::from_torrent_file(bytes);
    
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&torrent.metadata.as_ref().unwrap().name).await?;
    
    let peers = torrent.request_peers().await;
    torrent.start(peers, &[Task::EnqueuePieces, Task::DownloadPieces], file).await;

    Ok(())
}