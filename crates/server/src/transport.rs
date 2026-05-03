/// Protocol-agnostic transport abstraction and shared business logic.
///
/// The `ControlTransport` trait abstracts over different wire protocols
/// (WebSocket type-prefixed binary, WT raw streams, WT ContextStream),
/// allowing auth, attach, and message-loop logic to be shared.
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use saw_core::crypto::auth::{AuthKey, server_auth_challenge, server_auth_verify};
use saw_core::protocol::context_stream::ContextStream;
use saw_core::protocol::control::{AttachMode, Control};

use crate::relay;
use crate::session_mgr::{SharedSessionManager, WsClientHandle};

// ── Constants ──────────────────────────────────────────────────────────────

/// WebSocket message type prefix: Control message.
pub const WS_TYPE_CONTROL: u8 = 0x00;
/// WebSocket message type prefix: TerminalIO output (server→client).
pub const WS_TYPE_TERM_OUTPUT: u8 = 0x01;
/// WebSocket message type prefix: TerminalIO input (client→server).
pub const WS_TYPE_TERM_INPUT: u8 = 0x02;

// ── WsMessage ─────────────────────────────────────────────────────────────

/// Typed result from reading a WS message (handles both Control and TermInput).
pub enum WsMessage {
    Control(Box<Control>),
    TermInput(Bytes),
    Closed,
}

// ── ControlTransport trait ─────────────────────────────────────────────────

/// Abstraction over sending and receiving Control messages.
/// Implemented by WS, WT raw, and WT ContextStream transports.
#[async_trait]
pub trait ControlTransport: Send {
    /// Send a Control message to the peer.
    async fn send_control(&mut self, ctrl: &Control) -> Result<()>;

    /// Receive a Control message from the peer.
    /// Returns `Ok(None)` if the stream is closed.
    async fn recv_control(&mut self) -> Result<Option<Control>>;

    /// Send pre-encoded control bytes (from forward_to_client).
    /// The implementation handles protocol-specific framing.
    async fn send_outgoing(&mut self, data: Bytes) -> Result<()>;
}

// ── WsControlTransport ─────────────────────────────────────────────────────

/// The concrete WS stream type used by handle_native_ws.
pub type WsStream =
    tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

/// ControlTransport for native WebSocket (type-prefixed binary messages).
pub struct WsControlTransport {
    msg_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ws_stream: futures::stream::SplitStream<WsStream>,
}

impl WsControlTransport {
    pub fn new(
        msg_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        ws_stream: futures::stream::SplitStream<WsStream>,
    ) -> Self {
        Self { msg_tx, ws_stream }
    }
}

#[async_trait]
impl WsLikeTransport for WsControlTransport {
    async fn recv_ws_message(&mut self) -> Result<WsMessage> {
        loop {
            match self.ws_stream.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                    if data.is_empty() {
                        continue;
                    }
                    match data[0] {
                        WS_TYPE_CONTROL => {
                            if data.len() < 5 {
                                continue;
                            }
                            let len =
                                u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
                            if data.len() < 5 + len {
                                continue;
                            }
                            return Ok(WsMessage::Control(Box::new(Control::decode(
                                &data[5..5 + len],
                            )?)));
                        }
                        WS_TYPE_TERM_INPUT => {
                            let payload = Bytes::copy_from_slice(&data[1..]);
                            return Ok(WsMessage::TermInput(payload));
                        }
                        _ => continue,
                    }
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                    return Ok(WsMessage::Closed);
                }
                Some(Err(_)) => return Ok(WsMessage::Closed),
                None => return Ok(WsMessage::Closed),
                _ => continue,
            }
        }
    }

    fn msg_tx(&self) -> &tokio::sync::mpsc::Sender<Vec<u8>> {
        &self.msg_tx
    }
}

#[async_trait]
impl ControlTransport for WsControlTransport {
    async fn send_control(&mut self, ctrl: &Control) -> Result<()> {
        ws_send_control(&self.msg_tx, ctrl).await
    }

