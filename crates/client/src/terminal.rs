use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use crossterm::terminal;
use futures::StreamExt;
use saw_core::protocol::control::{AttachMode, Control};
use saw_core::protocol::kcp_transport;
use std::io::{self, IsTerminal as _, Write as _};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── Escape sequence detector (~. to disconnect) ───────────────────────────
// Matches OpenSSH behavior:
//   - After a newline (or at start of session), if the next char is '~',
//     enter escape detection mode
//   - '.' → disconnect (neither ~ nor . is sent to remote)
//   - '~' → send one '~' to remote (escape the tilde)
//   - anything else → send both '~' and the char to remote

/// State for detecting the ~. escape sequence.
struct EscapeDetector {
    /// True when we're at the beginning of a new line (just sent/received \r or \n)
    at_line_start: bool,
    /// True when we've seen '~' at line start and are waiting for the next char
    escape_pending: bool,
}

impl EscapeDetector {
    fn new() -> Self {
        Self {
            at_line_start: true, // session starts at "line start"
            escape_pending: false,
        }
    }

    /// Process outgoing bytes. Returns bytes to actually send, or None to disconnect.
    /// The detector tracks line boundaries to know when '~' could be an escape.
    fn process(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for &b in data {
            if self.escape_pending {
                self.escape_pending = false;
                match b {
                    b'.' => return None,    // ~. → disconnect
                    b'~' => out.push(b'~'), // ~~ → send one ~
                    _ => {
                        // Not an escape — send both ~ and this byte
                        out.push(b'~');
                        out.push(b);
                    }
                }
                self.at_line_start = false;
            } else if self.at_line_start && b == b'~' {
                self.escape_pending = true;
            } else {
                out.push(b);
                self.at_line_start = b == b'\r' || b == b'\n';
            }
        }
        Some(out)
    }
}

/// Reason the terminal relay ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayExit {
    /// User pressed ~. to disconnect
    UserDetach,
    /// Agent's shell exited (SessionClose)
    SessionClosed,
}

/// Check if stdin is connected to a TTY.
fn atty_check() -> bool {
    std::io::stdin().is_terminal()
}

/// Raw terminal mode guard - restores terminal on drop
pub struct RawTerminal;

impl RawTerminal {
    /// Enter raw mode (main screen buffer, preserves scrollback history)
    pub fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        print!("{}", crossterm::cursor::Show);
        io::stdout().flush()?;
        Ok(Self)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Disable focus event tracking before leaving raw mode
        let _ = io::stdout().write_all(b"\x1b[?1004l");
        let _ = io::stdout().flush();
        let _ = terminal::disable_raw_mode();
    }
}

fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let ch = c;
                if ch.is_ascii_lowercase() {
                    Some(vec![(ch as u8) - b'a' + 1])
                } else {
                    Some(c.to_string().into_bytes())
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut bytes = vec![0x1b];
                bytes.extend(c.to_string().as_bytes());
                Some(bytes)
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(1) => Some(b"\x1bOP".to_vec()),
        KeyCode::F(2) => Some(b"\x1bOQ".to_vec()),
        KeyCode::F(3) => Some(b"\x1bOR".to_vec()),
        KeyCode::F(4) => Some(b"\x1bOS".to_vec()),
        KeyCode::F(n) => {
            // xterm VT220 function key escape sequences
            // F1-F4 use SS3 sequences (handled above), F5+ use CSI
            // The numbering is non-contiguous: 15,17,18,19,20,21,23,24
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None, // Unsupported F-key
            };
            Some(format!("\x1b[{}~", code).into_bytes())
        }
        _ => None,
    }
}

/// Reconnect parameters for client auto-reconnect.
pub struct ReconnectParams {
    pub server: String,
    pub token: String,
    pub connect_timeout: std::time::Duration,
    pub fast_attempts: usize,
    pub fast_min: std::time::Duration,
    pub fast_max: std::time::Duration,
    pub slow_min: std::time::Duration,
    pub slow_max: std::time::Duration,
    pub keep_alive_interval: std::time::Duration,
    pub idle_timeout: std::time::Duration,
    pub focus_tracking: bool,
}

/// Generate a random duration in [min, max] range.
fn random_duration(min: std::time::Duration, max: std::time::Duration) -> std::time::Duration {
    let min_ms = min.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    if min_ms >= max_ms {
        return min;
    }
    let offset = rand::random::<u64>() % (max_ms - min_ms + 1);
    std::time::Duration::from_millis(min_ms + offset)
}

