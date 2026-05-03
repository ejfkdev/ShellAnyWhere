//! Server-side SSH server for accessing existing sessions.
//!
//! SSH connections attach to existing agent sessions (no new PTY is created).
//! Supports:
//! - Interactive session selection menu (shell_request)
//! - Direct session attach via exec_request: `ssh user@host <session_id>`
//! - Password auth (password = token) and public key auth (authorized_keys)
//! - Window resize forwarding
//!
//! # Data I/O migration note
//!
//! ShellInput/ShellOutput no longer exist as Control variants.
//! Terminal I/O data should be sent through a separate terminal_io ContextStream.
//! For now, SSH data forwarding uses a dedicated data channel until
//! full ContextStream integration is implemented.

use crate::session_mgr::SharedSessionManager;
use anyhow::Result;
use bytes::Bytes;
use saw_core::protocol::control::{AttachMode, Control, SessionInfo, format_session_menu};

use russh::keys::{self, PrivateKey, PublicKey};
use russh::server::{self, Server, Session};
use russh::{Channel, ChannelId};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, watch};

/// OS-backed random number generator for `rand_core` 0.10.
struct OsRng;

impl rand_core::TryRng for OsRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut buf = [0u8; 4];
        self.try_fill_bytes(&mut buf)?;
        Ok(u32::from_ne_bytes(buf))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut buf = [0u8; 8];
        self.try_fill_bytes(&mut buf)?;
        Ok(u64::from_ne_bytes(buf))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        getrandom::fill(dest).expect("getrandom failed");
        Ok(())
    }
}

impl rand_core::TryCryptoRng for OsRng {}

/// Shared, dynamically-updatable authorized_keys set.
/// Agent public keys are added when they register, allowing SSH clients
/// to authenticate with agent-token-derived private keys.
pub type SharedAuthorizedKeys = Arc<std::sync::Mutex<HashSet<PublicKey>>>;

/// SSH server configuration shared across all handlers.
pub struct SshServerConfig {
    /// Server token for SSH password authentication.
    /// When set, SSH password must match this token.
    pub token: Option<String>,
    pub authorized_keys: SharedAuthorizedKeys,
    pub session_mgr: SharedSessionManager,
    pub password_auth_enabled: bool,
}