    async fn recv_control(&mut self) -> Result<Option<Control>> {
        loop {
            match self.ws_stream.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                    if data.len() < 5 || data[0] != WS_TYPE_CONTROL {
                        continue;
                    }
                    let len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
                    if data.len() < 5 + len {
                        continue;
                    }
                    return Ok(Some(Control::decode(&data[5..5 + len])?));
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return Ok(None),
                Some(Err(_)) => return Ok(None),
                None => return Ok(None),
                _ => continue,
            }
        }
    }

    async fn send_outgoing(&mut self, data: Bytes) -> Result<()> {
        ws_send_outgoing(&self.msg_tx, data).await
    }
}

// ── ContextTransport ─────────────────────────────────────────────────────

/// ControlTransport backed by a ContextStream (post-auth).
pub struct ContextTransport<S: futures::AsyncRead + futures::AsyncWrite + Unpin + Send> {
    stream: ContextStream<S>,
}

impl<S: futures::AsyncRead + futures::AsyncWrite + Unpin + Send> ContextTransport<S> {
    pub fn new(stream: ContextStream<S>) -> Self {
        Self { stream }
    }
}

#[async_trait]
impl<S: futures::AsyncRead + futures::AsyncWrite + Unpin + Send> ControlTransport
    for ContextTransport<S>
{
    async fn send_control(&mut self, ctrl: &Control) -> Result<()> {
        self.stream.send_control(ctrl).await
    }

    async fn recv_control(&mut self) -> Result<Option<Control>> {
        self.stream.recv_control().await
    }

    async fn send_outgoing(&mut self, data: Bytes) -> Result<()> {
        self.stream.send_data(&data).await
    }
}

// ── WsLikeTransport trait ────────────────────────────────────────────────────

/// Extended transport trait for protocols that multiplex Control and TermInput
/// on a single channel (WebSocket, WebRTC Data Channel).
/// Unifies the client main loop for WS and WebRTC clients.
#[async_trait]
pub trait WsLikeTransport: ControlTransport + Send {
    /// Receive the next message (Control, TermInput, or Closed).
    async fn recv_ws_message(&mut self) -> Result<WsMessage>;
    /// Get the outbound message channel (for WsClientHandle).
    fn msg_tx(&self) -> &tokio::sync::mpsc::Sender<Vec<u8>>;
}

// ── WebrtcControlTransport ─────────────────────────────────────────────────

/// ControlTransport backed by a WebRTC Data Channel via channel bridge.
/// The str0m event loop task parses inbound messages and pushes them
/// into `ws_msg_tx`; outbound messages go through `msg_tx` (same format
/// as WS, reusing `WsClientHandle`).
pub struct WebrtcControlTransport {
    msg_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ws_msg_rx: tokio::sync::mpsc::Receiver<WsMessage>,
}

impl WebrtcControlTransport {
    pub fn new(
        msg_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        ws_msg_rx: tokio::sync::mpsc::Receiver<WsMessage>,
    ) -> Self {
        Self { msg_tx, ws_msg_rx }
    }
}

#[async_trait]
impl WsLikeTransport for WebrtcControlTransport {
    async fn recv_ws_message(&mut self) -> Result<WsMessage> {
        match self.ws_msg_rx.recv().await {
            Some(msg) => Ok(msg),
            None => Ok(WsMessage::Closed),
        }
    }

    fn msg_tx(&self) -> &tokio::sync::mpsc::Sender<Vec<u8>> {
        &self.msg_tx
    }
}

#[async_trait]
impl ControlTransport for WebrtcControlTransport {
    async fn send_control(&mut self, ctrl: &Control) -> Result<()> {
        ws_send_control(&self.msg_tx, ctrl).await
    }

