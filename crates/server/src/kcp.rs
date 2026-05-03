use crate::relay;
use crate::session_mgr::SharedSessionManager;
use crate::ssh::SharedAuthorizedKeys;
use crate::transport::{ControlTransport, client_attach, client_loop};
use async_trait::async_trait;
use bytes::Bytes;
use saw_core::crypto::auth::AuthKey;
use saw_core::protocol::control::Control;
use saw_core::protocol::kcp_transport::{self, stream_id};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── KcpControlTransport ────────────────────────────────────────────────────
// Uses length-prefixed framing on the control KCP virtual stream:
//   [4-byte BE len][bincode-encoded Control]
//
// Terminal I/O flows on a separate stream (TERMINAL_IO) as raw bytes.

/// Read exactly `n` bytes from the stream, or return error on EOF.
async fn read_exact(
    stream: &mut kcp_transport::KcpVirtualStream,
    n: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut offset = 0;
    while offset < n {
        match stream.read(&mut buf[offset..]).await {
            Ok(0) => anyhow::bail!("KCP stream closed while reading"),
            Ok(read_n) => offset += read_n,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(buf)
}

struct KcpControlTransport {
    stream: kcp_transport::KcpVirtualStream,
}

impl KcpControlTransport {
    fn new(stream: kcp_transport::KcpVirtualStream) -> Self {
        Self { stream }
    }
}

#[async_trait]
impl ControlTransport for KcpControlTransport {
    async fn send_control(&mut self, ctrl: &Control) -> anyhow::Result<()> {
        let data = ctrl.encode()?;
        log::debug!(
            "KCP send_control: {} bytes, variant={:?}",
            data.len(),
            std::mem::discriminant(ctrl)
        );
        let len = data.len() as u32;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&data).await?;
        Ok(())
    }

    async fn recv_control(&mut self) -> anyhow::Result<Option<Control>> {
        let len_buf = match read_exact(&mut self.stream, 4).await {
            Ok(buf) => buf,
            Err(_) => return Ok(None),
        };
        let len = u32::from_be_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;
        if len > 16 * 1024 * 1024 {
            anyhow::bail!("KCP control message too large: {} bytes", len);
        }
        let data = read_exact(&mut self.stream, len).await?;
        let ctrl = Control::decode(&data)?;
        Ok(Some(ctrl))
    }

    async fn send_outgoing(&mut self, data: Bytes) -> anyhow::Result<()> {
        // Pre-encoded control bytes from forward_to_client — add length prefix
        let len = data.len() as u32;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&data).await?;
        Ok(())
    }
}

// ── KCP connection handling ────────────────────────────────────────────────

/// Handle a new KCP connection.
pub async fn handle_kcp_connection<T: kcp_transport::Transport + Send + 'static>(
    mut mux: kcp_transport::KcpMultiplex<T>,
    session_mgr: SharedSessionManager,
    auth_key: Option<AuthKey>,
    shared_authorized_keys: SharedAuthorizedKeys,
) {
    // Open both virtual streams BEFORE spawning mux.run()
    let mut control_stream = mux.open_stream(stream_id::CONTROL);
    let term_stream = mux.open_stream(stream_id::TERMINAL_IO);

    // Spawn the mux run loop BEFORE any I/O — it must be running to actually
    // send/receive frames on the KCP connection.
    tokio::spawn(async move {
        if let Err(e) = mux.run().await {
            log::debug!("KCP mux run ended: {}", e);
        }
    });

    let role = match authenticate_kcp(&mut control_stream, &auth_key).await {
        Ok(r) => r,
        Err(e) => {
            log::debug!("KCP auth failed: {}", e);
            return;
        }
    };
    log::info!("KCP authenticated as {:?}", role);

    match role {
        KcpRole::Agent => {
            handle_kcp_agent(
                control_stream,
                term_stream,
                session_mgr,
                &shared_authorized_keys,
            )
            .await
        }
        KcpRole::Client => handle_kcp_client(control_stream, term_stream, session_mgr).await,
    }
}

#[derive(Debug, PartialEq)]
enum KcpRole {
    Agent,
    Client,
}

/// Auth: [0x01=agent|0x02=client][token_len:u16][token_bytes] → [0x00=ok|0x01=fail]
async fn authenticate_kcp(
    control: &mut kcp_transport::KcpVirtualStream,
    auth_key: &Option<AuthKey>,
) -> anyhow::Result<KcpRole> {
    let mut buf = [0u8; 1024];
    let n = control
        .read(&mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("read: {}", e))?;
    if n < 3 {
        return Err(anyhow::anyhow!("too short"));
    }
    let role = match buf[0] {
        0x01 => KcpRole::Agent,
        0x02 => KcpRole::Client,
        b => return Err(anyhow::anyhow!("unknown role: {}", b)),
    };
    let tlen = u16::from_be_bytes([buf[1], buf[2]]) as usize;
    if n < 3 + tlen {
        return Err(anyhow::anyhow!("truncated"));
    }
    let token = std::str::from_utf8(&buf[3..3 + tlen])?;
    if let Some(key) = auth_key {
        let candidate = AuthKey::derive(token);
        if !key.ct_eq(&candidate) {
            let _ = control.write_all(&[0x01]).await;
            return Err(anyhow::anyhow!("bad token"));
        }
    }
    let _ = control.write_all(&[0x00]).await;
    Ok(role)
}