/// Write data directly to stdout fd (fd 1), bypassing Rust's LineWriter buffering.
/// This avoids the need for flush() while ensuring data is visible immediately.
/// Uses a single write() syscall — zero userspace buffering overhead.
#[cfg(unix)]
fn write_stdout_raw(data: &[u8]) {
    unsafe {
        libc::write(1, data.as_ptr() as *const libc::c_void, data.len());
    }
}

#[cfg(not(unix))]
fn write_stdout_raw(data: &[u8]) {
    let _ = io::stdout().write_all(data);
    let _ = io::stdout().flush();
}

// ── KCP Terminal Relay ────────────────────────────────────────────────────

/// No-TTY observe mode: just read terminal output from KCP term stream,
/// log LATENCY probes, and send keepalive pings. Discards output bytes.
async fn run_kcp_observe_no_tty(
    mut term_stream: kcp_transport::KcpVirtualStream,
    mut from_server_rx: tokio::sync::mpsc::Receiver<anyhow::Result<Control>>,
    keep_alive_interval: std::time::Duration,
    idle_timeout: std::time::Duration,
) -> Result<RelayExit> {
    let mut term_buf = vec![0u8; 65536];
    let mut ping_interval = tokio::time::interval(keep_alive_interval);
    ping_interval.tick().await;
    let mut pending_pongs: u32 = 0;
    let max_missed = (idle_timeout.as_secs() / keep_alive_interval.as_secs()).max(3) as u32;
    let mut exit_reason = RelayExit::UserDetach;

    loop {
        tokio::select! {
            n = term_stream.read(&mut term_buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        let t = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64() * 1000.0;
                        log::trace!("[LATENCY] client recv {} bytes at {:.3}ms", n, t);
                    }
                    Err(_) => break,
                }
            }
            result = from_server_rx.recv() => {
                match result {
                    Some(Ok(Control::SessionClose { .. })) => {
                        exit_reason = RelayExit::SessionClosed;
                        break;
                    }
                    Some(Ok(Control::Pong)) => { pending_pongs = 0; }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            _ = ping_interval.tick() => {
                pending_pongs += 1;
                if pending_pongs > max_missed {
                    log::debug!("KCP observe: no pong for {} intervals, disconnecting", pending_pongs);
                    break;
                }
            }
        }
    }
    Ok(exit_reason)
}

/// Run the client terminal relay over KCP with automatic reconnection.
///
/// KCP transport version of the terminal relay with automatic reconnection.
/// When the connection drops, automatically reconnects with the same session_id
/// and client_id (via `previous_client_id` in SessionAttach).
#[allow(clippy::too_many_arguments)]
pub async fn run_kcp_terminal_relay_with_reconnect(
    control_stream: kcp_transport::KcpVirtualStream,
    term_stream: kcp_transport::KcpVirtualStream,
    session_id: &str,
    observe: bool,
    client_id: &str,
    reconnect: ReconnectParams,
) -> Result<RelayExit> {
    let mut raw_guard: Option<RawTerminal> = if observe && !atty_check() {
        None
    } else {
        Some(RawTerminal::enter()?)
    };

    let mut attempt_count: usize = 0;
    let mut current_client_id = client_id.to_string();

    // First connection
    let result = run_kcp_terminal_relay(
        control_stream,
        term_stream,
        session_id,
        observe,
        &current_client_id,
        reconnect.keep_alive_interval,
        reconnect.idle_timeout,
        reconnect.focus_tracking,
    )
    .await;

    match result {
        Ok(exit) => return Ok(exit),
        Err(e) => {
            log::debug!("KCP connection lost, will reconnect: {}", e);
            attempt_count += 1;
        }
    }

    // Reconnect loop
    loop {
        drop(raw_guard.take());
        eprintln!("\r\nConnection lost, reconnecting... (Ctrl+C to exit)");

        let delay = if attempt_count <= reconnect.fast_attempts {
            random_duration(reconnect.fast_min, reconnect.fast_max)
        } else {
            random_duration(reconnect.slow_min, reconnect.slow_max)
        };
        log::debug!(
            "KCP reconnect attempt {} (delay: {:?})",
            attempt_count,
            delay
        );

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => {
                log::info!("Interrupted by Ctrl+C, exiting");
                return Ok(RelayExit::UserDetach);
            }
        }

        let reconnect_result = tokio::select! {
            result = kcp_reconnect_attempt(
                &reconnect.server,
                &reconnect.token,
                session_id,
                observe,
                &current_client_id,
                reconnect.connect_timeout,
                reconnect.keep_alive_interval,
                reconnect.idle_timeout,
            ) => result,
            _ = tokio::signal::ctrl_c() => {
                log::info!("Interrupted by Ctrl+C, exiting");
                return Ok(RelayExit::UserDetach);
            }
        };

        match reconnect_result {
            Ok((control_stream, term_stream, new_client_id)) => {
                log::debug!(
                    "KCP reconnected to session {} (client_id: {})",
                    session_id,
                    new_client_id
                );
                current_client_id = new_client_id;
                attempt_count = 0;

                raw_guard = if observe && !atty_check() {
                    None
                } else {
                    Some(RawTerminal::enter()?)
                };

                match run_kcp_terminal_relay(
                    control_stream,
                    term_stream,
                    session_id,
                    observe,
                    &current_client_id,
                    reconnect.keep_alive_interval,
                    reconnect.idle_timeout,
                    reconnect.focus_tracking,
                )
                .await
                {
                    Ok(exit) => return Ok(exit),
                    Err(e) => {
                        log::debug!("KCP connection lost again: {}", e);
                        attempt_count += 1;
                    }
                }
            }
            Err(e) => {
                log::debug!("KCP reconnect attempt {} failed: {}", attempt_count, e);
                attempt_count += 1;
            }
        }
    }
}

