use log::{debug, trace, info, Level};
use wasm_bindgen_test::*;
use wasm_bindgen::prelude::*;
use web_sys::{
    MessageEvent, RtcDataChannelEvent, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescriptionInit, RtcDataChannelInit
};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen::JsCast;
use js_sys::Reflect;

use tokio::io::{AsyncRead, AsyncWriteExt, AsyncReadExt, AsyncWrite, AsyncSeek, AsyncSeekExt};

use futures::StreamExt;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn test_download_single_peer() {
    let _ = console_log::init_with_level(Level::Debug);

    let (mut dc1, mut dc2) = setup_datachannels("test").await.unwrap();

    let filename = "test.txt".to_string();
    let upload = vec![0u8; 1000];
    let mut torrent = wasmtorrent::Torrent::from_file(filename, upload.clone());
    let magnet = torrent.build_magnet_link();
    torrent.bitfield.set_all();
    let upload_cursor = std::io::Cursor::new(upload.clone());

    let (mut tx1, mut rx1) = futures::channel::mpsc::channel::<wasmtorrent::wasm::DataStream>(32);
    let mut peers1 = torrent.clone().handshake_stream(
        async_stream::try_stream! {
            loop {
                yield rx1.select_next_some().await
            }
        }.boxed_local()
    );

    let mut torrent2 = wasmtorrent::Torrent::from_magnet_link(&magnet);
    let (mut tx2, mut rx2) = futures::channel::mpsc::channel::<wasmtorrent::wasm::DataStream>(32);
    let mut peers2 = torrent2.clone().handshake_stream(
        async_stream::try_stream! {
            loop {
                yield rx2.select_next_some().await
            }
        }.boxed_local()
    );

    tx1.try_send(dc1);
    tx2.try_send(dc2);

    let mut connected_peers = Vec::new();
    let downloaded_file: Option<Vec<u8>> = tokio::select! {
        res = async {
            let metadata = torrent2.download_metadata(&mut peers2, &mut connected_peers).await;
            let torr = wasmtorrent::Torrent::from_metadata(metadata);
            log::info!("downloaded metadata, {:?}", torr.metadata);
            let metadata = torr.metadata.borrow().clone();
            torrent2.metadata.replace(metadata);
            torrent2.bitfield = torr.bitfield;

            let mut output = std::io::Cursor::new(vec![0u8; torrent2.metadata.borrow().as_ref().unwrap().length.unwrap() as usize]);
            let pieces = torrent2.enqueue_pieces(None).await;
            torrent2.download_pieces(&mut peers2, &mut connected_peers, &mut output, pieces).await;
            output.into_inner()
        } => {
            Some(res)
        }
        _ = torrent.seed_pieces(&mut peers1, upload_cursor) => { None },
    };

    assert_eq!(torrent2.metadata, torrent.metadata);
    assert_eq!(torrent2.metadata.borrow().as_ref().map(|x| x.name.clone()), Some("test.txt".to_string()));
    assert_eq!(downloaded_file, Some(upload));
}

pub async fn sleep(ms: i32) -> Result<(), JsValue> {
    let promise = js_sys::Promise::new(&mut |yes, _| {
        let win = web_sys::window().unwrap();
        win.set_timeout_with_callback_and_timeout_and_arguments_0(&yes, ms)
            .unwrap();
    });
    let js_fut = JsFuture::from(promise);
    js_fut.await?;
    Ok(())
}

#[wasm_bindgen_test]
async fn test_download_multiple_peers() {
    let _ = console_log::init_with_level(Level::Debug);

    let filename = "test.txt".to_string();
    let upload = vec![1u8; 1000];
    let mut torrent = wasmtorrent::Torrent::from_file(filename, upload.clone());
    let magnet = torrent.build_magnet_link();
    torrent.bitfield.set_all();
    let upload_cursor = std::io::Cursor::new(upload.clone());

    let (mut tx1, mut rx1) = futures::channel::mpsc::channel::<wasmtorrent::wasm::DataStream>(32);
    let mut peers1 = torrent.clone().handshake_stream(
        async_stream::try_stream! {
            loop {
                yield rx1.select_next_some().await
            }
        }.boxed_local()
    );

    let mut torrent2 = wasmtorrent::Torrent::from_magnet_link(&magnet);
    let (mut tx2, mut rx2) = futures::channel::mpsc::channel::<wasmtorrent::wasm::DataStream>(32);
    let mut peers2 = torrent2.clone().handshake_stream(
        async_stream::try_stream! {
            loop {
                yield rx2.select_next_some().await
            }
        }.boxed_local()
    );

    let (mut dc1, mut dc2) = setup_datachannels("my-data-channel-1").await.unwrap();
    let (mut dc3, mut dc4) = setup_datachannels("my-data-channel-2").await.unwrap();

    tx1.try_send(dc1);
    tx1.try_send(dc3);

    tx2.try_send(dc2);
    tx2.try_send(dc4);

    let mut connected_peers = Vec::new();
    let downloaded_file: Option<Vec<u8>> = tokio::select! {
        res = async {
            let metadata = torrent2.download_metadata(&mut peers2, &mut connected_peers).await;
            let torr = wasmtorrent::Torrent::from_metadata(metadata);
            log::info!("downloaded metadata, {:?}", torr.metadata);
            let metadata = torr.metadata.borrow().clone();
            torrent2.metadata.replace(metadata);
            torrent2.bitfield = torr.bitfield;

            let mut output = std::io::Cursor::new(vec![0u8; torrent2.metadata.borrow().as_ref().unwrap().length.unwrap() as usize]);
            let pieces = torrent2.enqueue_pieces(None).await;
            torrent2.download_pieces(&mut peers2, &mut connected_peers, &mut output, pieces).await;
            output.into_inner()
        } => {
            Some(res)
        }
        _ = torrent.seed_pieces(&mut peers1, upload_cursor) => { None },
        _ = sleep(3000) => { None }
    };

    assert_eq!(torrent2.metadata, torrent.metadata);
    assert_eq!(torrent2.metadata.borrow().as_ref().map(|x| x.name.clone()), Some("test.txt".to_string()));
    assert_eq!(downloaded_file, Some(upload));
}

