use wasm_bindgen::{prelude::*, JsCast};

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
pub async fn seed(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>, upload: Vec<u8>) {
    let torrent = wasmtorrent::Torrent::from(metainfo.to_vec());
    let upload = std::io::Cursor::new(upload);
    torrent.seed_to_connections(upload, peers).await;
}

#[wasm_bindgen]
pub async fn leech(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>) {
    let torrent = wasmtorrent::Torrent::from(metainfo);
    let mut output = std::io::Cursor::new(vec![0u8; torrent.file_len as usize]);
    let filename = torrent.name.clone();
    torrent.download(&mut output, peers).await.unwrap();
    download_file(output.get_ref(), &filename);
}