/// Factory that creates a new SshHandler for each incoming SSH connection.
#[derive(Clone)]
pub struct SshServer {
    pub config: Arc<SshServerConfig>,
    id_counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl SshServer {
    pub fn new(config: SshServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            id_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        let id = self
            .id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SshHandler {
            id,
            config: self.config.clone(),
            channel_id: None,
            handle: None,
            pty_cols: 80,
            pty_rows: 24,
            state: SshState::Initial,
            cleaned_up: false,
        }
    }
}

/// State machine for SSH handler.
enum SshState {
    /// Waiting for shell_request or exec_request
    Initial,
    /// Showing session list, waiting for user to pick
    Selecting {
        sessions: Arc<Mutex<Vec<SessionInfo>>>,
        input_buf: String,
        /// Background task that watches for new sessions and refreshes the menu
        watcher: Option<tokio::task::JoinHandle<()>>,
    },
    /// Attached to a session, forwarding data
    Attached {
        session_id: String,
        client_id: String,
        /// Watch channel to detect when the background task receives SessionClose
        session_close_rx: watch::Receiver<bool>,
        /// Handle to the background output relay task, for cancellation on re-attach
        bg_task: tokio::task::JoinHandle<()>,
        /// Channel to forward SSH input bytes to the relay task
        term_input_tx: mpsc::Sender<Bytes>,
    },
}

/// Action to take after processing input in Selecting state.
enum SelectAction {
    /// No action needed (still collecting input)
    None,
    /// Try to attach to a session
    Attach(String),
}

pub struct SshHandler {
    id: usize,
    config: Arc<SshServerConfig>,
    channel_id: Option<ChannelId>,
    handle: Option<server::Handle>,
    pty_cols: u16,
    pty_rows: u16,
    state: SshState,
    cleaned_up: bool,
}

impl server::Handler for SshHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        // Password auth disabled — always reject
        if !self.config.password_auth_enabled {
            log::warn!(
                "[{}] SSH password auth rejected: password auth disabled",
                self.id
            );
            return Ok(server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }

        // Check server token
        if let Some(token_str) = &self.config.token
            && password == token_str
        {
            log::info!("[{}] SSH password auth accepted (server token)", self.id);
            return Ok(server::Auth::Accept);
        }

        // If no server token is configured, accept any password
        if self.config.token.is_none() {
            log::info!(
                "[{}] SSH password auth accepted (no token configured)",
                self.id
            );
            return Ok(server::Auth::Accept);
        }

        log::warn!("[{}] SSH password auth rejected: invalid token", self.id);
        Ok(server::Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let keys = self.config.authorized_keys.lock().unwrap();
        if keys.contains(public_key) {
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        // russh verifies the signature before calling this method.
        // We just need to confirm the key is still in authorized_keys.
        let keys = self.config.authorized_keys.lock().unwrap();
        if keys.contains(public_key) {
            log::info!("[{}] SSH public key auth accepted", self.id);
            Ok(server::Auth::Accept)
        } else {
            log::warn!(
                "[{}] SSH public key auth rejected (key not in authorized_keys)",
                self.id
            );
            Ok(server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<russh::server::Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let _ = channel;
        self.channel_id = Some(channel.id());
        self.handle = Some(session.handle());
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pty_cols = col_width.max(1) as u16;
        self.pty_rows = row_height.max(1) as u16;
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;

        // Auto-attach if only one session
        let sessions = self.config.session_mgr.list_sessions();
        if sessions.len() == 1 {
            let sid = sessions[0].session_id.clone();
            if self.try_attach(&sid, channel).await.is_ok() {
                return Ok(());
            }
        }

        if !self.show_session_menu(channel).await {
            self.cleanup().await;
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let session_id = String::from_utf8_lossy(data).trim().to_string();
        session.channel_success(channel)?;

        if session_id.is_empty() {
            if !self.show_session_menu(channel).await {
                self.cleanup().await;
            }
            return Ok(());
        }

        if self.try_attach(&session_id, channel).await.is_err() {
            // try_attach already set Selecting state and showed error message.
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pty_cols = col_width.max(1) as u16;
        self.pty_rows = row_height.max(1) as u16;

        if let SshState::Attached {
            session_id,
            client_id,
            ..
        } = &self.state
        {
            let ctrl = Control::ClientResize {
                session_id: session_id.clone(),
                client_id: client_id.clone(),
                cols: self.pty_cols,
                rows: self.pty_rows,
            };
            let sid = session_id.clone();
            self.config.session_mgr.forward_to_agent(&sid, ctrl);
        }
        session.channel_success(channel)?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Clone handle upfront to avoid borrow conflicts
        let handle = self.handle.clone();

        // Process Selecting state: collect input and determine action
        let action = match &mut self.state {
            SshState::Selecting {
                sessions,
                input_buf,
                ..
            } => {
                let sessions_snapshot = sessions.lock().await.clone();
                let mut action = SelectAction::None;
                for &b in data {
                    match b {
                        b'\r' | b'\n' => {
                            let choice = input_buf.trim().to_string();
                            input_buf.clear();
                            let _ = send_to_channel(&handle, channel, b"\r\n").await;

                            // Try as number
                            if let Ok(num) = choice.parse::<usize>()
                                && num >= 1
                                && num <= sessions_snapshot.len()
                            {
                                let sid = sessions_snapshot[num - 1].session_id.clone();
                                action = SelectAction::Attach(sid);
                                break;
                            }

                            // Try as session ID directly
                            if !choice.is_empty() {
                                action = SelectAction::Attach(choice);
                                break;
                            }

                            let _ =
                                send_to_channel(&handle, channel, b"Invalid choice.\r\n\r\n").await;
                            break;
                        }
                        0x7f | 0x08 => {
                            input_buf.pop();
                            let _ = send_to_channel(&handle, channel, &[0x08, b' ', 0x08]).await;
                        }
                        0x03 => {
                            // Ctrl-C
                            if let Some(h) = &handle {
                                let _ = h
                                    .disconnect(
                                        russh::Disconnect::ByApplication,
                                        "".into(),
                                        "".into(),
                                    )
                                    .await;
                            }
                        }
                        b if (0x20..0x7f).contains(&b) => {
                            input_buf.push(b as char);
                            let _ = send_to_channel(&handle, channel, &[b]).await;
                        }
                        _ => {}
                    }
                }
                action
            }
            _ => SelectAction::None,
        };

        // Execute action outside the mutable borrow
        match action {
            SelectAction::Attach(sid) => {
                if self.try_attach(&sid, channel).await.is_err() {
                    // try_attach already set Selecting state and showed error message.
                }
            }
            SelectAction::None => {}
        }

        // Process Attached state: forward input to relay task
        if let SshState::Attached {
            session_close_rx,
            term_input_tx,
            ..
        } = &mut self.state
        {
            // Check if the background task detected SessionClose
            // (bg_task already sent "Session closed." + menu via handle.data())
            if *session_close_rx.borrow() {
                self.cleanup().await;
                // Show session menu (starts watcher, auto-attaches if 1 session)
                if !self.show_session_menu(channel).await {
                    self.cleanup().await;
                }
                return Ok(());
            }

            // Forward input data through terminal_io relay
            if !data.is_empty() {
                let _ = term_input_tx.send(Bytes::copy_from_slice(data)).await;
            }
        }

        Ok(())
    }

    async fn signal(
        &mut self,
        _channel: ChannelId,
        signal: russh::Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // TODO: Signal forwarding should go through terminal_io ContextStream.
        // ShellInput no longer exists as a Control variant.
        if let SshState::Attached { session_id, .. } = &self.state {
            log::debug!(
                "[{}] SSH signal {:?} for session {} dropped — \
                 terminal_io ContextStream not yet implemented",
                self.id,
                signal,
                session_id,
            );
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.cleanup().await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.cleanup().await;
        Ok(())
    }
}

impl SshHandler {
    /// Show the session selection menu.
    /// Returns false if no sessions are available (caller should disconnect).
    async fn show_session_menu(&mut self, channel: ChannelId) -> bool {
        // Abort previous watcher if re-entering Selecting
        if let SshState::Selecting { watcher, .. } = &mut self.state
            && let Some(w) = watcher.take()
        {
            w.abort();
        }

        let sessions = self.config.session_mgr.list_sessions();

        // Auto-attach when there's exactly one session
        if sessions.len() == 1 {
            let sid = sessions[0].session_id.clone();
            let output = format_session_menu(&sessions, "");
            let _ = self.send_str(channel, &output).await;
            let _ = self.send_str(channel, "Auto-attaching...\r\n").await;
            return self.try_attach(&sid, channel).await.is_ok();
        }

        let shared_sessions = Arc::new(Mutex::new(sessions.clone()));

        let output = format_session_menu(&sessions, "");
        let _ = self.send_str(channel, &output).await;

        let watcher = self.spawn_session_watcher(channel, &shared_sessions);

        self.state = SshState::Selecting {
            sessions: shared_sessions,
            input_buf: String::new(),
            watcher,
        };
        true
    }

    /// Spawn a background task that refreshes the session menu when sessions change.
    fn spawn_session_watcher(
        &self,
        channel: ChannelId,
        shared: &Arc<Mutex<Vec<SessionInfo>>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let handle = self.handle.clone()?;
        let session_mgr = self.config.session_mgr.clone();
        let shared = shared.clone();
        let ssh_id = self.id;
        Some(tokio::spawn(async move {
            let mut event_rx = session_mgr.subscribe();
            loop {
                match event_rx.recv().await {
                    Ok(_) => {
                        let new_sessions = session_mgr.list_sessions();
                        let mut current = shared.lock().await;
                        let changed = current.len() != new_sessions.len()
                            || current
                                .iter()
                                .zip(new_sessions.iter())
                                .any(|(a, b)| a.session_id != b.session_id);
                        if changed {
                            *current = new_sessions.clone();
                            drop(current);
                            let output = format_session_menu(&new_sessions, "");
                            let _ = handle.data(channel, output.as_bytes().to_vec()).await;
                            log::debug!("[{}] SSH session menu refreshed", ssh_id);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }))
    }

    /// Try to attach to a session by ID.
    /// Returns Err(()) if session not found.
    async fn try_attach(
        &mut self,
        session_id: &str,
        channel: ChannelId,
    ) -> std::result::Result<(), ()> {
        let sessions = self.config.session_mgr.list_sessions();
        let session_info = sessions.iter().find(|s| s.session_id == session_id);
        if session_info.is_none() {
            let _ = self
                .send_str(
                    channel,
                    "\r\nSession not found.\r\n\
                 Enter session number or ID: ",
                )
                .await;
            self.state = SshState::Selecting {
                sessions: Arc::new(Mutex::new(sessions)),
                input_buf: String::new(),
                watcher: None,
            };
            return Err(());
        }

        let client_id = format!("ssh-{}", self.id);
        let (client_tx, client_rx) = tokio::sync::mpsc::channel::<Control>(256);

        let attached = {
            self.config.session_mgr.attach_client_control(
                session_id,
                client_id.clone(),
                AttachMode::Interact,
                client_tx,
            )
        };

        if attached.is_some() {
            // Cancel any existing background tasks from previous state
            match &mut self.state {
                SshState::Attached { bg_task, .. } => bg_task.abort(),
                SshState::Selecting { watcher, .. } => {
                    if let Some(w) = watcher.take() {
                        w.abort();
                    }
                }
                _ => {}
            }

            // Send initial resize
            let resize_ctrl = Control::ClientResize {
                session_id: session_id.to_string(),
                client_id: client_id.clone(),
                cols: self.pty_cols,
                rows: self.pty_rows,
            };
            {
                self.config
                    .session_mgr
                    .forward_to_agent(session_id, resize_ctrl);
            }

            let msg = format!("Attached to session {}\r\n", session_id);
            let _ = self.send_str(channel, &msg).await;

            // Clear screen before relay starts so replay buffer renders cleanly
            let _ = self.send_str(channel, "\x1b[2J\x1b[H").await;

            log::info!("[{}] SSH attached to session {}", self.id, session_id);

            // Mark terminal_io active so control-channel Output frames are skipped
            self.config
                .session_mgr
                .set_terminal_io_active(session_id, &client_id, true);

            let client_rx = Arc::new(Mutex::new(client_rx));
            let (session_close_tx, session_close_rx) = watch::channel(false);

            // Create input channel: SSH data() → relay task → agent
            let (term_input_tx, term_input_rx) = mpsc::channel::<Bytes>(256);

            // Try WT relay first, then KCP relay
            let session_mgr = self.config.session_mgr.clone();
            let ssh_handle = self.handle.clone();
            let sid = session_id.to_string();
            let cid = client_id.clone();

            let bg_task = if let Some(ssh_h) = ssh_handle.clone() {
                let has_kcp = session_mgr.subscribe_kcp_term_output(session_id).is_some();

                if has_kcp {
                    let term_rx = session_mgr.subscribe_kcp_term_output(session_id).unwrap();
                    log::info!(
                        "[{}] SSH starting KCP terminal_io relay for session {}",
                        self.id,
                        session_id
                    );
                    tokio::spawn(async move {
                        ssh_terminal_io_relay_kcp(
                            term_rx,
                            SshRelayParams {
                                ssh_handle: ssh_h,
                                channel,
                                client_rx,
                                session_close_tx,
                                term_input_rx,
                                session_id: sid,
                                client_id: cid,
                                session_mgr,
                            },
                        )
                        .await
                    })
                } else {
                    log::warn!(
                        "[{}] SSH: no KCP terminal_io available for session {} (kcp_term={})",
                        self.id,
                        session_id,
                        has_kcp,
                    );
                    // Fallback: only handle control messages
                    tokio::spawn(async move {
                        let mut rx = client_rx.lock().await;
                        loop {
                            match rx.recv().await {
                                Some(Control::SessionClose { .. }) | None => {
                                    let _ = session_close_tx.send(true);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    })
                }
            } else {
                tokio::spawn(async {})
            };

            self.state = SshState::Attached {
                session_id: session_id.to_string(),
                client_id,
                session_close_rx,
                bg_task,
                term_input_tx,
            };
            Ok(())
        } else {
            Err(())
        }
    }

    /// Send a string to SSH channel.
    async fn send_str(&self, channel: ChannelId, s: &str) -> std::result::Result<(), ()> {
        if let Some(handle) = &self.handle {
            let _ = handle.data(channel, s.as_bytes().to_vec()).await;
        }
        Ok(())
    }

    /// Cleanup: detach from session if attached.
    /// Safe to call multiple times — only executes once.
    async fn cleanup(&mut self) {
        if self.cleaned_up {
            return;
        }
        self.cleaned_up = true;
        match &self.state {
            SshState::Attached {
                session_id,
                client_id,
                bg_task,
                ..
            } => {
                let sid = session_id.clone();
                let cid = client_id.clone();
                bg_task.abort();
                self.config
                    .session_mgr
                    .set_terminal_io_active(&sid, &cid, false);
                self.config.session_mgr.detach_client(&sid, &cid);
                log::info!("[{}] SSH detached from session {}", self.id, sid);
            }
            SshState::Selecting {
                watcher: Some(w), ..
            } => {
                w.abort();
            }
            _ => {}
        }
        self.state = SshState::Initial;
    }
}

/// Load or generate an SSH host key.
/// If `custom_path` is provided, use it instead of the default path.
pub fn load_or_generate_host_key(custom_path: Option<&Path>) -> Result<PrivateKey> {
    // Try system host keys (Unix only)
    #[cfg(unix)]
    for path in &["/etc/ssh/ssh_host_ed25519_key", "/etc/ssh/ssh_host_rsa_key"] {
        if Path::new(path).exists()
            && let Ok(key) = keys::load_secret_key(path, None)
        {
            log::info!("Loaded SSH host key from {}", path);
            return Ok(key);
        }
    }

    // Determine user-level host key path
    let user_path = if let Some(path) = custom_path {
        path.to_path_buf()
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("ShellAnyWhere")
            .join("ssh_host_key")
    };

    if user_path.exists()
        && let Ok(key) = keys::load_secret_key(user_path.to_str().unwrap_or(""), None)
    {
        log::info!("Loaded SSH host key from {}", user_path.display());
        return Ok(key);
    }

    let key = keys::PrivateKey::random(&mut OsRng, keys::Algorithm::Ed25519)
        .map_err(|e| anyhow::anyhow!("Failed to generate SSH host key: {}", e))?;

    if let Some(parent) = user_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(pem) = key.to_openssh(russh::keys::ssh_key::LineEnding::LF) {
        let _ = std::fs::write(&user_path, pem.as_ref() as &[u8]);
        log::info!(
            "Generated and saved SSH host key to {}",
            user_path.display()
        );
    }
    Ok(key)
}

/// Load authorized_keys from a file.
pub fn load_authorized_keys(path: &str) -> Result<HashSet<PublicKey>> {
    let content = std::fs::read_to_string(path)?;
    let mut loaded_keys = HashSet::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 2
            && let Ok(key) = keys::parse_public_key_base64(parts[1])
        {
            loaded_keys.insert(key);
        }
    }
    Ok(loaded_keys)
}

// TODO: export_openssh_private_key was removed because token_key::derive_signing_key
// no longer exists in saw-core. If needed, this should be reimplemented using a
// different key derivation mechanism or the user should provide their own SSH key.

/// Send raw bytes to an SSH channel via a handle (avoids self borrow conflicts).
async fn send_to_channel(handle: &Option<server::Handle>, channel: ChannelId, data: &[u8]) {
    if let Some(h) = handle {
        let _ = h.data(channel, data.to_vec()).await;
    }
}

// ── SSH terminal I/O relay functions ──────────────────────────────────────

/// Common parameters for SSH terminal I/O relay tasks.
struct SshRelayParams {
    ssh_handle: russh::server::Handle,
    channel: ChannelId,
    client_rx: Arc<Mutex<mpsc::Receiver<Control>>>,
    session_close_tx: watch::Sender<bool>,
    term_input_rx: mpsc::Receiver<Bytes>,
    session_id: String,
    client_id: String,
    session_mgr: SharedSessionManager,
}

/// After a terminal relay ends (SessionClose or disconnect), show the session list.
async fn show_session_list_after_close(
    ssh_handle: &russh::server::Handle,
    channel: ChannelId,
    session_mgr: &SharedSessionManager,
) {
    let sessions = session_mgr.list_sessions();
    let menu = format_session_menu(&sessions, "");
    let _ = ssh_handle
        .data(
            channel,
            format!("\r\nSession closed.\r\n{}", menu)
                .as_bytes()
                .to_vec(),
        )
        .await;
}

/// SSH terminal I/O relay for KCP agent: subscribes to KCP broadcast output
/// and forwards input via the KCP input channel.
async fn ssh_terminal_io_relay_kcp(
    mut term_output_rx: tokio::sync::broadcast::Receiver<Bytes>,
    params: SshRelayParams,
) {
    let SshRelayParams {
        ssh_handle,
        channel,
        client_rx,
        session_close_tx,
        mut term_input_rx,
        session_id,
        client_id,
        session_mgr,
    } = params;

    let mut recv_count: u64 = 0;
    let mut input_closed = false;

    loop {
        tokio::select! {
            // KCP output → SSH channel
            result = term_output_rx.recv() => {
                match result {
                    Ok(data) => {
                        recv_count += 1;
                        let _ = ssh_handle.data(channel, data.to_vec()).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("SSH KCP relay: lagged {} messages", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        log::info!("SSH KCP relay: broadcast closed (recv_count={})", recv_count);
                        break;
                    }
                }
            }
            // SSH input → KCP agent
            data = async {
                if input_closed {
                    std::future::pending().await
                } else {
                    term_input_rx.recv().await
                }
            } => {
                match data {
                    Some(data) => {
                        if !session_mgr.forward_kcp_term_input(&session_id, data) {
                            log::warn!("SSH KCP relay: forward_kcp_term_input failed");
                        }
                    }
                    None => {
                        log::info!("SSH KCP relay: term_input_rx closed");
                        input_closed = true;
                    }
                }
            }
            // Control channel: SessionClose
            ctrl = async {
                client_rx.lock().await.recv().await
            } => {
                match ctrl {
                    Some(Control::SessionClose { .. }) | None => {
                        let _ = session_close_tx.send(true);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    log::info!(
        "SSH KCP relay ended for session {} client {} (recv_count={})",
        session_id,
        client_id,
        recv_count
    );
    session_mgr.set_terminal_io_active(&session_id, &client_id, false);

    show_session_list_after_close(&ssh_handle, channel, &session_mgr).await;
}
