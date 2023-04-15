use std::io::Result as IoResult;
use tokio::fs::OpenOptions;
use wasmtorrent::Torrent;

#[tokio::main]
async fn main() -> IoResult<()> {
    tracing_subscriber::fmt()
        .with_line_number(true)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = tokio::fs::read(filename).await?;

    // from file
    let torrent = Torrent::from_torrent_file(bytes);
    
    let meta = torrent.metadata.borrow();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&meta.as_ref().unwrap().name).await?;

    let mut peers = torrent.clone().handshake_stream(torrent.request_peers().await);
    let piece_queue = torrent.enqueue_pieces(None).await;

    let mut connected_peers = Vec::new();
    torrent.download_pieces(&mut peers, &mut connected_peers, &mut file, piece_queue).await;

    Ok(())
}