/// Handle KCP agent: read control + terminal output, forward via shared logic.
async fn handle_kcp_agent(
    control_stream: kcp_transport::KcpVirtualStream,
    term_stream: kcp_transport::KcpVirtualStream,
    session_mgr: SharedSessionManager,
    shared_authorized_keys: &SharedAuthorizedKeys,
) {
    let mut transport = KcpControlTransport::new(control_stream);
    let mut term_stream = term_stream;

    // First message must be SessionRegister
    let (session_id, generation, mut agent_rx) = match transport.recv_control().await {
        Ok(Some(Control::SessionRegister {
            session,
            ssh_public_keys,
        })) => {
            let sid = session.session_id.clone();
            let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<Control>(256);
            let (_, generation) = session_mgr.register_session(session, agent_tx);
            crate::listener::add_ssh_keys(&ssh_public_keys, shared_authorized_keys);
            log::info!("KCP agent session registered: {} (gen={})", sid, generation);
            (sid, generation, agent_rx)
        }
        Ok(other) => {
            log::debug!("KCP agent: expected SessionRegister, got {:?}", other);
            return;
        }
        Err(e) => {
            log::debug!("KCP agent: recv error: {}", e);
            return;
        }
    };

    // Create broadcast channel for terminal output and input channel for client keystrokes
    let (term_tx, _) = tokio::sync::broadcast::channel::<Bytes>(512);
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    session_mgr.set_kcp_term_channels(&session_id, term_tx.clone(), input_tx);

    let mut event_rx = session_mgr.subscribe();
    let mut term_buf = vec![0u8; 65536];

    // Agent loop: handle control + terminal I/O
    loop {
        tokio::select! {
            // Control from agent
            ctrl_result = transport.recv_control() => {
                match ctrl_result {
                    Ok(Some(ctrl)) => {
                        let is_close = matches!(ctrl, Control::SessionClose { .. });
                        relay::handle_agent_frame(&session_mgr, &session_id, ctrl);
                        if is_close {
                            log::info!("KCP agent {} closed session", session_id);
                            break;
                        }
                    }
                    Ok(None) => {
                        log::info!("KCP agent {} disconnected", session_id);
                        break;
                    }
                    Err(e) => {
                        log::debug!("KCP agent {} connection lost: {}", session_id, e);
                        break;
                    }
                }
            }
            // Control to agent (from session_mgr)
            ctrl = agent_rx.recv() => {
                match ctrl {
                    Some(ctrl) => {
                        log::info!("KCP agent {}: forwarding control to shell: {:?}", session_id, std::mem::discriminant(&ctrl));
                        if let Err(e) = transport.send_control(&ctrl).await {
                            log::debug!("KCP agent forward error: {}", e);
                            break;
                        }
                    }
                    None => break,
                }
            }
            // Terminal output from agent → broadcast to clients
            n = term_stream.read(&mut term_buf) => {
                match n {
                    Ok(0) => {
                        log::info!("KCP agent {} term stream closed", session_id);
                        break;
                    }
                    Ok(n) => {
                        // Bytes::from(Vec) avoids the extra copy that copy_from_slice does.
                        // The Vec allocation is moved into Bytes, sharing the same backing memory.
                        let data = Bytes::from(term_buf[..n].to_vec());
                        let _ = term_tx.send(data);
                    }
                    Err(e) => {
                        log::debug!("KCP agent {} term read error: {}", session_id, e);
                        break;
                    }
                }
            }
            // Terminal input from clients → agent's PTY
            data = input_rx.recv() => {
                match data {
                    Some(data) => {
                        if let Err(e) = term_stream.write_all(&data).await {
                            log::debug!("KCP agent {} term write error: {}", session_id, e);
                            break;
                        }
                        let _ = term_stream.flush().await;
                    }
                    None => break,
                }
            }
            // Broadcast events
            event = event_rx.recv() => {
                match event {
                    Ok(ctrl) => {
                        if let Err(e) = transport.send_control(&ctrl).await {
                            log::debug!("KCP agent event send error: {}", e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("KCP agent broadcast lagged by {}", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    session_mgr.unregister_session(&session_id, generation);
    log::info!(
        "KCP agent disconnected: {} (gen={})",
        session_id,
        generation
    );
}

/// Handle KCP client: attach to session, relay control + terminal data.
async fn handle_kcp_client(
    control_stream: kcp_transport::KcpVirtualStream,
    term_stream: kcp_transport::KcpVirtualStream,
    session_mgr: SharedSessionManager,
) {
    let mut transport = KcpControlTransport::new(control_stream);
    let client_id = format!("kcp-{}", nanoid::nanoid!(8));

    // Read control messages — handle SessionList with push updates, then wait for SessionAttach
    let (session_id, mode, previous_client_id) = {
        let mut event_rx = session_mgr.subscribe();
        let mut list_sent = false;
        loop {
            tokio::select! {
                ctrl_result = transport.recv_control() => {
                    match ctrl_result {
                        Ok(Some(Control::SessionAttach { session_id, mode, previous_client_id })) => {
                            break (session_id, mode, previous_client_id);
                        }
                        Ok(Some(Control::SessionList { .. })) => {
                            let sessions = session_mgr.list_sessions();
                            if let Err(e) = transport.send_control(&Control::SessionList { sessions }).await {
                                log::debug!("KCP client session list send error: {}", e);
                                return;
                            }
                            list_sent = true;
                        }
                        Ok(other) => {
                            log::debug!("KCP client: expected SessionAttach/SessionList, got {:?}", other);
                            return;
                        }
                        Err(e) => {
                            log::debug!("KCP client: recv error: {}", e);
                            return;
                        }
                    }
                }
                // Push session list updates after initial request
                result = event_rx.recv() => {
                    if !list_sent { continue; }
                    match result {
                        Ok(_) => {
                            let sessions = session_mgr.list_sessions();
                            if transport.send_control(&Control::SessionList { sessions }).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    };

    // Use shared client_attach logic
    let attach_result = client_attach(
        &mut transport,
        &session_mgr,
        &session_id,
        mode,
        &client_id,
        &previous_client_id,
    )
    .await;

    match attach_result {
        Ok(Some((effective_client_id, mut client_rx))) => {
            // Spawn terminal I/O relay task
            let term_relay = tokio::spawn(kcp_client_term_relay(
                term_stream,
                session_mgr.clone(),
                session_id.clone(),
                effective_client_id.clone(),
            ));

            // Run control loop (handles Control messages, outgoing bytes, events, keepalive)
            let mut event_rx = session_mgr.subscribe();
            let _ = client_loop(
                &mut transport,
                &session_mgr,
                &session_id,
                &effective_client_id,
                &mut client_rx,
                &mut event_rx,
            )
            .await;

            // Cancel terminal relay
            term_relay.abort();

            log::info!("KCP client {} disconnected", effective_client_id);
        }
        Ok(None) => {
            log::warn!(
                "KCP client {} failed to attach to session {}",
                client_id,
                session_id
            );
        }
        Err(e) => {
            log::debug!("KCP client attach error: {}", e);
        }
    }
}

/// Terminal I/O relay for KCP clients.
/// Reads terminal output from session broadcast and writes to client's term_stream.
/// Reads terminal input from client's term_stream and forwards to agent via session_mgr.
async fn kcp_client_term_relay(
    mut term_stream: kcp_transport::KcpVirtualStream,
    session_mgr: SharedSessionManager,
    session_id: String,
    client_id: String,
) {
    let mut term_output_rx = match session_mgr.subscribe_kcp_term_output(&session_id) {
        Some(rx) => rx,
        None => {
            log::debug!(
                "KCP client {}: no KCP term output channel for session {}",
                client_id,
                session_id
            );
            return;
        }
    };
    let mut term_buf = vec![0u8; 65536];

    loop {
        tokio::select! {
            // Terminal output from agent → client
            result = term_output_rx.recv() => {
                match result {
                    Ok(data) => {
                        if let Err(e) = term_stream.write_all(&data).await {
                            log::debug!("KCP client {} term output write error: {}", client_id, e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::debug!("KCP client {} term output lagged by {}", client_id, n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // Terminal input from client → agent
            n = term_stream.read(&mut term_buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = Bytes::from(term_buf[..n].to_vec());
                        if !session_mgr.forward_kcp_term_input(&session_id, data) {
                            log::debug!("KCP client {}: failed to forward term input", client_id);
                        }
                    }
                    Err(e) => {
                        log::debug!("KCP client {} term input read error: {}", client_id, e);
                        break;
                    }
                }
            }
        }
    }
    log::debug!("KCP client {} term relay ended", client_id);
}

/// Start the KCP listener using a custom transport (e.g., from UdpMux for port sharing).
pub async fn run_kcp_listener_with_transport(
    transport: crate::udp_mux::MuxTransport,
    session_mgr: SharedSessionManager,
    auth_key: Option<AuthKey>,
    shared_authorized_keys: SharedAuthorizedKeys,
) -> anyhow::Result<()> {
    let config = kcp_transport::low_latency_kcp_config();
    let mut listener =
        kcp_transport::KcpListener::with_transport(std::sync::Arc::new(transport), config).await?;
    log::info!("KCP listener started (shared port via UdpMux)");
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                log::info!("KCP connection from {}", peer_addr);
                let mux = kcp_transport::KcpMultiplex::new(stream);
                let sm = session_mgr.clone();
                let ak = auth_key.clone();
                let sak = shared_authorized_keys.clone();
                tokio::spawn(handle_kcp_connection(mux, sm, ak, sak));
            }
            Err(e) => log::warn!("KCP accept error: {}", e),
        }
    }
}
