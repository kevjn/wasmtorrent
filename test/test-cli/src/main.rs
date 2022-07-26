use std::{path::PathBuf, net::SocketAddr};

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

fn main() {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_line_number(true)
        .without_time()
        .init();

    let bytes = std::fs::read(args.torrentfile).unwrap();
    let torrent = serde_bencode::from_bytes::<torrentclient::Torrent>(&bytes).unwrap();
    
    // the peer we want to connect to
    let peers = args.seeder.unwrap();

    let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&torrent.name)
            .unwrap();

    torrent.download(&mut file, &peers).unwrap();
}
