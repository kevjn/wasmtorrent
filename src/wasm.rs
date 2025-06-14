use std::sync::Arc;
use std::task::{Poll, Waker};
use tokio::sync::Mutex;
use wasm_bindgen::{prelude::*, JsCast};

use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::IoResult;

extern crate console_error_panic_hook;

#[wasm_bindgen(start)]
pub fn run() -> Result<(), JsValue> {
    console_log::init_with_level(log::Level::Debug).unwrap();
    std::panic::set_hook(Box::new(console_error_panic_hook::hook));
    Ok(())
}

struct State {
    // ring buffer for holding buffered bytes
    buf: Vec<u8>,
    waker: Option<Waker>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            waker: None,
        }
    }
}

#[derive(Clone)]
pub struct DataStream {
    inner: web_sys::EventTarget,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for DataStream {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("DataStream")
            .field("peer", &"<some-ip>")
            .finish()
    }
}

impl DataStream {
    pub fn new(inner: web_sys::EventTarget) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let state_cloned = state.clone();

        let onmessage = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::MessageEvent| {
            let mut data = ev
                .data()
                .dyn_into::<js_sys::ArrayBuffer>()
                .map(|buf| js_sys::Uint8Array::new(&buf).to_vec())
                .expect("Failed to read ArrayBuffer from MessageEvent");

            let mut state = state_cloned.blocking_lock();
            state.buf.append(&mut data);
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        });
        inner
            .add_event_listener_with_callback("message", onmessage.as_ref().unchecked_ref())
            .expect("Failed to add event listener");
        onmessage.forget();

        Self { inner, state }
    }
}

trait Write {
    fn write(&self, data: &[u8]) -> Result<(), JsValue>;
}

impl Write for web_sys::RtcDataChannel {
    fn write(&self, data: &[u8]) -> Result<(), JsValue> {
        self.send_with_u8_array(data)?;
        Ok(())
    }
}

impl Write for web_sys::EventTarget {
    fn write(&self, data: &[u8]) -> Result<(), JsValue> {
        let js_array = js_sys::Uint8Array::from(data);

        let event = web_sys::MessageEvent::new_with_event_init_dict(
            "sendMessage",
            &web_sys::MessageEventInit::new().data(&js_array),
        )?;

        self.dispatch_event(&event)?;
        Ok(())
    }
}

impl AsyncWrite for DataStream {
    fn poll_write(
        self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>, buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let inner = &self.as_ref().inner;

        // check the type of `inner` and call the appropriate `write` method
        let result = if let Some(rtc_channel) = inner.dyn_ref::<web_sys::RtcDataChannel>() {
            rtc_channel.write(buf)
        } else {
            inner.write(buf)
        };

        Poll::Ready(result.map(|_| buf.len()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Error writing data: {:?}", e),
            )
        }))
    }

    fn poll_flush(
        self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

use futures::FutureExt; // TODO remove this

impl AsyncRead for DataStream {
    fn poll_read(
        self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        match Box::pin(self.state.lock()).poll_unpin(cx) {
            Poll::Ready(mut state) => {
                state.waker = Some(cx.waker().clone());

                let dsize = state.buf.len();
                let bsize = buf.remaining();

                if dsize < bsize {
                    return Poll::Pending;
                }

                let data = state.buf.drain(0..bsize);
                buf.put_slice(data.as_slice());
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

use futures::SinkExt;

// concrete types for wasm
#[derive(Clone)]
#[wasm_bindgen]
pub struct Faucet {
    inner: crate::Faucet<DataStream>,
}

#[wasm_bindgen]
impl Faucet {
    #[wasm_bindgen(method)]
    pub async fn send(&mut self, event_target: web_sys::EventTarget) {
        let data_stream = DataStream::new(event_target);
        self.inner.sender.send(Ok(data_stream)).await.unwrap();
    }
}

#[wasm_bindgen]
pub struct Sink {
    inner: crate::Sink<DataStream>,
}

#[wasm_bindgen]
pub struct ConnectionPool {
    #[wasm_bindgen(getter_with_clone)]
    pub faucet: Faucet,
    sink: Option<Sink>,
}

#[wasm_bindgen]
impl ConnectionPool {
    #[wasm_bindgen(getter)]
    pub fn sink(&mut self) -> Sink {
        self.sink.take().expect("Sink already taken")
    }
}

impl From<crate::ConnectionPool<DataStream>> for ConnectionPool {
    fn from(value: crate::ConnectionPool<DataStream>) -> Self {
        ConnectionPool {
            faucet: Faucet {
                inner: value.faucet,
            },
            sink: Some(Sink { inner: value.sink }),
        }
    }
}

mod videostream {
    use crate::wasm::Poll;

    pub struct SourceBuffer(pub web_sys::SourceBuffer);

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
            Poll::Ready(Ok(0))
        }
    }
}

#[wasm_bindgen]
struct Torrent {
    inner: crate::Torrent,
}

#[wasm_bindgen]
impl Torrent {
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> Option<String> {
        if let Some(info) = self.inner.metadata.borrow().as_ref() {
            return Some(info.name.clone())
        }
        return None
    }

    #[wasm_bindgen(static_method_of=Torrent)]
    pub fn from_magnet_link(magnet: &str) -> Self {
        Self {
            inner: crate::Torrent::from_magnet_link(magnet),
        }
    }

    #[wasm_bindgen(static_method_of=Torrent)]
    pub fn from_file(filename: String, file: Vec<u8>) -> Self {
        Self {
            inner: crate::Torrent::from_file(filename, file),
        }
    }

    pub fn from_metadata(metadata: Vec<u8>) -> Self {
        Self {
            inner: crate::Torrent::from_metadata(metadata),
        }
    }

    #[wasm_bindgen(method)]
    pub fn build_magnet_link(&self) -> String {
        self.inner.build_magnet_link()
    }

    #[wasm_bindgen(method)]
    pub fn connection_pool(&self) -> ConnectionPool {
        crate::ConnectionPool::new(&self.inner).into()
    }

    #[wasm_bindgen(method)]
    pub async fn download_metadata(&self, sink: &mut Sink) -> Vec<u8> {
        return self
            .inner
            .download_metadata(&mut sink.inner.incoming, &mut sink.inner.connected)
            .await;
    }

    #[wasm_bindgen(method)]
    pub async fn download_pieces(&self, sink: &mut Sink) -> Vec<u8> {
        let mut output = std::io::Cursor::new(vec![
            0u8;
            self.inner
                .metadata
                .borrow()
                .as_ref()
                .unwrap()
                .length
                .unwrap() as usize
        ]);
        let pieces = self.inner.enqueue_pieces(None).await;
        self.inner
            .download_pieces(
                &mut sink.inner.incoming,
                &mut sink.inner.connected,
                &mut output,
                pieces,
            )
            .await;
        output.into_inner()
    }

    #[wasm_bindgen(method)]
    pub async fn download_pieces_to_source_buf(&self, sink: &mut Sink, source_buf: &web_sys::SourceBuffer) {
        let pieces = self.inner.enqueue_pieces(None).await;
        self.inner
            .download_pieces(
                &mut sink.inner.incoming,
                &mut sink.inner.connected,
                &mut videostream::SourceBuffer(source_buf.clone()),
                pieces,
            )
            .await;
    }

    #[wasm_bindgen(method)]
    pub async fn seed_pieces(&mut self, stream: &mut Sink, bytes: Vec<u8>) {
        self.inner.bitfield.set_all();
        let upload = std::io::Cursor::new(bytes);
        self.inner
            .seed_pieces(&mut stream.inner.incoming, upload)
            .await
    }
}
