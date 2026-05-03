use bytes::Bytes;
use dashmap::DashMap;
use saw_core::protocol::control::{AttachMode, Control, SessionInfo};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Handle for sending typed WebSocket messages to a browser client.
/// Replaces QmuxSessionHandle — native WebSocket has no stream multiplexing,
/// so all data (control + TerminalIO) flows through a single message channel.
#[derive(Clone)]
pub struct WsClientHandle {
    /// Channel for outbound WS messages (type prefix + payload).
    pub msg_tx: mpsc::Sender<Vec<u8>>,
}

/// Channel type for attached clients.
/// Bytes: pre-encoded wire bytes for network clients (TCP/WT), avoiding re-serialization.
/// Control: Control objects for local consumers (SSH handler), avoiding re-deserialization.
#[derive(Debug, Clone)]
pub enum ClientChannel {
    Bytes(mpsc::Sender<Bytes>),
    Control(mpsc::Sender<Control>),
}

/// A client attached to a session
#[derive(Debug)]
pub struct AttachedClient {
    pub client_id: String,
    pub mode: AttachMode,
    pub channel: ClientChannel,
    /// Last known terminal size reported by this client.
    pub cols: u16,
    pub rows: u16,
    /// True when a TerminalIO bidi stream relay is active for this client.
    /// When set, the server skips forwarding Output frames
    /// to this client (those arrive through the TerminalIO stream instead).
    pub terminal_io_active: bool,
}

/// An agent session registered with the server
pub struct AgentSession {
    pub info: SessionInfo,
    pub attached_clients: HashMap<String, AttachedClient>,
    pub output_sender: mpsc::Sender<Control>,
    /// The currently active client ID. Updated on attach, resize, or input.
    pub active_client_id: Option<String>,
    /// Agent's WebSocket handle for TerminalIO relay to browser clients.
    pub ws_handle: Option<WsClientHandle>,
    /// KCP terminal output broadcast: agent writes PTY output here,
    /// all attached KCP clients subscribe and relay to their term_streams.
    pub kcp_term_tx: Option<broadcast::Sender<Bytes>>,
    /// KCP terminal input channel: clients write keystrokes here,
    /// agent handler reads and forwards to the PTY via term_stream.
    pub kcp_input_tx: Option<mpsc::Sender<Bytes>>,
    /// Generation counter incremented on each register_session call.
    /// Used to detect stale unregister calls from old agent connections.
    pub generation: u64,
}

impl std::fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSession")
            .field("info", &self.info)
            .field("active_client_id", &self.active_client_id)
            .field("attached_clients", &self.attached_clients.len())
            .finish()
    }
}

/// A client connection to the server
pub struct ClientConnection {
    pub id: String,
    /// Session this client is attached to (None if not attached).
    pub session_id: Option<String>,
    /// Attach mode (None if not attached).
    pub mode: Option<AttachMode>,
    /// Client's WebSocket handle for TerminalIO relay.
    pub ws_handle: Option<WsClientHandle>,
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConnection")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("mode", &self.mode)
            .finish()
    }
}

/// Manages all agent sessions and client connections.
/// Uses DashMap for fine-grained per-key locking — different sessions
/// never block each other, unlike the previous RwLock<SessionManager>.
pub struct SessionManager {
    sessions: DashMap<String, AgentSession>,
    clients: DashMap<String, ClientConnection>,
    /// Broadcast channel for session lifecycle events (register, close, update).
    /// All connected clients subscribe to this to get real-time session list updates.
    session_events_tx: broadcast::Sender<Control>,
    /// Monotonic generation counter for detecting stale unregister calls.
    next_generation: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("sessions", &self.sessions.len())
            .field("clients", &self.clients.len())
            .finish()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            sessions: DashMap::new(),
            clients: DashMap::new(),
            session_events_tx: tx,
            next_generation: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Subscribe to session lifecycle events (SessionRegister, SessionClose, SessionUpdate).
    /// Clients use this to receive real-time session list updates without polling.
    pub fn subscribe(&self) -> broadcast::Receiver<Control> {
        self.session_events_tx.subscribe()
    }

