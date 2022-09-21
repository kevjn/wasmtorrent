use std::io::{Write, Seek};
use wasm_bindgen::{prelude::*, JsCast};
struct SourceBuffer(web_sys::SourceBuffer);
use std::task::Poll;
use tokio::{io::{AsyncRead, AsyncWriteExt, AsyncReadExt, AsyncWrite, AsyncSeek, AsyncSeekExt}, sync::mpsc};
use futures::StreamExt;

impl tokio::io::AsyncWrite for SourceBuffer {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let arr_buf = js_sys::Uint8Array::from(buf).buffer();
        if self.0.updating() {
            log::warn!("not ready to append buffer!");
            return Poll::Pending;
        }
        match self.0.append_buffer_with_array_buffer(&arr_buf) {
            Ok(()) => {
                Poll::Ready(Ok(buf.len()))
            }
            Err(e) => {
                log::error!("failed to append with error {:?}", e);
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e))))
            }
        }
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncSeek for SourceBuffer {
    fn start_seek(self: std::pin::Pin<&mut Self>, position: tokio::io::SeekFrom) -> std::io::Result<()> {
        Ok(())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>
    ) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(2))
    }
}

#[wasm_bindgen(start)]
pub fn run() -> Result<(), JsValue> {
    console_log::init_with_level(log::Level::Info).unwrap();
    Ok(())
}

#[wasm_bindgen]
pub async fn seed(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>, upload: Vec<u8>, socket: web_sys::EventTarget) {
    let torrent = wasmtorrent::Torrent::from(metainfo.to_vec());
    let upload = std::io::Cursor::new(upload);

    // currently connected peers
    let peers = futures::stream::iter(peers.into_iter().map(|peer| futures::future::ok(wasmtorrent::DataStream::new(peer))));

    torrent.seed_to_connections(upload, peers).await;
}

#[wasm_bindgen]
pub async fn leech(metainfo: Vec<u8>, peers: Vec<web_sys::RtcDataChannel>, source_buf: web_sys::SourceBuffer, socket: web_sys::EventTarget) {
    let torrent = wasmtorrent::Torrent::from(metainfo);

    // currently connected peers
    let peers = futures::stream::iter(peers.into_iter().map(|peer| futures::future::ok(wasmtorrent::DataStream::new(peer))));

    torrent.download(&mut SourceBuffer(source_buf), peers).await.unwrap();


}