/// Attempt a single KCP reconnection: connect, authenticate, attach.
#[allow(clippy::too_many_arguments)]
async fn kcp_reconnect_attempt(
    server: &str,
    token: &str,
    session_id: &str,
    observe: bool,
    previous_client_id: &str,
    connect_timeout: std::time::Duration,
    _keep_alive_interval: std::time::Duration,
    _idle_timeout: std::time::Duration,
) -> Result<(
    kcp_transport::KcpVirtualStream,
    kcp_transport::KcpVirtualStream,
    String,
)> {
    use crate::kcp_connector::KcpClientConnector;

    let connect_fut = KcpClientConnector::connect(server, token.to_string());
    let connector = tokio::time::timeout(connect_timeout, connect_fut)
        .await
        .map_err(|_| anyhow::anyhow!("KCP connect timeout"))??;

    let mode = if observe {
        AttachMode::Observe
    } else {
        AttachMode::Interact
    };
    let attached = connector
        .attach(session_id, mode, Some(previous_client_id.to_string()))
        .await?;

    Ok((
        attached.control_stream,
        attached.term_stream,
        attached.client_id,
    ))
}

/// Run the client terminal relay over KCP (separate control + term streams).
#[allow(clippy::too_many_arguments)]
pub async fn run_kcp_terminal_relay(
    mut control_stream: kcp_transport::KcpVirtualStream,
    mut term_stream: kcp_transport::KcpVirtualStream,
    session_id: &str,
    observe: bool,
    client_id: &str,
    keep_alive_interval: std::time::Duration,
    idle_timeout: std::time::Duration,
    focus_tracking: bool,
) -> Result<RelayExit> {
    // In observe mode without a TTY, skip raw mode so we can still receive data
    let _raw_guard = if observe && !atty_check() {
        None
    } else {
        Some(RawTerminal::enter()?)
    };

    let (to_server_tx, mut to_server_rx) = tokio::sync::mpsc::channel::<Control>(256);
    let (from_server_tx, mut from_server_rx) =
        tokio::sync::mpsc::channel::<anyhow::Result<Control>>(64);

    // Control I/O task
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ctrl = to_server_rx.recv() => {
                    match ctrl {
                        Some(ctrl) => {
                            if let Err(e) = send_kcp_control(&mut control_stream, &ctrl).await {
                                let _ = from_server_tx.send(Err(e)).await;
                                break;
                            }
                        }
                        None => break,
                    }
                }
                result = recv_kcp_control(&mut control_stream) => {
                    match result {
                        Ok(ctrl) => {
                            if from_server_tx.send(Ok(ctrl)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = from_server_tx.send(Err(e)).await;
                            break;
                        }
                    }
                }
            }
        }
    });

    // In observe mode without TTY, skip keyboard event reader
    if observe && !atty_check() {
        return run_kcp_observe_no_tty(
            term_stream,
            from_server_rx,
            keep_alive_interval,
            idle_timeout,
        )
        .await;
    }

    let reader = EventStream::new();
    futures::pin_mut!(reader);
    let mut escape = EscapeDetector::new();

    // Enable focus event tracking
    if focus_tracking {
        let _ = io::stdout().write_all(b"\x1b[?1004h");
        let _ = io::stdout().flush();
    }

    // Send initial terminal size
    if !observe && let Ok((cols, rows)) = terminal::size() {
        let _ = to_server_tx
            .send(Control::ClientResize {
                session_id: session_id.to_string(),
                client_id: client_id.to_string(),
                cols,
                rows,
            })
            .await;
    }

    // Heartbeat
    let mut ping_interval = tokio::time::interval(keep_alive_interval);
    ping_interval.tick().await;
    let mut pending_pongs: u32 = 0;
    let max_pending_pongs = (idle_timeout.as_secs() / keep_alive_interval.as_secs().max(1)) as u32;

    let mut term_buf = vec![0u8; 65536];
    let exit_reason;

    loop {
        tokio::select! {
            // Terminal input → term_stream
            event = reader.next() => {
                if observe {
                    if let Some(Ok(Event::Key(key))) = event
                        && let Some(data) = key_event_to_bytes(key)
                            && escape.process(&data).is_none() {
                                exit_reason = RelayExit::UserDetach;
                                break;
                            }
                } else {
                    match event {
                        Some(Ok(Event::Key(key))) => {
                            if let Some(data) = key_event_to_bytes(key) {
                                match escape.process(&data) {
                                    None => {
                                        // ~. escape → disconnect
                                        let _ = to_server_tx.send(Control::SessionDetach {
                                            session_id: session_id.to_string(),
                                            client_id: client_id.to_string(),
                                        }).await;
                                        exit_reason = RelayExit::UserDetach;
                                        break;
                                    }
                                    Some(filtered) if !filtered.is_empty()
                                        && term_stream.write_all(&filtered).await.is_err() => {
                                            return Err(anyhow::anyhow!("KCP term stream closed"));
                                        }
                                    _ => {}
                                }
                            }
                        }
                        Some(Ok(Event::Resize(cols, rows))) => {
                            let _ = to_server_tx.send(Control::ClientResize {
                                session_id: session_id.to_string(),
                                client_id: client_id.to_string(),
                                cols,
                                rows,
                            }).await;
                        }
                        Some(Ok(Event::FocusGained)) if focus_tracking && !observe => {
                            let (cols, rows) = terminal::size().unwrap_or((0, 0));
                            let _ = to_server_tx.send(Control::ClientResize {
                                session_id: session_id.to_string(),
                                client_id: client_id.to_string(),
                                cols,
                                rows,
                            }).await;
                        }
                        Some(Ok(Event::FocusLost)) => {}
                        _ => {}
                    }
                }
            }
            // Control message from server
            ctrl = from_server_rx.recv() => {
                match ctrl {
                    Some(Ok(Control::SessionDetach { client_id: detach_client_id, .. })) => {
                        if detach_client_id == client_id {
                            exit_reason = RelayExit::UserDetach;
                            break;
                        }
                    }
                    Some(Ok(Control::SessionClose { .. })) => {
                        log::info!("Remote shell has exited");
                        exit_reason = RelayExit::SessionClosed;
                        break;
                    }
                    Some(Ok(Control::Pong)) => {
                        pending_pongs = 0;
                    }
                    Some(Ok(Control::Ping)) => {
                        let _ = to_server_tx.send(Control::Pong).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        return Err(e);
                    }
                    None => {
                        return Err(anyhow::anyhow!("Connection closed"));
                    }
                }
            }
            // Terminal output from server → stdout
            n = term_stream.read(&mut term_buf) => {
                match n {
                    Ok(0) => {
                        return Err(anyhow::anyhow!("KCP term stream closed"));
                    }
                    Ok(n) => {
                        let t = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs_f64() * 1000.0;
                        log::trace!("[LATENCY] client recv {} bytes at {:.3}ms", n, t);
                        write_stdout_raw(&term_buf[..n]);
                        if focus_tracking && term_buf[..n].windows(7).any(|w| w == b"\x1b[?1049l") {
                            write_stdout_raw(b"\x1b[?1004h");
                        }
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("KCP term read error: {}", e));
                    }
                }
            }
            // Heartbeat
            _ = ping_interval.tick() => {
                if pending_pongs >= max_pending_pongs {
                    return Err(anyhow::anyhow!("Connection timeout: no Pong received"));
                }
                if to_server_tx.send(Control::Ping).await.is_err() {
                    return Err(anyhow::anyhow!("Connection closed"));
                }
                pending_pongs += 1;
            }
        }
    }

    Ok(exit_reason)
}