    async fn recv_control(&mut self) -> Result<Option<Control>> {
        loop {
            match self.ws_msg_rx.recv().await {
                Some(WsMessage::Control(ctrl)) => return Ok(Some(*ctrl)),
                Some(WsMessage::TermInput(_)) => continue,
                Some(WsMessage::Closed) => return Ok(None),
                None => return Ok(None),
            }
        }
    }

    async fn send_outgoing(&mut self, data: Bytes) -> Result<()> {
        ws_send_outgoing(&self.msg_tx, data).await
    }
}

// ── Shared WS-frame helpers ─────────────────────────────────────────────────

/// Encode and send a Control message as a WS-type-prefixed frame via mpsc channel.
async fn ws_send_control(
    msg_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    ctrl: &Control,
) -> Result<()> {
    let payload = ctrl.encode()?;
    let len = payload.len() as u32;
    let mut msg = Vec::with_capacity(1 + 4 + payload.len());
    msg.push(WS_TYPE_CONTROL);
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&payload);
    msg_tx
        .send(msg)
        .await
        .map_err(|_| anyhow::anyhow!("channel closed"))
}

/// Send pre-encoded control bytes as a WS-type-prefixed frame via mpsc channel.
async fn ws_send_outgoing(msg_tx: &tokio::sync::mpsc::Sender<Vec<u8>>, data: Bytes) -> Result<()> {
    let len = data.len() as u32;
    let mut msg = Vec::with_capacity(1 + 4 + data.len());
    msg.push(WS_TYPE_CONTROL);
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(&data[..]);
    msg_tx
        .send(msg)
        .await
        .map_err(|_| anyhow::anyhow!("channel closed"))
}

// ── Shared business logic (continued) ─────────────────────────────────────

/// Perform server-side mutual challenge-response authentication.
/// If auth_key is None, authentication is skipped and `Ok(true)` is returned.
/// Returns `Ok(true)` on success, `Ok(false)` if verification failed.
pub async fn mutual_auth(
    transport: &mut impl ControlTransport,
    auth_key: &Option<AuthKey>,
) -> Result<bool> {
    let key = match auth_key {
        Some(k) => k,
        _ => return Ok(true),
    };

    // 1. Read AuthInit
    let client_nonce = match transport.recv_control().await? {
        Some(Control::AuthInit { client_nonce }) => client_nonce,
        Some(_) => anyhow::bail!("Expected AuthInit from client"),
        None => anyhow::bail!("Connection closed during auth"),
    };

    // 2. Send AuthChallenge
    let (server_nonce, server_proof) = server_auth_challenge(key, &client_nonce);
    transport
        .send_control(&Control::AuthChallenge {
            nonce: server_nonce.clone(),
            proof: server_proof,
        })
        .await?;

    // 3. Read AuthResponse
    let response = match transport.recv_control().await? {
        Some(Control::AuthResponse { response }) => response,
        Some(_) => anyhow::bail!("Expected AuthResponse from client"),
        None => anyhow::bail!("Connection closed during auth"),
    };

    // 4. Verify
    if !server_auth_verify(key, &server_nonce, &response) {
        transport
            .send_control(&Control::AuthResult { ok: false })
            .await
            .ok();
        return Ok(false);
    }
    transport
        .send_control(&Control::AuthResult { ok: true })
        .await?;
    log::info!("Mutual authentication successful");
    Ok(true)
}