    /// Register an agent session. Returns (session_id, generation).
    /// If a session with the same ID already exists (agent reconnected),
    /// the old session's attached clients are notified with SessionClose
    /// so they can re-attach to the new session.
    pub fn register_session(
        &self,
        info: SessionInfo,
        sender: mpsc::Sender<Control>,
    ) -> (String, u64) {
        let session_id = info.session_id.clone();
        let broadcast_info = info.clone();
        let generation = self
            .next_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = AgentSession {
            info,
            attached_clients: HashMap::new(),
            output_sender: sender,
            active_client_id: None,
            ws_handle: None,
            kcp_term_tx: None,
            kcp_input_tx: None,
            generation,
        };
        if let Some(old_session) = self.sessions.insert(session_id.clone(), session) {
            // Notify old session's clients that the session is being replaced.
            // Only send via Control channel — Bytes-channel clients will receive
            // SessionClose through the broadcast (session_events_tx) below.
            for ac in old_session.attached_clients.values() {
                if let ClientChannel::Control(tx) = &ac.channel {
                    let _ = tx.try_send(Control::SessionClose {
                        session_id: session_id.clone(),
                    });
                }
            }
            // Remove old client connections
            for ac in old_session.attached_clients.values() {
                self.clients.remove(&ac.client_id);
            }
            log::info!("Session {} re-registered (agent reconnected)", session_id);
        }

        // Broadcast SessionRegister to all connected clients (sanitized — no sensitive fields)
        let _ = self.session_events_tx.send(Control::SessionRegister {
            session: broadcast_info,
            ssh_public_keys: Vec::new(),
        });

        (session_id, generation)
    }

    /// Unregister an agent session, detaching all connected clients.
    /// If the session was re-registered (agent reconnected), the current
    /// entry belongs to the new connection — skip removal to avoid killing
    /// the re-registered session.
    pub fn unregister_session(&self, session_id: &str, generation: u64) {
        self.do_unregister(session_id, Some(generation))
    }

    /// Forcefully unregister a session regardless of generation.
    /// Used when the agent explicitly sends SessionClose.
    pub fn force_unregister_session(&self, session_id: &str) {
        self.do_unregister(session_id, None)
    }

    fn do_unregister(&self, session_id: &str, expected_generation: Option<u64>) {
        if let Some(expected) = expected_generation {
            if let Some(session) = self.sessions.get(session_id) {
                if session.generation != expected {
                    log::info!(
                        "Session {} not removed: generation mismatch (old={}, current={}), agent re-registered",
                        session_id,
                        expected,
                        session.generation
                    );
                    return;
                }
            } else {
                return;
            }
        }
        if let Some((_, session)) = self.sessions.remove(session_id) {
            let close_ctrl = Control::SessionClose {
                session_id: session_id.to_string(),
            };
            // Send SessionClose only via broadcast (event_rx).
            // Bytes-channel clients also subscribe to the broadcast, so
            // sending through both channels would duplicate the frame.
            for ac in session.attached_clients.values() {
                // Only send to Control-channel clients that don't use broadcast
                if let ClientChannel::Control(tx) = &ac.channel {
                    let _ = tx.try_send(close_ctrl.clone());
                }
            }
            // Broadcast SessionClose to all connected clients (covers Bytes + Control)
            let _ = self.session_events_tx.send(close_ctrl);
            log::info!("Session unregistered: {}", session_id);
        }
    }

