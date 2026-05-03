//! WebRTC Data Channel support for browser clients.
//!
//! Uses str0m (sans-I/O WebRTC) with ICE Lite for direct connections.
//! All WebRTC peers share a single UDP socket via UdpMux, routed by STUN ufrag.
//! SDP signaling via `POST /api/webrtc/offer`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::mpsc;

use saw_core::crypto::auth::AuthKey;
use saw_core::protocol::control::Control;

use crate::session_mgr::SharedSessionManager;
use crate::transport::{
    ControlTransport, WS_TYPE_CONTROL, WS_TYPE_TERM_INPUT, WebrtcControlTransport, WsMessage,
};
use crate::udp_mux::UdpMux;

/// Initialize the WebRTC crypto provider. Must be called once at startup.
pub fn init_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}

/// Extract the ICE ufrag from an SDP answer string.
/// Looks for `a=ice-ufrag:` line.
fn extract_ufrag_from_sdp(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        if let Some(ufrag) = line.strip_prefix("a=ice-ufrag:") {
            return Some(ufrag.trim().to_string());
        }
    }
    None
}

// ── Handle offer ────────────────────────────────────────────────────────────

/// Handle a WebRTC SDP offer and return the SDP answer.
/// Uses the shared UdpMux for packet routing.
pub async fn handle_offer(
    offer_sdp: &str,
    public_ip: Option<std::net::IpAddr>,
    mux: &Arc<UdpMux>,
    session_mgr: SharedSessionManager,
    auth_key: Option<AuthKey>,
) -> Result<String> {
    // Create Rtc with ICE Lite
    let mut rtc = str0m::Rtc::builder()
        .set_ice_lite(true)
        .build(Instant::now());

    // Add ICE host candidate pointing to the shared UDP socket
    let candidate_ip = public_ip.ok_or_else(|| anyhow::anyhow!("No candidate IP available"))?;
    let mux_addr = mux.listen_addr();
    let candidate_addr: SocketAddr = format!("{}:{}", candidate_ip, mux_addr.port()).parse()?;
    let candidate = str0m::Candidate::host(candidate_addr, "udp")
        .map_err(|e| anyhow::anyhow!("Invalid host candidate: {}", e))?;
    rtc.add_local_candidate(candidate);

    // Accept SDP offer
    let offer = str0m::change::SdpOffer::from_sdp_string(offer_sdp)?;
    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_sdp = answer.to_sdp_string();

    // Extract server ufrag from the answer SDP for mux routing
    let local_ufrag = extract_ufrag_from_sdp(&answer_sdp).unwrap_or_else(|| "unknown".to_string());

    log::info!(
        "WebRTC: accepted offer, candidate {}:{} ufrag={}",
        candidate_ip,
        mux_addr.port(),
        local_ufrag
    );

    // Register with the mux to receive packets
    let pkt_rx = mux.register(&local_ufrag);

    // Spawn event loop
    let mux = mux.clone();
    tokio::spawn(run_rtc_event_loop(
        rtc,
        mux,
        candidate_addr,
        local_ufrag,
        pkt_rx,
        session_mgr,
        auth_key,
    ));

    Ok(answer_sdp)
}

// ── RTC event loop ──────────────────────────────────────────────────────────