/// Attach a client to a session.
/// Returns `Some((effective_client_id, client_rx))` on success, `None` on failure.
/// Handle storage (set_client_ws_handle) is the caller's
/// responsibility — it's protocol-specific.
pub async fn client_attach(
    transport: &mut impl ControlTransport,
    session_mgr: &SharedSessionManager,
    session_id: &str,
    mode: AttachMode,
    default_client_id: &str,
    previous_client_id: &Option<String>,
) -> Result<Option<(String, tokio::sync::mpsc::Receiver<Bytes>)>> {
    let effective_client_id =
        crate::listener::resolve_client_id(session_mgr, previous_client_id, default_client_id);
    let (client_tx, client_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let attached =
        session_mgr.attach_client(session_id, effective_client_id.clone(), mode, client_tx);
    if let Some(_replay) = attached {
        transport
            .send_control(&Control::AttachAck {
                session_id: session_id.to_string(),
                client_id: effective_client_id.clone(),
                mode,
            })
            .await
            .ok();
        Ok(Some((effective_client_id, client_rx)))
    } else {
        log::warn!(
            "Client {} failed to attach to session {}",
            effective_client_id,
            session_id
        );
        Ok(None)
    }
}

/// Main control loop for an attached client.
/// Handles: SessionDetach → break, Ping → Pong, other → relay::handle_client_frame.
/// Forwards: outgoing bytes from client_rx, events from event_rx, keepalive pings.
/// Cleans up client on exit.
pub async fn client_loop(
    transport: &mut impl ControlTransport,
    session_mgr: &SharedSessionManager,
    _session_id: &str,
    client_id: &str,
    client_rx: &mut tokio::sync::mpsc::Receiver<Bytes>,
    event_rx: &mut tokio::sync::broadcast::Receiver<Control>,
) -> Result<()> {
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(10));
    keepalive.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            ctrl_result = transport.recv_control() => {
                match ctrl_result {
                    Ok(Some(ctrl)) => match ctrl {
                        Control::SessionDetach { .. } => break,
                        Control::Ping => {
                            let _ = transport.send_control(&Control::Pong).await;
                        }
                        ctrl => {
                            relay::handle_client_frame(session_mgr, client_id, ctrl);
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        log::debug!("Client {} connection lost: {}", client_id, e);
                        break;
                    }
                }
            }
            outgoing = client_rx.recv() => {
                match outgoing {
                    Some(data) => {
                        if let Err(e) = transport.send_outgoing(data).await {
                            log::debug!("Client {} send error: {}", client_id, e);
                            break;
                        }
                    }
                    None => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(ctrl) => {
                        if let Err(e) = transport.send_control(&ctrl).await {
                            log::debug!("Client {} event send error: {}", client_id, e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("Client {} broadcast lagged by {}", client_id, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = keepalive.tick() => {
                let _ = transport.send_control(&Control::Ping).await;
            }
        }
    }

    session_mgr.remove_client(client_id);
    Ok(())
}

/// Main client loop for WS-like transports (WebSocket, WebRTC Data Channel).
///
/// These transports multiplex Control and TermInput on a single channel,
/// so this loop handles both message types in one `select!`.
/// On exit, removes the client from the session manager.
pub async fn ws_like_client_loop(
    transport: &mut impl WsLikeTransport,
    session_mgr: &SharedSessionManager,
    client_id_prefix: &str,
    initial_ctrl: Control,
) -> Result<()> {
    use crate::listener::{generate_alphanum_id, start_terminal_io_relay};

    let client_id = format!("{}{}", client_id_prefix, generate_alphanum_id(8));
    let ws_handle = WsClientHandle {
        msg_tx: transport.msg_tx().clone(),
    };
    session_mgr.set_client_ws_handle(&client_id, ws_handle.clone());

    let mut event_rx = session_mgr.subscribe();
    let mut current_session_id: Option<String> = None;
    let mut current_client_rx: Option<tokio::sync::mpsc::Receiver<Bytes>> = None;
    let mut current_cid: Option<String> = None;
    let (term_input_tx, term_input_rx_holder) = tokio::sync::mpsc::channel::<Bytes>(256);
    let mut term_input_rx: Option<tokio::sync::mpsc::Receiver<Bytes>> = Some(term_input_rx_holder);

    // Handle initial control
    match initial_ctrl {
        Control::SessionList { .. } => {
            let sessions = session_mgr.list_sessions();
            transport
                .send_control(&Control::SessionList { sessions })
                .await?;
        }
        Control::SessionAttach {
            session_id,
            mode,
            previous_client_id,
        } => {
            if let Some((cid, client_rx)) = client_attach(
                transport,
                session_mgr,
                &session_id,
                mode,
                &client_id,
                &previous_client_id,
            )
            .await?
            {
                session_mgr.set_client_ws_handle(&cid, ws_handle.clone());
                current_session_id = Some(session_id.clone());
                current_client_rx = Some(client_rx);
                current_cid = Some(cid.clone());
                if let Err(e) =
                    start_terminal_io_relay(session_mgr, &session_id, &cid, term_input_rx.take())
                        .await
                {
                    log::warn!("Failed to start TerminalIO relay: {}", e);
                }
            }
        }
        _ => {}
    }

    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(10));
    keepalive.tick().await;

    loop {
        tokio::select! {
            ws_msg = transport.recv_ws_message() => {
                let ws_msg = ws_msg?;
                match ws_msg {
                    WsMessage::Control(ref ctrl) => {
                        log::info!("ws_like_client_loop({}): received control: {:?}", client_id_prefix, ctrl);
                    }
                    WsMessage::TermInput(_) => {
                        log::info!("ws_like_client_loop({}): received TermInput", client_id_prefix);
                    }
                    WsMessage::Closed => {
                        log::info!("ws_like_client_loop({}): received Closed", client_id_prefix);
                    }
                }
                match ws_msg {
                    WsMessage::Control(ctrl) => match *ctrl {
                        Control::SessionList { .. } => {
                            let sessions = session_mgr.list_sessions();
                            transport.send_control(&Control::SessionList { sessions }).await?;
                        }
                        Control::SessionAttach { session_id, mode, previous_client_id } => {
                            let default_id = format!("{}{}", client_id_prefix, generate_alphanum_id(8));
                            if let Some((cid, client_rx)) = client_attach(
                                transport, session_mgr, &session_id, mode, &default_id, &previous_client_id,
                            ).await? {
                                session_mgr.set_client_ws_handle(&cid, ws_handle.clone());
                                current_session_id = Some(session_id.clone());
                                current_client_rx = Some(client_rx);
                                current_cid = Some(cid.clone());
                                if let Err(e) = start_terminal_io_relay(
                                    session_mgr, &session_id, &cid, term_input_rx.take(),
                                ).await {
                                    log::warn!("Failed to start TerminalIO relay: {}", e);
                                }
                            }
                        }
                        Control::SessionDetach { .. } => break,
                        Control::Ping => {
                            let _ = transport.send_control(&Control::Pong).await;
                        }
                        ctrl => {
                            let effective_id = current_cid.as_deref().unwrap_or(&client_id);
                            log::info!("ws_like_client_loop: unhandled ctrl type for {}: {:?}", effective_id, std::mem::discriminant(&ctrl));
                            relay::handle_client_frame(session_mgr, effective_id, ctrl);
                        }
                    },
                    WsMessage::TermInput(payload) => {
                        let _ = term_input_tx.send(payload).await;
                    }
                    WsMessage::Closed => break,
                }
            }
            outgoing = async {
                match current_client_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match outgoing {
                    Some(data) => {
                        if let Err(e) = transport.send_outgoing(data).await {
                            log::debug!("{} client send error: {}", client_id_prefix, e);
                            break;
                        }
                    }
                    None => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(ctrl) => {
                        if let Err(e) = transport.send_control(&ctrl).await {
                            log::debug!("{} event send error: {}", client_id_prefix, e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("{} client {} broadcast lagged by {}", client_id_prefix, client_id, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = keepalive.tick() => {
                if transport.send_control(&Control::Ping).await.is_err() {
                    break;
                }
            }
        }
    }

    log::info!(
        "{} client handler exiting: client_id={} session={:?}",
        client_id_prefix,
        current_cid.as_deref().unwrap_or(&client_id),
        current_session_id
    );
    if let Some(ref cid) = current_cid {
        session_mgr.remove_client(cid);
    } else {
        session_mgr.remove_client(&client_id);
    }
    Ok(())
}
