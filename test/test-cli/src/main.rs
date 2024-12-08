use futures::StreamExt;
use std::io::Result as IoResult;
use std::{net::SocketAddr, path::PathBuf};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// The path to the torrent metainfo file.
    #[clap(short, long)]
    torrentfile: PathBuf,

    /// A comma separated list of <ip>:<port> pairs of the seeds.
    #[clap(short, long)]
    seeder: Option<Vec<SocketAddr>>,
}

#[macro_use]
extern crate log;

use crossbeam_channel::bounded;

#[tokio::main]
async fn main() -> IoResult<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_line_number(true)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let bytes = std::fs::read(args.torrentfile).unwrap();
    let torrent = wasmtorrent::Torrent::from_torrent_file(bytes);

    let piece_queue = torrent.enqueue_pieces(None).await;

    let mut peers = args
        .seeder
        .unwrap()
        .into_iter()
        .map(tokio::net::TcpStream::connect)
        .collect::<futures::stream::FuturesUnordered<_>>();

    let mut stream = torrent.clone().handshake_stream(peers);

    let meta = torrent.metadata.borrow();
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&meta.as_ref().unwrap().name)
        .await?;

    let mut connected_peers = vec![];

    torrent
        .download_pieces(&mut stream, &mut connected_peers, &mut file, piece_queue)
        .await;

    Ok(())
}
