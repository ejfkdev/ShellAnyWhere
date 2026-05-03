//! WebRTC Data Channel support for browser clients.
//!
//! Uses str0m (sans-I/O WebRTC) with ICE Lite for direct connections.
//! All WebRTC peers share a single UDP socket, routed by STUN ufrag.
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

/// Initialize the WebRTC crypto provider. Must be called once at startup.
pub fn init_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}

// ── WebRtcMux: packet router for WebRTC peers ──────────────────────────────

/// Routes WebRTC packets to the correct `Rtc` instance by:
/// 1. Parsing STUN Binding Request USERNAME to extract the server ufrag
/// 2. Falling back to source-address lookup for post-ICE DTLS/SCTP packets
///
/// Reads packets from the bound UDP socket in a background task and dispatches
/// them to the appropriate Rtc event loop. Outbound packets are sent through
/// the same socket.
pub struct WebRtcMux {
    socket: Arc<tokio::net::UdpSocket>,
    listen_addr: SocketAddr,
    /// server ufrag → channel to feed incoming packets into the Rtc event loop
    routes: std::sync::Mutex<HashMap<String, PktSender>>,
    /// peer source addr → server ufrag (established after first STUN exchange)
    peer_map: std::sync::Mutex<HashMap<SocketAddr, String>>,
}

use std::collections::HashMap;

/// Type alias for the channel that feeds incoming packets into an Rtc event loop.
type PktSender = mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>;

impl WebRtcMux {
    /// Create a new WebRtcMux using the given bound UDP socket.
    ///
    /// Spawns a background task that reads packets from the socket and
    /// dispatches them to the correct Rtc instance.
    pub fn new(socket: tokio::net::UdpSocket) -> Arc<Self> {
        let listen_addr = socket.local_addr().expect("WebRTC UDP socket local_addr");
        let socket = Arc::new(socket);
        let mux = Arc::new(Self {
            socket: socket.clone(),
            listen_addr,
            routes: std::sync::Mutex::new(HashMap::new()),
            peer_map: std::sync::Mutex::new(HashMap::new()),
        });

        // Spawn the packet receive loop
        let mux_clone = mux.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                match mux_clone.socket.recv_from(&mut buf).await {
                    Ok((n, source)) => {
                        mux_clone.dispatch_packet(source, &buf[..n]);
                    }
                    Err(e) => {
                        log::warn!("WebRTC mux: UDP recv error: {}", e);
                        break;
                    }
                }
            }
        });

        log::info!("WebRTC mux ready on UDP {}", listen_addr);
        mux
    }

    /// The local address the UDP socket is bound to.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Send a UDP packet through the socket.
    pub async fn send_to(&self, data: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        self.socket.send_to(data, dest).await?;
        Ok(())
    }

    /// Register a new Rtc instance to receive packets for the given server ufrag.
    /// Returns the receiver that the Rtc event loop should use for incoming packets.
    pub fn register(&self, ufrag: &str) -> mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().insert(ufrag.to_string(), tx);
        log::debug!("WebRTC mux: registered ufrag={}", ufrag);
        rx
    }

    /// Unregister a peer (on disconnect).
    pub fn unregister(&self, ufrag: &str) {
        self.routes.lock().unwrap().remove(ufrag);
        // Also remove peer_map entries pointing to this ufrag
        let mut pm = self.peer_map.lock().unwrap();
        pm.retain(|_, v| v != ufrag);
        log::debug!("WebRTC mux: unregistered ufrag={}", ufrag);
    }

    /// Map a peer's source address to its server ufrag (after ICE completes).
    pub fn register_peer(&self, peer_addr: SocketAddr, ufrag: &str) {
        self.peer_map
            .lock()
            .unwrap()
            .insert(peer_addr, ufrag.to_string());
    }

    /// Route a received packet to the correct Rtc event loop.
    fn dispatch_packet(&self, source: SocketAddr, packet: &[u8]) {
        let ufrag = extract_stun_ufrag(packet)
            .or_else(|| self.peer_map.lock().unwrap().get(&source).cloned());

        match ufrag {
            Some(ufrag) => {
                if let Some(tx) = self.routes.lock().unwrap().get(&ufrag) {
                    let _ = tx.send((source, packet.to_vec()));
                } else {
                    log::debug!("WebRTC mux: no route for ufrag={}", ufrag);
                }
            }
            None => {
                log::debug!("WebRTC mux: cannot route packet from {}", source);
            }
        }
    }
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

/// Try to extract the server (remote) ufrag from a STUN Binding Request.
///
/// STUN Binding Request USERNAME attribute format: `remote_ufrag:local_ufrag`
/// where "remote" is the server's ufrag (the one we need for routing).
///
/// Returns `None` if the packet is not a STUN Binding Request or has no USERNAME.
fn extract_stun_ufrag(packet: &[u8]) -> Option<String> {
    // STUN header: first 2 bits must be 00, method = Binding (0x0001)
    if packet.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
    // First 2 bits must be 00 (STUN, not channel data or DTLS)
    if msg_type & 0xC000 != 0 {
        return None;
    }
    // STUN magic cookie at offset 4
    if u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) != 0x2112A442 {
        return None;
    }

    let msg_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < 20 + msg_len {
        return None;
    }

    // Parse STUN attributes
    let mut offset = 20;
    while offset + 4 <= 20 + msg_len {
        let attr_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let attr_len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let attr_start = offset + 4;
        let attr_end = attr_start + attr_len;
        if attr_end > packet.len() {
            break;
        }

        if attr_type == 0x0006 {
            // USERNAME attribute
            if let Ok(username) = std::str::from_utf8(&packet[attr_start..attr_end]) {
                // Format: "remote_ufrag:local_ufrag"
                if let Some(remote) = username.split(':').next() {
                    return Some(remote.to_string());
                }
            }
        }

        // Attributes are padded to 4-byte boundary
        let padded = (attr_len + 3) & !3;
        offset = attr_start + padded;
    }

    None
}

// ── Handle offer ────────────────────────────────────────────────────────────

/// Handle a WebRTC SDP offer and return the SDP answer.
/// Uses the shared UDP mux instead of creating a per-connection socket.
pub async fn handle_offer(
    offer_sdp: &str,
    public_ip: Option<std::net::IpAddr>,
    mux: &Arc<WebRtcMux>,
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
/// Receives packets from the WebRtcMux via `pkt_rx` channel instead of
/// reading from a per-connection UDP socket. Sends via `mux.send_to()`.
async fn run_rtc_event_loop(
    mut rtc: str0m::Rtc,
    mux: Arc<WebRtcMux>,
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
