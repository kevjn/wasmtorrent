#![feature(type_alias_impl_trait)]
#![feature(slice_as_chunks)]

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use futures::StreamExt;
use wasmtorrent::Torrent;
use std::pin::Pin;

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
pub async fn seed(filename: String, upload: Vec<u8>, socket: web_sys::EventTarget) {
    let mut torrent = wasmtorrent::Torrent::from_file(filename, upload.clone());

    let window = web_sys::window().expect("global window does not exists");    
	let document = window.document().expect("expecting a document on window");
    let url = document.get_element_by_id("torrent-url").unwrap().dyn_into::<web_sys::HtmlInputElement>().unwrap();
    let location = window.location();
    let magnet = format!("{}&{}", location.href().unwrap(), torrent.build_magnet_link());
    url.set_value(&magnet);

    let upload = std::io::Cursor::new(upload);
    let (mut tx, mut rx) = futures::channel::mpsc::channel(32);
    let onpeer_callback = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        tx.try_send(ev.detail().dyn_into::<web_sys::RtcDataChannel>().unwrap()).unwrap()
    });
    socket.add_event_listener_with_callback("newPeer", onpeer_callback.as_ref().unchecked_ref()).unwrap();
    onpeer_callback.forget();

    torrent.bitfield.set_all();
    let peers = async_stream::try_stream! {
        loop {
            let peer = rx.select_next_some().await;
            yield wasmtorrent::wasm::DataStream::new(peer);
        }
    }.boxed_local();

    let mut peers = torrent.clone().handshake_stream(peers);
    torrent.seed_pieces(&mut peers, upload).await;
}

#[wasm_bindgen]
pub async fn seed_with_metainfo(metainfo: Vec<u8>, upload: Vec<u8>, socket: web_sys::EventTarget) {
    let mut torrent = wasmtorrent::Torrent::from_torrent_file(metainfo);

    let window = web_sys::window().expect("global window does not exists");    
	let document = window.document().expect("expecting a document on window");
    let url = document.get_element_by_id("torrent-url").unwrap().dyn_into::<web_sys::HtmlInputElement>().unwrap();
    let location = window.location();
    let magnet = format!("{}&{}", location.href().unwrap(), torrent.build_magnet_link());
    url.set_value(&magnet);

    let upload = std::io::Cursor::new(upload);
    let (mut tx, mut rx) = futures::channel::mpsc::channel(32);
    let onpeer_callback = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::CustomEvent| {
        tx.try_send(ev.detail().dyn_into::<web_sys::RtcDataChannel>().unwrap()).unwrap()
    });
    socket.add_event_listener_with_callback("newPeer", onpeer_callback.as_ref().unchecked_ref()).unwrap();
    onpeer_callback.forget();

    torrent.bitfield.set_all();
    let peers = async_stream::try_stream! {
        loop {
            let peer = rx.select_next_some().await;
            yield wasmtorrent::wasm::DataStream::new(peer);
        }
    }.boxed_local();

    let mut peers = torrent.clone().handshake_stream(peers);
    torrent.seed_pieces(&mut peers, upload).await;
}

#[wasm_bindgen]
pub async fn leech(magnet: String, socket: web_sys::EventTarget) {
    let mut torrent = wasmtorrent::Torrent::from_magnet_link(&magnet);
    log::info!("parsed magnet link {:?}", torrent);

    let (mut tx, mut rx) = futures::channel::mpsc::channel(32);
    let onpeer_callback = Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
        tx.try_send(ev.detail().dyn_into::<web_sys::RtcDataChannel>()).unwrap()
    });
    socket.add_event_listener_with_callback("newPeer", onpeer_callback.as_ref().unchecked_ref()).unwrap();
    onpeer_callback.forget();

    let info_hash = torrent.info_hash.clone();
    let metadata = torrent.metadata.borrow();
    let metadata_size = metadata.as_ref().map(|x| wasmtorrent::serde_bencode::to_bytes(x).unwrap().len() as u32);
    drop(metadata);

    let peers_stream: Pin<Box<dyn futures::stream::Stream<Item = std::io::Result<_>>>> = async_stream::try_stream! {
        loop {
            let peer = rx.select_next_some().await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Unable to convert {:?}", e)))?;
            yield wasmtorrent::wasm::DataStream::new(peer);
        }
    }.boxed_local();

    let mut peers = torrent.clone().handshake_stream(peers_stream);

    let mut connected_peers = Vec::new();
    let metadata = torrent.download_metadata(&mut peers, &mut connected_peers).await;

    let torr = wasmtorrent::Torrent::from_metadata(metadata);
    log::info!("downloaded metadata, {:?}", torr.metadata);
    let metadata = torr.metadata.borrow().clone();
    torrent.metadata.replace(metadata);
    torrent.bitfield = torr.bitfield;

    let mut output = std::io::Cursor::new(vec![0u8; torrent.metadata.borrow().as_ref().unwrap().length.unwrap() as usize]);
    let pieces = torrent.enqueue_pieces(None).await;
    torrent.download_pieces(&mut peers, &mut connected_peers, &mut output, pieces).await;
    let metadata = torrent.metadata.borrow();

    log::info!("downloaded file {:?}", metadata.as_ref().unwrap().name);
    download_file(output.get_ref(), &metadata.as_ref().unwrap().name);

    log::info!("seeding file {:?}", metadata.as_ref().unwrap().name);
    torrent.bitfield.set_all();
    torrent.seed_pieces(&mut peers, &mut output).await;
}