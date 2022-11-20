use std::io::Result as IoResult;
use tokio::fs::OpenOptions;
use wasmtorrent::Torrent;

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
    
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&torrent.metadata.as_ref().unwrap().name).await?;

    // let torrent = Torrent::from_info_hash(torrent.info_hash);
    
    let mut peers = torrent.request_peers().await;
    // let metadata = torrent.download_metadata(&mut peers).await;
    let (tx, rx) = torrent.enqueue_pieces(Some(&mut file)).await;
    torrent.download_pieces(&mut peers, &mut file, tx, rx).await;
    // torrent.seed_pieces(&mut peers, &mut file).await;

    Ok(())
}