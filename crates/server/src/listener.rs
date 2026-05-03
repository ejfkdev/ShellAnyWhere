use crate::session_mgr;
use crate::session_mgr::SharedSessionManager;
use crate::session_mgr::WsClientHandle;
use crate::ssh::{self, SharedAuthorizedKeys, SshServerConfig};
use crate::transport::WS_TYPE_TERM_OUTPUT;
use bytes::Bytes;
use russh::server::Server;
use saw_core::crypto::auth::{AuthKey, get_or_create_token};
use saw_core::crypto::tls;
use saw_core::protocol::control::Control;

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ── PeekedStream: replays peeked bytes before delegating to inner stream ──

/// A stream wrapper that replays already-read bytes before delegating to the
/// inner stream. Used for protocol detection after TLS handshake.
struct PeekedStream<S> {
    inner: S,
    peeked: Vec<u8>,
    peeked_pos: usize,
}

impl<S> PeekedStream<S> {
    fn new(inner: S, peeked: Vec<u8>) -> Self {
        Self {
            inner,
            peeked,
            peeked_pos: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PeekedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // First drain peeked bytes
        if this.peeked_pos < this.peeked.len() {
            let remaining = &this.peeked[this.peeked_pos..];
            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            this.peeked_pos += to_copy;
            return Poll::Ready(Ok(()));
        }
        // Then delegate to inner stream
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PeekedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Check if a byte is the first byte of an HTTP method (uppercase ASCII letter).
fn is_http_byte(b: u8) -> bool {
    b.is_ascii_uppercase()
}

/// WS TerminalIO output sub-flag: payload is raw (uncompressed)
const TERM_FLAG_RAW: u8 = 0x00;
/// WS TerminalIO output sub-flag: payload is lz4 compressed (compress_prepend_size format)
#[allow(dead_code)]
const TERM_FLAG_LZ4: u8 = 0x01;

/// Server configuration
pub struct ServerConfig {
    pub listen_addr: String,
    pub token: Option<String>,
    pub ssh_authorized_keys: Option<String>,
    pub ssh_idle_timeout_secs: u64,
    pub ssh_enabled: bool,
    pub ssh_password_auth: bool,
    pub peek_timeout_secs: u64,
    pub data_dir: std::path::PathBuf,
    pub keep_alive_interval: std::time::Duration,
    pub idle_timeout: std::time::Duration,
    pub cert_file: std::path::PathBuf,
    pub key_file: std::path::PathBuf,
    pub webrtc_public_ip: String,
    pub web_dir: Option<std::path::PathBuf>,
}

/// Start the server listener.
pub async fn run_server(config: ServerConfig) -> anyhow::Result<()> {
    // Resolve the raw token, then immediately derive the auth key.
    // The plaintext token is retained only when SSH password auth is enabled
    // (which requires plaintext comparison inside the encrypted SSH tunnel).
    // All other auth paths (WS/WT/WebRTC) use only the HKDF-derived key.
    let (auth_key, resolved_token) = match config.token.clone() {
        Some(t) if !t.is_empty() => {
            let resolved = get_or_create_token(Some(t))?;
            log::info!("Server token configured");
            (Some(AuthKey::derive(&resolved)), Some(resolved))
        }
        Some(_) => {
            log::info!("Server running without authentication");
            (None, None)
        }
        None => {
            let resolved = get_or_create_token(None)?;
            log::info!("Server token auto-generated");
            (Some(AuthKey::derive(&resolved)), Some(resolved))
        }
    };

    // Only keep plaintext token in memory when SSH password auth is enabled.
    let ssh_token = if config.ssh_enabled && config.ssh_password_auth {
        resolved_token
    } else {
        None
    };

    // Main TCP listener on listen_addr (SSH + HTTPS)
    let (cert, key) = tls::load_or_generate_cert(&config.cert_file, &config.key_file)?;
    let main_tls_acceptor = tls::build_tls_acceptor(cert, key)?;
    let main_listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;

    let session_mgr = session_mgr::shared_session_manager();

    // Load SSH configuration (only if SSH is enabled)
    let ssh_config = if config.ssh_enabled {
        let ssh_host_key_path = config.data_dir.join("ssh_host_key");
        match ssh::load_or_generate_host_key(Some(&ssh_host_key_path)) {
            Ok(host_key) => {
                let mut authorized_keys = std::collections::HashSet::new();

                let ak_path: Option<String> = config.ssh_authorized_keys.clone().or_else(|| {
                    let default_path = dirs::home_dir()
                        .unwrap_or_else(|| std::path::PathBuf::from("."))
                        .join(".config")
                        .join("ShellAnyWhere")
                        .join("authorized_keys");
                    if default_path.exists() {
                        Some(default_path.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                });
                if let Some(ref path) = ak_path {
                    match ssh::load_authorized_keys(path) {
                        Ok(keys) => {
                            log::info!("Loaded {} authorized keys from {}", keys.len(), path);
                            authorized_keys.extend(keys);
                        }
                        Err(e) => {
                            log::warn!("Failed to load authorized keys from {}: {}", path, e);
                        }
                    }
                }

                // Add token-derived SSH public key to authorized_keys
                if let Some(ref key) = auth_key {
                    let derived_pubkey = key.derive_ssh_public_key();
                    let parts: Vec<&str> = derived_pubkey.splitn(3, ' ').collect();
                    if parts.len() >= 2
                        && let Ok(pk) = russh::keys::parse_public_key_base64(parts[1])
                    {
                        log::info!("Added token-derived SSH public key to authorized_keys");
                        authorized_keys.insert(pk);
                    }
                }

                let shared_authorized_keys = Arc::new(std::sync::Mutex::new(authorized_keys));

                let ssh_server_config = Arc::new(russh::server::Config {
                    inactivity_timeout: Some(Duration::from_secs(config.ssh_idle_timeout_secs)),
                    auth_rejection_time: Duration::from_secs(1),
                    auth_rejection_time_initial: Some(Duration::from_secs(0)),
                    keepalive_interval: Some(Duration::from_secs(30)),
                    keepalive_max: 3,
                    keys: vec![host_key],
                    ..Default::default()
                });
                Some((ssh_server_config, shared_authorized_keys))
            }
            Err(e) => {
                log::warn!("SSH disabled: failed to load host key: {}", e);
                None
            }
        }
    } else {
        log::info!("SSH protocol disabled by configuration");
        None
    };

    let ssh_status = if ssh_config.is_some() {
        if config.ssh_password_auth {
            "enabled (password auth on)"
        } else {
            "enabled (password auth off)"
        }
    } else {
        "disabled"
    };
    log::info!(
        "Server started on {} (SSH {})",
        config.listen_addr,
        ssh_status
    );

    let shared_authorized_keys = ssh_config
        .as_ref()
        .map(|(_, ak)| ak.clone())
        .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())));

    // Initialize WebRTC crypto provider once at startup
    crate::webrtc::init_crypto();

    // ── Unified UDP socket (WebRTC + KCP on the same port) ──
    // Parse listen_addr to determine the UDP port (same as TCP)
    let udp_port = config
        .listen_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(18708);
    // Use [::] for dual-stack (IPv4+IPv6) support
    let udp_addr: std::net::SocketAddr = format!("[::]:{}", udp_port).parse().unwrap();
    let udp_socket = tokio::net::UdpSocket::bind(udp_addr).await?;
    log::info!("UDP socket bound on {}", udp_socket.local_addr()?);

    // Resolve WebRTC public IP (None = auto-detect from local address)
    let webrtc_public_ip: Option<std::net::IpAddr> = if !config.webrtc_public_ip.is_empty() {
        config.webrtc_public_ip.parse().ok()
    } else {
        None
    };
    log::info!(
        "WebRTC enabled (public IP: {})",
        webrtc_public_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "auto-detect".to_string())
    );

    // Create UdpMux (unified WebRTC + KCP packet routing on one socket)
    let udp_mux = crate::udp_mux::UdpMux::new(udp_socket);

    let app_state = Arc::new(crate::http::AppState {
        auth_key: auth_key.clone(),
        session_mgr: session_mgr.clone(),
        shared_authorized_keys: shared_authorized_keys.clone(),
        webrtc_public_ip,
        udp_mux: udp_mux.clone(),
        web_dir: config.web_dir,
    });

    // ── KCP listener (low-latency UDP transport, shares port with WebRTC via UdpMux) ──
    #[cfg(feature = "kcp")]
    {
        let kcp_transport = udp_mux.create_kcp_transport();
        let kcp_session_mgr = session_mgr.clone();
        let kcp_auth_key = auth_key.clone();
        let kcp_authorized_keys = shared_authorized_keys.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::kcp::run_kcp_listener_with_transport(
                kcp_transport,
                kcp_session_mgr,
                kcp_auth_key,
                kcp_authorized_keys,
            )
            .await
            {
                log::error!("KCP listener error: {}", e);
            }
        });
    }

    loop {
        let (tcp_stream, peer_addr) = main_listener.accept().await?;
        let local_addr = tcp_stream.local_addr().unwrap_or(peer_addr);
        log::info!("New TCP connection from {}", peer_addr);

        let tls_acceptor = main_tls_acceptor.clone();
        let ssh_token_c = ssh_token.clone();
        let session_mgr = session_mgr.clone();
        let ssh_config = ssh_config.clone();
        let peek_timeout = Duration::from_secs(config.peek_timeout_secs);
        let app_state = app_state.clone();
        let listen_addr = config.listen_addr.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 1];
            match tokio::time::timeout(peek_timeout, tcp_stream.peek(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => match buf[0] {
                    0x53 => {
                        // SSH connection
                        if let Some((ssh_server_config, shared_authorized_keys)) = ssh_config {
                            log::info!("SSH connection from {}", peer_addr);
                            let mut ssh_factory = crate::ssh::SshServer::new(SshServerConfig {
                                token: ssh_token_c.clone(),
                                authorized_keys: shared_authorized_keys,
                                session_mgr: session_mgr.clone(),
                                password_auth_enabled: config.ssh_password_auth,
                            });
                            let handler = ssh_factory.new_client(Some(peer_addr));
                            let _ =
                                russh::server::run_stream(ssh_server_config, tcp_stream, handler)
                                    .await;
                        } else {
                            log::warn!("SSH connection from {} but SSH is disabled", peer_addr);
                        }
                    }
                    b if is_http_byte(b) => {
                        log::info!("HTTP connection from {}, redirecting to HTTPS", peer_addr);
                        crate::http::handle_http_redirect(tcp_stream, &listen_addr).await;
                    }
                    _ => {
                        let mut tls_stream = match tls_acceptor.accept(tcp_stream).await {
                            Ok(s) => s,
                            Err(e) => {
                                log::debug!("TLS handshake failed from {}: {}", peer_addr, e);
                                return;
                            }
                        };
                        log::debug!("TLS handshake completed with {}", peer_addr);

                        let mut first_byte = [0u8; 1];
                        match tokio::time::timeout(
                            peek_timeout,
                            tokio::io::AsyncReadExt::read_exact(&mut tls_stream, &mut first_byte),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {
                                if is_http_byte(first_byte[0]) {
                                    log::debug!("HTTPS web connection from {}", peer_addr);
                                    let stream = PeekedStream::new(tls_stream, vec![first_byte[0]]);
                                    crate::http::handle_http_stream(
                                        stream,
                                        app_state.clone(),
                                        local_addr,
                                    )
                                    .await;
                                } else {
                                    log::warn!(
                                        "Non-HTTP TLS connection from {} rejected",
                                        peer_addr
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                log::debug!("Post-TLS read error from {}: {}", peer_addr, e);
                            }
                            Err(_) => {
                                log::debug!(
                                    "Post-TLS protocol detection timeout from {}",
                                    peer_addr
                                );
                            }
                        }
                    }
                },
                _ => {
                    log::debug!("Protocol detection timeout or error from {}", peer_addr);
                }
            }
        });
    }
}