    /// List all registered sessions.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.sessions.iter().map(|r| r.info.clone()).collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        list
    }

    /// Update session info (from agent SessionUpdate control).
    pub fn update_session(&self, info: &SessionInfo) {
        if let Some(mut session) = self.sessions.get_mut(&info.session_id) {
            log::info!(
                "SessionUpdate: {} cwd={} title={}",
                info.session_id,
                info.cwd,
                info.title
            );
            session.info = info.clone();
            // Broadcast SessionUpdate to all connected clients
            let n = self.session_events_tx.send(Control::SessionUpdate {
                session: info.clone(),
            });
            log::info!("SessionUpdate broadcast: {} receivers", n.unwrap_or(0));
        } else {
            log::warn!("SessionUpdate for unknown session {}", info.session_id);
        }
    }

    /// Set a custom title for a session (from client ClientSetTitle).
    /// Updates session.info.title and broadcasts SessionUpdate to all clients.
    pub fn set_session_title(&self, session_id: &str, title: &str) {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.info.title = title.to_string();
            let info = session.info.clone();
            log::info!("ClientSetTitle: session {} title={:?}", session_id, title);
            let n = self
                .session_events_tx
                .send(Control::SessionUpdate { session: info });
            log::info!(
                "SessionUpdate broadcast (title): {} receivers",
                n.unwrap_or(0)
            );
        } else {
            log::warn!("ClientSetTitle for unknown session {}", session_id);
        }
    }

    /// Attach a client to a session. Returns None if session not found.
    pub fn attach_client(
        &self,
        session_id: &str,
        client_id: String,
        mode: AttachMode,
        sender: mpsc::Sender<Bytes>,
    ) -> Option<Vec<Control>> {
        self.attach_client_internal(session_id, client_id, mode, ClientChannel::Bytes(sender))
    }

    /// Attach a client that receives Control objects directly (no re-serialization).
    /// Used by SSH handler and other local consumers that need Control access.
    pub fn attach_client_control(
        &self,
        session_id: &str,
        client_id: String,
        mode: AttachMode,
        sender: mpsc::Sender<Control>,
    ) -> Option<Vec<Control>> {
        self.attach_client_internal(session_id, client_id, mode, ClientChannel::Control(sender))
    }

    fn attach_client_internal(
        &self,
        session_id: &str,
        client_id: String,
        mode: AttachMode,
        channel: ClientChannel,
    ) -> Option<Vec<Control>> {
        // Register client if not already registered, or update session info
        self.clients
            .entry(client_id.clone())
            .and_modify(|c| {
                c.session_id = Some(session_id.to_string());
                c.mode = Some(mode);
            })
            .or_insert_with(|| ClientConnection {
                id: client_id.clone(),
                session_id: Some(session_id.to_string()),
                mode: Some(mode),
                ws_handle: None,
            });

        let mut session = self.sessions.get_mut(session_id)?;
        let init_cols = session.info.cols;
        let init_rows = session.info.rows;
        session.attached_clients.insert(
            client_id.clone(),
            AttachedClient {
                client_id: client_id.clone(),
                mode,
                channel: channel.clone(),
                cols: init_cols,
                rows: init_rows,
                terminal_io_active: false,
            },
        );
        log::info!(
            "Client {} attached to session {} (mode: {:?})",
            client_id,
            session_id,
            mode
        );

        // New client automatically becomes active (unless observe mode)
        if mode != AttachMode::Observe {
            session.active_client_id = Some(client_id.clone());
            // Use current session size (client hasn't sent resize yet)
            let cols = session.info.cols;
            let rows = session.info.rows;
            // Send ClientAttached to agent (triggers on_client_attached, replay).
            // This is the unified path for both regular and SSH clients.
            let _ = session.output_sender.try_send(Control::ClientAttached {
                session_id: session_id.to_string(),
                client_id: client_id.clone(),
                mode,
            });
            // Broadcast ClientActive to all attached Bytes-channel clients
            let active_ctrl = Control::ClientActive {
                session_id: session_id.to_string(),
                client_id: client_id.clone(),
                cols,
                rows,
            };
            if let Ok(data) = active_ctrl.encode() {
                let encoded = Bytes::from(data);
                for ac in session.attached_clients.values() {
                    if let ClientChannel::Bytes(tx) = &ac.channel {
                        let _ = tx.try_send(encoded.clone());
                    }
                }
            }
        }

        // No cached replay frames for now; return empty vec
        Some(vec![])
    }

    /// Detach a client from a session.
    pub fn detach_client(&self, session_id: &str, client_id: &str) {
        if let Some(mut session) = self.sessions.get_mut(session_id)
            && session.attached_clients.remove(client_id).is_some()
        {
            log::info!("Client {} detached from session {}", client_id, session_id);
            // Notify agent that this client has detached
            let _ = session.output_sender.try_send(Control::SessionDetach {
                session_id: session_id.to_string(),
                client_id: client_id.to_string(),
            });
            // If the detached client was active, clear active status
            if session.active_client_id.as_deref() == Some(client_id) {
                session.active_client_id = None;
            }
        }
        // Clear client session info (keep connection alive)
        if let Some(mut client) = self.clients.get_mut(client_id) {
            client.session_id = None;
            client.mode = None;
        }
    }

    /// Set the active client for a session and broadcast ClientActive to
    /// agent and all attached clients. Immediately updates last_activity_at
    /// and session size so the session data is consistent even if other
    /// messages arrive between the event and the next SessionUpdate.
    /// Broadcasts when either the active client changes OR the size differs
    /// from the session's previous size, so that all clients stay in sync
    /// with the shell's current terminal dimensions.
    /// Returns (should_broadcast, size_changed).
    pub fn set_active_client(
        &self,
        session_id: &str,
        client_id: &str,
        cols: u16,
        rows: u16,
    ) -> (bool, bool) {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            let client_changed = session.active_client_id.as_deref() != Some(client_id);
            let size_changed =
                cols > 0 && rows > 0 && (session.info.cols != cols || session.info.rows != rows);
            session.active_client_id = Some(client_id.to_string());
            // Update last_activity_at immediately — no async delay.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            session.info.last_activity_at = now;
            // Update session size immediately from the active client's terminal.
            // This ensures cols/rows are consistent with the active client's view.
            if cols > 0 && rows > 0 {
                session.info.cols = cols;
                session.info.rows = rows;
            }
            let should_broadcast = client_changed || size_changed;
            log::debug!(
                "Active client for session {} set to {} ({}x{}) client_changed={} size_changed={}",
                session_id,
                client_id,
                cols,
                rows,
                client_changed,
                size_changed
            );
            // Broadcast ClientActive when the active client or size changed.
            // last_activity_at updates above still happen unconditionally.
            if should_broadcast {
                // Broadcast ClientActive to agent
                let _ = session.output_sender.try_send(Control::ClientActive {
                    session_id: session_id.to_string(),
                    client_id: client_id.to_string(),
                    cols,
                    rows,
                });
                // Broadcast ClientActive to all attached clients EXCEPT the source.
                // The source already knows it's active (it just sent the resize),
                // so echoing back would cause an unnecessary refresh.
                let active_ctrl = Control::ClientActive {
                    session_id: session_id.to_string(),
                    client_id: client_id.to_string(),
                    cols,
                    rows,
                };
                if let Ok(data) = active_ctrl.encode() {
                    let encoded = Bytes::from(data);
                    for ac in session.attached_clients.values() {
                        if ac.client_id == client_id {
                            continue;
                        }
                        if let ClientChannel::Bytes(tx) = &ac.channel {
                            let _ = tx.try_send(encoded.clone());
                        }
                    }
                }
            }
            (should_broadcast, size_changed)
        } else {
            (false, false)
        }
    }

    /// Update a client's terminal size and return the new size.
    /// Returns (0, 0) if the client is not found in the session.
    pub fn update_client_size(&self, session_id: &str, client_id: &str, cols: u16, rows: u16) {
        if let Some(mut session) = self.sessions.get_mut(session_id)
            && let Some(ac) = session.attached_clients.get_mut(client_id)
        {
            ac.cols = cols;
            ac.rows = rows;
        }
    }

    /// Check if a client_id is currently connected (has an active connection).
    pub fn is_client_connected(&self, client_id: &str) -> bool {
        self.clients.contains_key(client_id)
    }

    /// Check if a client is in observe mode for a given session.
    /// O(1) lookup via clients DashMap instead of O(n) scan of attached_clients.
    pub fn is_observe_client(&self, session_id: &str, client_id: &str) -> bool {
        self.clients
            .get(client_id)
            .map(|c| {
                matches!(c.mode, Some(AttachMode::Observe))
                    && c.session_id.as_deref() == Some(session_id)
            })
            .unwrap_or(false)
    }

    /// Forward a control to all clients attached to a session.
    /// - Bytes clients: serializes ONCE, sends pre-encoded bytes (cheap ref-counted clone).
    /// - Control clients: sends the Control object directly (no serialization needed).
    /// - Returns (encoded_bytes, ok, dead_clients): encoded_bytes is the serialized control
    ///   (can be reused for upstream forwarding without re-serialization),
    ///   ok is true if at least one client received the control,
    ///   dead_clients contains client IDs whose channels are closed.
    pub fn forward_to_client(
        &self,
        session_id: &str,
        ctrl: Control,
    ) -> (Option<Bytes>, bool, Vec<String>) {
        // Encode eagerly — needed for upstream anyway, and avoids complex lazy init
        let encoded = match ctrl.encode() {
            Ok(data) => Bytes::from(data),
            Err(e) => {
                log::error!("Failed to encode control for broadcast: {}", e);
                return (None, false, Vec::new());
            }
        };

        if let Some(session) = self.sessions.get(session_id) {
            let mut any_ok = false;
            let mut dead_clients = Vec::new();
            for ac in session.attached_clients.values() {
                match &ac.channel {
                    ClientChannel::Bytes(tx) => {
                        if tx.is_closed() {
                            dead_clients.push(ac.client_id.clone());
                            continue;
                        }
                        match tx.try_send(encoded.clone()) {
                            Ok(()) => any_ok = true,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                log::warn!(
                                    "Dropping control for client {} (channel full)",
                                    ac.client_id
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                dead_clients.push(ac.client_id.clone());
                            }
                        }
                    }
                    ClientChannel::Control(tx) => {
                        if tx.is_closed() {
                            dead_clients.push(ac.client_id.clone());
                            continue;
                        }
                        match tx.try_send(ctrl.clone()) {
                            Ok(()) => any_ok = true,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                log::warn!(
                                    "Dropping control for client {} (channel full)",
                                    ac.client_id
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                dead_clients.push(ac.client_id.clone());
                            }
                        }
                    }
                }
            }
            (Some(encoded), any_ok, dead_clients)
        } else {
            (Some(encoded), false, Vec::new())
        }
    }

    /// Remove specific clients from a session by their IDs.
    /// Used to clean up clients whose channels are closed.
    pub fn remove_clients_by_id(&self, session_id: &str, client_ids: &[String]) {
        if client_ids.is_empty() {
            return;
        }
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            let mut removed = 0;
            for cid in client_ids {
                if session.attached_clients.remove(cid).is_some() {
                    removed += 1;
                }
            }
            if removed > 0 {
                log::info!(
                    "Cleaned up {} dead client(s) from session {}",
                    removed,
                    session_id
                );
            }
        }
        for cid in client_ids {
            self.clients.remove(cid);
        }
    }

    /// Forward a control to the agent for a session.
    pub fn forward_to_agent(&self, session_id: &str, ctrl: Control) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            match session.output_sender.try_send(ctrl) {
                Ok(()) => return true,
                Err(e) => {
                    log::warn!(
                        "Failed to forward control to agent for session {}: {}",
                        session_id,
                        e
                    );
                }
            }
        }
        false
    }

    /// Store a client's WebSocket handle for TerminalIO relay.
    pub fn set_client_ws_handle(&self, client_id: &str, handle: WsClientHandle) {
        self.clients
            .entry(client_id.to_string())
            .and_modify(|c| {
                log::debug!("Stored WS handle for client {}", client_id);
                c.ws_handle = Some(handle.clone());
            })
            .or_insert_with(|| {
                log::debug!(
                    "Created ClientConnection with WS handle for client {}",
                    client_id
                );
                ClientConnection {
                    id: client_id.to_string(),
                    session_id: None,
                    mode: None,
                    ws_handle: Some(handle),
                }
            });
    }

    /// Get a client's WebSocket handle.
    pub fn get_client_ws_handle(&self, client_id: &str) -> Option<WsClientHandle> {
        self.clients
            .get(client_id)
            .and_then(|c| c.ws_handle.clone())
    }

    /// Set whether a TerminalIO bidi stream relay is active for a client.
    /// When active, the server skips forwarding Output frames
    /// to this client (they arrive through the TerminalIO stream instead).
    pub fn set_terminal_io_active(&self, session_id: &str, client_id: &str, active: bool) {
        if let Some(mut session) = self.sessions.get_mut(session_id)
            && let Some(ac) = session.attached_clients.get_mut(client_id)
        {
            ac.terminal_io_active = active;
        }
    }

    // ── KCP terminal I/O channels ─────────────────────────────────────────

    /// Store KCP terminal channels for a session (called by KCP agent handler).
    pub fn set_kcp_term_channels(
        &self,
        session_id: &str,
        term_tx: broadcast::Sender<Bytes>,
        input_tx: mpsc::Sender<Bytes>,
    ) {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.kcp_term_tx = Some(term_tx);
            session.kcp_input_tx = Some(input_tx);
        }
    }

    /// Subscribe to a session's KCP terminal output broadcast.
    /// Returns None if the session doesn't exist or has no KCP channels.
    pub fn subscribe_kcp_term_output(
        &self,
        session_id: &str,
    ) -> Option<broadcast::Receiver<Bytes>> {
        self.sessions
            .get(session_id)
            .and_then(|s| s.kcp_term_tx.as_ref().map(|tx| tx.subscribe()))
    }

    /// Forward terminal input bytes to the agent's KCP input channel.
    /// Returns true if sent successfully.
    pub fn forward_kcp_term_input(&self, session_id: &str, data: Bytes) -> bool {
        if let Some(session) = self.sessions.get(session_id)
            && let Some(ref tx) = session.kcp_input_tx
        {
            match tx.try_send(data) {
                Ok(()) => return true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    log::warn!("KCP input channel full for session {}", session_id);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        false
    }

    /// Remove a client connection, detaching it from any session.
    /// O(1) lookup via client's session_id field instead of scanning all sessions.
    pub fn remove_client(&self, client_id: &str) {
        // Look up the client's session directly (O(1))
        let session_id = self
            .clients
            .get(client_id)
            .and_then(|c| c.session_id.clone());

        if let Some(ref sid) = session_id {
            // Remove from the specific session's attached_clients
            let detached = if let Some(mut session) = self.sessions.get_mut(sid) {
                let before = session.attached_clients.len();
                session
                    .attached_clients
                    .retain(|_, ac| ac.client_id != client_id);
                session.attached_clients.len() < before
            } else {
                false
            };
            if detached {
                log::info!(
                    "Client {} disconnected, detached from session {}",
                    client_id,
                    sid
                );
                // Release session lock before forwarding to avoid DashMap deadlock
                self.forward_to_agent(
                    sid,
                    Control::SessionDetach {
                        session_id: sid.clone(),
                        client_id: client_id.to_string(),
                    },
                );
            }
        }
        self.clients.remove(client_id);
    }
}

/// Thread-safe shared session manager type.
/// No longer needs RwLock — DashMap provides internal fine-grained locking.
pub type SharedSessionManager = Arc<SessionManager>;

/// Create a new shared session manager
pub fn shared_session_manager() -> SharedSessionManager {
    Arc::new(SessionManager::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session_info(id: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            shell: "/bin/zsh".to_string(),
            started_at: 1713400000,
            cols: 80,
            rows: 24,
            cwd: "/home/user".to_string(),
            first_command: None,
            terminal_program: None,
            last_activity_at: 1713400000,
            hostname: "myhost".to_string(),
            username: "user".to_string(),
            title: String::new(),
        }
    }

    #[test]
    fn test_register_and_list_sessions() {
        let mgr = SessionManager::new();
        let (tx, _rx) = mpsc::channel::<Control>(32);

        let info = make_session_info("s1");
        let (id, _gen) = mgr.register_session(info, tx);
        assert_eq!(id, "s1");

        let sessions = mgr.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
    }

    #[test]
    fn test_unregister_session() {
        let mgr = SessionManager::new();
        let (tx, _rx) = mpsc::channel::<Control>(32);

        let (_, generation) = mgr.register_session(make_session_info("s1"), tx);
        mgr.unregister_session("s1", generation);
        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn test_attach_and_detach_client() {
        let mgr = SessionManager::new();
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);

        let _ = mgr.register_session(make_session_info("s1"), agent_tx.clone());

        let (client_tx, _client_rx) = mpsc::channel::<Bytes>(32);
        let result = mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);
        assert!(result.is_some());

        // Session should have attached client
        let session = mgr.sessions.get("s1").unwrap();
        assert_eq!(session.attached_clients.len(), 1);
        assert_eq!(session.attached_clients.get("c1").unwrap().client_id, "c1");
        drop(session); // release the DashMap ref before mutating

        mgr.detach_client("s1", "c1");
        let session = mgr.sessions.get("s1").unwrap();
        assert!(session.attached_clients.is_empty());
    }

    #[test]
    fn test_multiple_clients() {
        let mgr = SessionManager::new();
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);

        let _ = mgr.register_session(make_session_info("s1"), agent_tx.clone());

        let (client1_tx, mut client1_rx) = mpsc::channel::<Bytes>(32);
        let (client2_tx, mut client2_rx) = mpsc::channel::<Bytes>(32);
        mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client1_tx);
        mgr.attach_client("s1", "c2".into(), AttachMode::Observe, client2_tx);

        // Drain ClientActive controls from attach
        while client1_rx.try_recv().is_ok() {}
        while client2_rx.try_recv().is_ok() {}

        // Both should receive encoded control
        let ctrl = Control::SessionUpdate {
            session: make_session_info("s1"),
        };
        let (_encoded, ok, _) = mgr.forward_to_client("s1", ctrl);
        assert!(ok);

        // Verify both received the same encoded bytes
        let recv1 = client1_rx.try_recv().unwrap();
        let recv2 = client2_rx.try_recv().unwrap();
        assert_eq!(recv1, recv2);
    }

    #[test]
    fn test_forward_to_client() {
        let mgr = SessionManager::new();
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);

        let _ = mgr.register_session(make_session_info("s1"), agent_tx.clone());

        let (client_tx, mut client_rx) = mpsc::channel::<Bytes>(32);
        mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);

        let ctrl = Control::SessionUpdate {
            session: make_session_info("s1"),
        };
        let (_encoded, ok, _) = mgr.forward_to_client("s1", ctrl);
        assert!(ok);

        // Verify received bytes can be decoded back
        let received = client_rx.try_recv().unwrap();
        assert!(!received.is_empty());
    }

    #[test]
    fn test_forward_to_agent() {
        let mgr = SessionManager::new();
        let (agent_tx, mut agent_rx) = mpsc::channel::<Control>(32);

        let _ = mgr.register_session(make_session_info("s1"), agent_tx.clone());

        let ctrl = Control::ClientResize {
            session_id: "s1".into(),
            client_id: "c1".into(),
            cols: 120,
            rows: 40,
        };
        let ok = mgr.forward_to_agent("s1", ctrl.clone());
        assert!(ok);

        let received = agent_rx.try_recv().unwrap();
        match received {
            Control::ClientResize { cols, rows, .. } => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            _ => panic!("Expected ClientResize"),
        }
    }

    #[test]
    fn test_remove_client() {
        let mgr = SessionManager::new();
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);

        let _ = mgr.register_session(make_session_info("s1"), agent_tx.clone());

        let (client_tx, _client_rx) = mpsc::channel::<Bytes>(32);
        mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);

        mgr.remove_client("c1");
        let session = mgr.sessions.get("s1").unwrap();
        assert!(session.attached_clients.is_empty());
        assert!(!mgr.clients.contains_key("c1"));
    }

    #[test]
    fn test_shared_session_manager() {
        let mgr = shared_session_manager();
        let (tx, _rx) = mpsc::channel::<Control>(32);

        mgr.register_session(make_session_info("s1"), tx);

        let sessions = mgr.list_sessions();
        assert_eq!(sessions.len(), 1);
    }
}
