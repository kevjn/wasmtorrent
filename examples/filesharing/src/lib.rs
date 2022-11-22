#![feature(type_alias_impl_trait)]

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use futures::StreamExt;
use futures::FutureExt;
use futures::TryFutureExt;

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
pub async fn seed(metainfo: Vec<u8>, upload: Vec<u8>, socket: web_sys::EventTarget) {
    let mut torrent = wasmtorrent::Torrent::from_torrent_file(metainfo);
    let mut upload = std::io::Cursor::new(upload);

    let (mut tx, mut rx) = futures::channel::mpsc::channel(32);
    let onpeer_callback = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        tx.try_send(ev.detail().dyn_into::<web_sys::RtcDataChannel>().unwrap()).unwrap()
    });
    socket.add_event_listener_with_callback("newPeer", onpeer_callback.as_ref().unchecked_ref()).unwrap();
    onpeer_callback.forget();

    torrent.bitfield.set_all();
    let info_hash = &torrent.info_hash;
    let mut peers = async_stream::stream! {
        loop {
            let peer = rx.select_next_some().await;
            yield wasmtorrent::Torrent::handshake(info_hash, wasmtorrent::wasm::DataStream::new(peer))
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, format!("handshake failed with error: {:?}", err))).await;
        }
    }.boxed_local();

    torrent.seed_pieces(&mut peers, upload).await;
}

#[wasm_bindgen]
pub async fn leech(metainfo: Vec<u8>, socket: web_sys::EventTarget) {
    let torrent = wasmtorrent::Torrent::from_torrent_file(metainfo);
    let mut output = std::io::Cursor::new(vec![0u8; torrent.metadata.as_ref().unwrap().length.unwrap() as usize]);

    let (mut tx, mut rx) = futures::channel::mpsc::channel(32);
    let onpeer_callback = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        tx.try_send(ev.detail().dyn_into::<web_sys::RtcDataChannel>().unwrap()).unwrap()
    });
    socket.add_event_listener_with_callback("newPeer", onpeer_callback.as_ref().unchecked_ref()).unwrap();
    onpeer_callback.forget();

    let info_hash = &torrent.info_hash;
    let mut peers = async_stream::stream! {
        loop {
            let peer = rx.select_next_some().await;
            yield wasmtorrent::Torrent::handshake(&info_hash, wasmtorrent::wasm::DataStream::new(peer))
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, format!("handshake failed with error: {:?}", err))).await;
        }
    }.boxed_local();

    let (tx, rx) = torrent.enqueue_pieces(None).await;
    torrent.download_pieces(&mut peers, &mut output, tx, rx).await;
    download_file(output.get_ref(), &torrent.metadata.as_ref().unwrap().name);
}