/// Send a length-prefixed Control message on a KCP virtual stream.
async fn send_kcp_control(
    stream: &mut kcp_transport::KcpVirtualStream,
    ctrl: &Control,
) -> anyhow::Result<()> {
    let data = ctrl.encode()?;
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

/// Receive a length-prefixed Control message from a KCP virtual stream.
async fn recv_kcp_control(stream: &mut kcp_transport::KcpVirtualStream) -> anyhow::Result<Control> {
    let mut len_buf = [0u8; 4];
    let mut offset = 0;
    while offset < 4 {
        match stream.read(&mut len_buf[offset..]).await {
            Ok(0) => anyhow::bail!("control stream closed"),
            Ok(n) => offset += n,
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        anyhow::bail!("control message too large: {} bytes", len);
    }
    let mut buf = vec![0u8; len];
    let mut offset = 0;
    while offset < len {
        match stream.read(&mut buf[offset..]).await {
            Ok(0) => anyhow::bail!("control stream closed mid-message"),
            Ok(n) => offset += n,
            Err(e) => return Err(e.into()),
        }
    }
    Control::decode(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn test_key_char() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(key), Some(b"a".to_vec()));
    }

    #[test]
    fn test_key_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(key), Some(b"\r".to_vec()));
    }

    #[test]
    fn test_key_backspace() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(key), Some(vec![0x7f]));
    }

    #[test]
    fn test_key_arrow_up() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(key), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn test_ctrl_c() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_bytes(key), Some(vec![3])); // ^C
    }

    #[test]
    fn test_escape_tilde_dot() {
        let mut ed = EscapeDetector::new();
        // Newline, then ~.
        assert_eq!(ed.process(b"\r"), Some(b"\r".to_vec()));
        assert_eq!(ed.process(b"~"), Some(vec![])); // ~ eaten, waiting
        assert_eq!(ed.process(b"."), None); // disconnect!
    }

    #[test]
    fn test_escape_tilde_tilde() {
        let mut ed = EscapeDetector::new();
        // ~~ → send one ~
        assert_eq!(ed.process(b"~"), Some(vec![])); // first ~ eaten
        assert_eq!(ed.process(b"~"), Some(b"~".to_vec())); // second ~ → send one
    }

    #[test]
    fn test_escape_tilde_other() {
        let mut ed = EscapeDetector::new();
        // ~a → send both ~ and a
        assert_eq!(ed.process(b"~"), Some(vec![])); // ~ eaten
        assert_eq!(ed.process(b"a"), Some(b"~a".to_vec())); // send both
    }

    #[test]
    fn test_tilde_not_at_line_start() {
        let mut ed = EscapeDetector::new();
        // Not at line start — ~ is normal
        assert_eq!(ed.process(b"hello~world"), Some(b"hello~world".to_vec()));
    }

    #[test]
    fn test_escape_after_backspace_cancel() {
        let mut ed = EscapeDetector::new();
        // ~ then backspace (0x7f) → not '.', so ~ and 0x7f are both sent
        assert_eq!(ed.process(b"~"), Some(vec![]));
        assert_eq!(ed.process(b"\x7f"), Some(b"~\x7f".to_vec()));
        // Now at_line_start is false, so next ~ is normal
        assert_eq!(ed.process(b"~."), Some(b"~.".to_vec()));
    }

    #[test]
    fn test_alt_letter() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(key_event_to_bytes(key), Some(b"\x1ba".to_vec()));
    }

    #[test]
    fn test_f1_f4() {
        let f1 = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(f1), Some(b"\x1bOP".to_vec()));
        let f4 = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(f4), Some(b"\x1bOS".to_vec()));
    }

    #[test]
    fn test_f5_plus() {
        let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(f5), Some(b"\x1b[15~".to_vec()));
        let f6 = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(f6), Some(b"\x1b[17~".to_vec()));
        let f12 = KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE);
        assert_eq!(key_event_to_bytes(f12), Some(b"\x1b[24~".to_vec()));
    }
}
