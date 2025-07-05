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

    // initialize torrent meta from file
    let args = std::env::args().collect::<Vec<String>>();
    let filename = args.get(1).expect("no torrent file specified");
    let bytes = tokio::fs::read(filename).await?;
    let torrent = Torrent::from_torrent_file(bytes);

    // output to file
    let meta = torrent.metadata.borrow();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&meta.as_ref().unwrap().name).await?;

    // request peers from tracker url
    let incoming = torrent.request_peers().await;
    let mut incoming = torrent.clone().handshake_stream(incoming);

    // enqueue all pieces
    let piece_queue = torrent.enqueue_pieces(None).await;

    // keep track of connected peers
    let mut connected = Vec::new();

    torrent.download_pieces(&mut incoming, &mut connected, &mut file, piece_queue).await;

    Ok(())
}