/// Run the str0m poll loop for a single WebRTC peer.
///
/// Receives packets from the UdpMux via `pkt_rx` channel instead of
/// reading from a per-connection UDP socket. Sends via `mux.send_to()`.
async fn run_rtc_event_loop(
    mut rtc: str0m::Rtc,
    mux: Arc<UdpMux>,
    candidate_addr: SocketAddr,
    ufrag: String,
    mut pkt_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    session_mgr: SharedSessionManager,
    auth_key: Option<AuthKey>,
) {
    let mut channel_id: Option<str0m::channel::ChannelId> = None;
    let (msg_tx, mut msg_rx) = mpsc::channel::<Vec<u8>>(256);
    let (ws_msg_tx, ws_msg_rx) = mpsc::channel::<WsMessage>(256);
    let mut ws_msg_rx = Some(ws_msg_rx); // Option to allow one-time take

    // Track the peer's source address for mux peer_map cleanup
    let mut peer_addr: Option<SocketAddr> = None;

    loop {
        match rtc.poll_output() {
            Ok(str0m::Output::Transmit(transmit)) => {
                // Track peer address for mux routing of non-STUN packets
                if peer_addr.is_none() {
                    peer_addr = Some(transmit.destination);
                    mux.register_peer(transmit.destination, &ufrag);
                }
                if let Err(e) = mux.send_to(&transmit.contents, transmit.destination).await {
                    log::debug!("WebRTC send error: {}", e);
                }
            }

            Ok(str0m::Output::Event(event)) => match event {
                str0m::Event::IceConnectionStateChange(str0m::IceConnectionState::Disconnected) => {
                    log::info!("WebRTC peer ICE state: Disconnected, disconnecting");
                    break;
                }
                str0m::Event::IceConnectionStateChange(_) => {}
                str0m::Event::Connected => {
                    log::info!("WebRTC peer connected (DTLS handshake complete)");
                }
                str0m::Event::ChannelOpen(id, label) => {
                    log::info!("WebRTC DataChannel opened: id={:?}, label={}", id, label);
                    channel_id = Some(id);

                    if let Some(rx) = ws_msg_rx.take() {
                        let transport = WebrtcControlTransport::new(msg_tx.clone(), rx);
                        let session_mgr = session_mgr.clone();
                        let auth_key = auth_key.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_webrtc_client(transport, &session_mgr, &auth_key).await
                            {
                                log::debug!("WebRTC client handler error: {}", e);
                            }
                        });
                    }
                }
                str0m::Event::ChannelData(data) => {
                    if !data.binary || data.data.is_empty() {
                        continue;
                    }
                    log::info!(
                        "WebRTC ChannelData: {} bytes, type=0x{:02x}",
                        data.data.len(),
                        data.data[0]
                    );
                    match data.data[0] {
                        WS_TYPE_CONTROL if data.data.len() >= 5 => {
                            let len = u32::from_be_bytes([
                                data.data[1],
                                data.data[2],
                                data.data[3],
                                data.data[4],
                            ]) as usize;
                            if data.data.len() >= 5 + len {
                                match Control::decode(&data.data[5..5 + len]) {
                                    Ok(ctrl) => {
                                        log::info!("WebRTC decoded control: {:?}", ctrl);
                                        if ws_msg_tx
                                            .send(WsMessage::Control(Box::new(ctrl)))
                                            .await
                                            .is_err()
                                        {
                                            log::warn!(
                                                "WebRTC ws_msg_tx send failed — channel closed"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        log::warn!("WebRTC control decode error: {}", e)
                                    }
                                }
                            }
                        }
                        WS_TYPE_TERM_INPUT => {
                            let payload = Bytes::copy_from_slice(&data.data[1..]);
                            let _ = ws_msg_tx.send(WsMessage::TermInput(payload)).await;
                        }
                        _ => {}
                    }
                }
                str0m::Event::ChannelClose(id) => {
                    log::info!("WebRTC DataChannel closed: {:?}", id);
                    break;
                }
                _ => {}
            },

            Ok(str0m::Output::Timeout(timeout)) => {
                let now = Instant::now();
                let sleep_dur = if timeout > now {
                    (timeout - now).min(Duration::from_secs(5))
                } else {
                    Duration::ZERO
                };

                if sleep_dur.is_zero() {
                    let _ = rtc.handle_input(str0m::Input::Timeout(Instant::now()));
                    continue;
                }

                tokio::select! {
                    _ = tokio::time::sleep(sleep_dur) => {
                        let _ = rtc.handle_input(str0m::Input::Timeout(Instant::now()));
                    }
                    result = pkt_rx.recv() => {
                        match result {
                            Some((source, data)) => {
                                if let Ok(receive) = str0m::net::Receive::new(
                                    str0m::net::Protocol::Udp,
                                    source,
                                    candidate_addr,
                                    &data,
                                ) {
                                    let _ = rtc.handle_input(str0m::Input::Receive(
                                        Instant::now(),
                                        receive,
                                    ));
                                }
                            }
                            None => break, // mux channel closed
                        }
                    }
                    msg = msg_rx.recv(), if channel_id.is_some() => {
                        if let (Some(msg), Some(cid)) = (msg, channel_id)
                            && let Some(mut ch) = rtc.channel(cid)
                        {
                            if let Err(e) = ch.write(true, &msg) {
                                log::debug!("WebRTC channel write error: {}", e);
                            }
                            // Drain all pending messages to batch into fewer SCTP packets
                            while let Ok(msg) = msg_rx.try_recv() {
                                if let Err(e) = ch.write(true, &msg) {
                                    log::debug!("WebRTC channel write error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            Err(e) => {
                log::debug!("WebRTC poll error: {}", e);
                break;
            }
        }
    }

    // Cleanup: unregister from mux
    mux.unregister(&ufrag);
    log::info!("WebRTC event loop ended (ufrag={})", ufrag);
}

// ── WebRTC client handler ───────────────────────────────────────────────────

/// Handle a WebRTC client after DataChannel opens.
async fn handle_webrtc_client(
    mut transport: WebrtcControlTransport,
    session_mgr: &SharedSessionManager,
    auth_key: &Option<AuthKey>,
) -> Result<()> {
    use crate::transport::mutual_auth;

    if !mutual_auth(&mut transport, auth_key).await? {
        return Ok(());
    }

    let first_ctrl = match transport.recv_control().await? {
        Some(ctrl) => ctrl,
        None => return Ok(()),
    };

    match first_ctrl {
        Control::SessionList { .. } | Control::SessionAttach { .. } => {
            crate::transport::ws_like_client_loop(
                &mut transport,
                session_mgr,
                "client-",
                first_ctrl,
            )
            .await
        }
        _ => {
            log::warn!("Unexpected first control from WebRTC client");
            Ok(())
        }
    }
}