/// Process a single Control message from an agent.
/// Write a varint (LEB128) to a buffer.
fn write_varint(buf: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

/// Start a TerminalIO relay between the agent and a client.
/// Spawns a background task that relays terminal output and input
/// between the agent and the connected client.
pub(crate) async fn start_terminal_io_relay(
    session_mgr: &SharedSessionManager,
    session_id: &str,
    client_id: &str,
    term_input_rx: Option<tokio::sync::mpsc::Receiver<Bytes>>,
) -> anyhow::Result<()> {
    let client_ws_handle = session_mgr.get_client_ws_handle(client_id);
    let kcp_term_rx = session_mgr.subscribe_kcp_term_output(session_id);

    session_mgr.set_terminal_io_active(session_id, client_id, true);

    let sid = session_id.to_string();
    let cid = client_id.to_string();
    let mgr = session_mgr.clone();

    match (client_ws_handle, kcp_term_rx, term_input_rx) {
        // Agent KCP, client WS (browser via WebSocket or WebRTC Data Channel)
        (Some(client_h), Some(term_rx), Some(input_rx)) => {
            tokio::spawn(start_terminal_io_relay_kcp_ws(
                term_rx, client_h, sid, cid, input_rx, mgr,
            ));
        }
        _ => {
            session_mgr.set_terminal_io_active(session_id, client_id, false);
            anyhow::bail!(
                "No matching handles for session {} client {}",
                session_id,
                client_id
            );
        }
    }

    log::info!(
        "TerminalIO relay started for session {} client {}",
        session_id,
        client_id
    );
    Ok(())
}

/// TerminalIO relay: agent KCP → client WS (browser via WebSocket or WebRTC Data Channel)
///
/// KCP agent sends raw bytes via broadcast channel. This relay subscribes to
/// the broadcast, wraps each chunk in WS type 0x01 format, and sends to the
/// browser client. Keyboard input from the browser is forwarded to the agent
/// via the session's input channel.
async fn start_terminal_io_relay_kcp_ws(
    mut term_rx: tokio::sync::broadcast::Receiver<Bytes>,
    client_handle: WsClientHandle,
    session_id: String,
    client_id: String,
    mut term_input_rx: tokio::sync::mpsc::Receiver<Bytes>,
    session_mgr: SharedSessionManager,
) {
    // Send TerminalIO header as first type 0x01 WS message
    let mut header = Vec::new();
    header.push(0x02); // version
    header.push(0x01); // TerminalIO type
    let sid_bytes = session_id.as_bytes();
    write_varint(&mut header, sid_bytes.len());
    header.extend_from_slice(sid_bytes);
    let cid_bytes = client_id.as_bytes();
    write_varint(&mut header, cid_bytes.len());
    header.extend_from_slice(cid_bytes);
    header.push(0x00); // flags: no compression (KCP sends raw)

    let mut msg = Vec::with_capacity(1 + header.len());
    msg.push(WS_TYPE_TERM_OUTPUT);
    msg.extend_from_slice(&header);
    if client_handle.msg_tx.send(msg).await.is_err() {
        log::warn!("KCP-WS relay: failed to send header, channel closed");
        session_mgr.set_terminal_io_active(&session_id, &client_id, false);
        return;
    }

    let mut recv_count: u64 = 0;
    let mut term_input_closed = false;
    let exit_reason;

    loop {
        tokio::select! {
            result = term_rx.recv() => {
                match result {
                    Ok(data) => {
                        recv_count += 1;
                        let mut msg = Vec::with_capacity(2 + data.len());
                        msg.push(WS_TYPE_TERM_OUTPUT);
                        msg.push(TERM_FLAG_RAW);
                        msg.extend_from_slice(&data);
                        if client_handle.msg_tx.send(msg).await.is_err() {
                            exit_reason = "msg_tx_closed".to_string();
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("KCP-WS relay: broadcast lagged by {}", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        exit_reason = "broadcast_closed".to_string();
                        break;
                    }
                }
            }
            data = async {
                if term_input_closed {
                    std::future::pending().await
                } else {
                    term_input_rx.recv().await
                }
            } => {
                match data {
                    Some(data) => {
                        if !session_mgr.forward_kcp_term_input(&session_id, data) {
                            exit_reason = "input_channel_closed".to_string();
                            break;
                        }
                    }
                    None => {
                        log::info!("KCP-WS relay: term_input_rx closed, continuing output-only mode");
                        term_input_closed = true;
                    }
                }
            }
        }
    }

    log::info!(
        "KCP-WS relay ended: recv_count={} reason={}",
        recv_count,
        exit_reason
    );
    session_mgr.set_terminal_io_active(&session_id, &client_id, false);
}

/// Resolve the effective client_id for an attach request.
pub(crate) fn resolve_client_id(
    session_mgr: &SharedSessionManager,
    previous_client_id: &Option<String>,
    default_id: &str,
) -> String {
    if let Some(prev_id) = previous_client_id {
        if !session_mgr.is_client_connected(prev_id) {
            log::info!("Client reconnecting with previous client_id: {}", prev_id);
            return prev_id.clone();
        }
        log::info!(
            "Previous client_id {} is in use, generating new one",
            prev_id
        );
    }
    default_id.to_string()
}

/// Add SSH public keys to the shared authorized keys set.
pub fn add_ssh_keys(ssh_public_keys: &[String], shared_authorized_keys: &SharedAuthorizedKeys) {
    if ssh_public_keys.is_empty() {
        return;
    }
    let mut keys = shared_authorized_keys.lock().unwrap();
    for key_line in ssh_public_keys {
        let parts: Vec<&str> = key_line.splitn(3, ' ').collect();
        if parts.len() >= 2 {
            let _ = russh::keys::parse_public_key_base64(parts[1]).map(|k| keys.insert(k));
        }
    }
}

pub(crate) fn generate_alphanum_id(len: usize) -> String {
    const ALPHABET: &[char] = &[
        '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j',
        'k', 'm', 'n', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
    ];
    nanoid::nanoid!(len, ALPHABET)
}

/// Handle a native WebSocket connection from a browser client.
/// Uses type-prefixed binary messages: 0x00=Control, 0x01=TerminalIO output, 0x02=TerminalIO input.
pub async fn handle_native_ws(
    ws: crate::transport::WsStream,
    app_state: std::sync::Arc<crate::http::AppState>,
) -> anyhow::Result<()> {
    use crate::transport::ControlTransport;
    use futures::{SinkExt, StreamExt};

    let (mut ws_sink, ws_stream) = ws.split();

    // Channel for outbound messages (control + terminal output)
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    // Spawn writer task: drain msg_rx, send to WS
    let _writer_task = tokio::spawn(async move {
        let mut ws_send_count: u64 = 0;
        while let Some(msg) = msg_rx.recv().await {
            ws_send_count += 1;
            if ws_sink
                .send(tokio_tungstenite::tungstenite::Message::Binary(msg.into()))
                .await
                .is_err()
            {
                log::warn!(
                    "WS writer task: send failed after {} messages, closing",
                    ws_send_count
                );
                break;
            }
        }
        log::info!(
            "WS writer task exiting: sent {} messages total",
            ws_send_count
        );
        let _ = ws_sink.close().await;
    });

    // ── Auth using shared mutual_auth ───────────────────────────────
    let mut ws_transport = crate::transport::WsControlTransport::new(msg_tx.clone(), ws_stream);
    if !crate::transport::mutual_auth(&mut ws_transport, &app_state.auth_key).await? {
        return Ok(());
    }

    // ── Read first control to determine role ────────────────────────
    let first_ctrl = match ws_transport.recv_control().await? {
        Some(ctrl) => ctrl,
        None => return Ok(()),
    };

    match first_ctrl {
        Control::SessionRegister { .. } => {
            log::warn!("Unexpected SessionRegister from native WS client");
            Ok(())
        }
        Control::SessionList { .. } | Control::SessionAttach { .. } => {
            handle_native_ws_client(ws_transport, &app_state, first_ctrl).await
        }
        ctrl => {
            log::warn!("Unexpected first control: {:?}", ctrl);
            Ok(())
        }
    }
}

/// Handle a native WebSocket client after auth and role determination.
async fn handle_native_ws_client(
    mut ws_transport: crate::transport::WsControlTransport,
    app_state: &std::sync::Arc<crate::http::AppState>,
    initial_ctrl: Control,
) -> anyhow::Result<()> {
    crate::transport::ws_like_client_loop(
        &mut ws_transport,
        &app_state.session_mgr,
        "client-",
        initial_ctrl,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_alphanum_id() {
        let id1 = generate_alphanum_id(8);
        let id2 = generate_alphanum_id(8);
        assert_eq!(id1.len(), 8);
        assert_eq!(id2.len(), 8);
        assert!(id1 != id2);
    }

    #[tokio::test]
    async fn test_server_config() {
        let config = ServerConfig {
            listen_addr: "0.0.0.0:18708".to_string(),
            token: Some("test-token".to_string()),
            ssh_authorized_keys: None,
            ssh_idle_timeout_secs: 3600,
            ssh_enabled: true,
            ssh_password_auth: false,
            peek_timeout_secs: 5,
            data_dir: std::path::PathBuf::from("/tmp/ShellAnyWhere"),
            keep_alive_interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(5),
            cert_file: std::path::PathBuf::from("/tmp/ShellAnyWhere/server.crt"),
            key_file: std::path::PathBuf::from("/tmp/ShellAnyWhere/server.key"),
            webrtc_public_ip: String::new(),
            web_dir: None,
        };
        assert_eq!(config.listen_addr, "0.0.0.0:18708");
    }
}
