#![feature(type_alias_impl_trait)]

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use futures::StreamExt;

#[wasm_bindgen(start)]
pub fn run() -> Result<(), JsValue> {
    console_log::init_with_level(log::Level::Debug).unwrap();
    Ok(())
}

#[wasm_bindgen(module = "/aux.js")]
extern {
    pub fn download_file(data: &[u8], name: &str);
}

#[wasm_bindgen]
pub async fn seed(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>, upload: Vec<u8>, socket: web_sys::EventTarget) {
    let torrent = wasmtorrent::Torrent::from(metainfo);
    let upload = std::io::Cursor::new(upload);
    let peers = futures::stream::iter(peers.into_iter().map(|peer| futures::future::ok(wasmtorrent::DataStream::new(peer))));

    let (mut tx, rx) = futures::channel::mpsc::channel(32);
    let onpeer = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        log::info!("new event: {:?}", ev);
        let peer = match ev.detail().dyn_into::<web_sys::RtcDataChannel>() {
            Ok(peer) => {
                futures::future::ok::<wasmtorrent::DataStream, std::io::Error>(wasmtorrent::DataStream::new(peer))
            }
            Err(_) => {
                panic!("error reading from RtcDataChannel");
            }
        };
        if let Err(e) = tx.try_send(peer) {
            panic!("Error sending via channel: {:?}", e);
        }
    });
    socket.add_event_listener_with_callback("newPeer", onpeer.as_ref().unchecked_ref()).unwrap();
    onpeer.forget();

    let peers = peers.chain(rx);

    torrent.seed_to_connections(upload, rx).await;
}

#[wasm_bindgen]
pub async fn leech(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>, socket: web_sys::EventTarget) {
    let torrent = wasmtorrent::Torrent::from(metainfo);
    let mut output = std::io::Cursor::new(vec![0u8; torrent.file_len as usize]);

    // currently connected peers
    let peers = futures::stream::iter(peers.into_iter().map(|peer| futures::future::ok(wasmtorrent::DataStream::new(peer))));

    let (mut tx, rx) = futures::channel::mpsc::channel(32);
    let onpeer = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        let peer = match ev.detail().dyn_into::<web_sys::RtcDataChannel>() {
            Ok(peer) => {
                futures::future::ok::<wasmtorrent::DataStream, std::io::Error>(wasmtorrent::DataStream::new(peer))
            }
            Err(_) => {
                panic!("error reading from RtcDataChannel");
            }
        };
        if let Err(e) = tx.try_send(peer) {
            panic!("Error sending via channel: {:?}", e);
        }
    });
    socket.add_event_listener_with_callback("newPeer", onpeer.as_ref().unchecked_ref()).unwrap();
    onpeer.forget();

    let peers = peers.chain(rx);

    let filename = torrent.name.clone();
    torrent.download(&mut output, rx).await.unwrap();
    download_file(output.get_ref(), &filename);
}