async fn setup_datachannels(datachannel_name: &str) -> Result<(wasmtorrent::wasm::DataStream, wasmtorrent::wasm::DataStream), JsValue> {
    let pc1 = RtcPeerConnection::new()?;
    trace!("pc1 created: state {:?}", pc1.signaling_state());
    let pc2 = RtcPeerConnection::new()?;
    trace!("pc2 created: state {:?}", pc2.signaling_state());

    let dc1 = pc1.create_data_channel(datachannel_name);
    trace!("dc1 created: label {:?}", dc1.label());

    let (mut tx, mut rx) = futures::channel::mpsc::channel::<web_sys::RtcDataChannel>(1);

    let ondatachannel_callback = Closure::<dyn FnMut(_)>::new(move |ev: RtcDataChannelEvent| {
        let dc2 = ev.channel();
        trace!("pc2.ondatachannel!: {:?}", dc2.label());

        let mut tx = tx.clone();

        let dc2_clone = dc2.clone();
        let onopen_callback = Closure::<dyn FnMut()>::new(move || {
            tx.try_send(dc2_clone.clone()).unwrap();
        });
        dc2.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();
    });
    pc2.set_ondatachannel(Some(ondatachannel_callback.as_ref().unchecked_ref()));
    ondatachannel_callback.forget();

    // Handle ICE candidate each other
    let pc2_clone = pc2.clone();
    let onicecandidate_callback1 =
        Closure::<dyn FnMut(_)>::new(move |ev: RtcPeerConnectionIceEvent| match ev.candidate() {
            Some(candidate) => {
                trace!("pc1.onicecandidate: {:#?}", candidate.candidate());
                let _ = pc2_clone.add_ice_candidate_with_opt_rtc_ice_candidate(Some(&candidate));
            }
            None => {}
        });
    pc1.set_onicecandidate(Some(onicecandidate_callback1.as_ref().unchecked_ref()));
    onicecandidate_callback1.forget();

    let pc1_clone = pc1.clone();
    let onicecandidate_callback2 =
        Closure::<dyn FnMut(_)>::new(move |ev: RtcPeerConnectionIceEvent| match ev.candidate() {
            Some(candidate) => {
                trace!("pc2.onicecandidate: {:#?}", candidate.candidate());
                let _ = pc1_clone.add_ice_candidate_with_opt_rtc_ice_candidate(Some(&candidate));
            }
            None => {}
        });
    pc2.set_onicecandidate(Some(onicecandidate_callback2.as_ref().unchecked_ref()));
    onicecandidate_callback2.forget();

    // Send OFFER from pc1 to pc2
    let offer = JsFuture::from(pc1.create_offer()).await?;
    let offer_sdp = Reflect::get(&offer, &JsValue::from_str("sdp"))?
        .as_string()
        .unwrap();
    trace!("pc1: offer {:?}", offer_sdp);

    let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    offer_obj.sdp(&offer_sdp);
    let _ = JsFuture::from(pc1.set_local_description(&offer_obj)).await?;
    trace!("pc1: state {:?}", pc1.signaling_state());

    // Receive OFFER from pc1
    // Create and send ANSWER from pc2 to pc1
    let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    offer_obj.sdp(&offer_sdp);
    let _ = JsFuture::from(pc2.set_remote_description(&offer_obj)).await?;
    trace!("pc2: state {:?}", pc2.signaling_state());

    let answer = JsFuture::from(pc2.create_answer()).await?;
    let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))?
        .as_string()
        .unwrap();
    trace!("pc2: answer {:?}", answer_sdp);

    let mut answer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    answer_obj.sdp(&answer_sdp);
    let _ = JsFuture::from(pc2.set_local_description(&answer_obj)).await?;
    trace!("pc2: state {:?}", pc2.signaling_state());

    // Receive ANSWER from pc2
    let mut answer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    answer_obj.sdp(&answer_sdp);
    let _ = JsFuture::from(pc1.set_remote_description(&answer_obj)).await?;
    trace!("pc1: state {:?}", pc1.signaling_state());

    let dc2 = rx.select_next_some().await;

    Ok((wasmtorrent::wasm::DataStream::new(dc1), wasmtorrent::wasm::DataStream::new(dc2)))
}

