/// Virtual terminal for replay and diff — runs vt100 parser in a dedicated
/// thread to avoid blocking the shell's main relay loop.
///
/// The shell's PTY output is forwarded to this thread via an mpsc channel.
/// When a new client attaches, the main loop requests a screen snapshot
/// via a oneshot channel, and the virtual term thread responds with
/// the serialized escape sequences (scrollback + visible screen).
///
/// The vt100 parser is configured with `scrollback_len = SCROLLBACK_LINES`
/// so scrolled-off rows are preserved with full per-cell attributes (fg, bg,
/// flags). When serializing, we set `scrollback_offset` to reveal all
/// scrollback rows, then call `contents_formatted()` which emits escape
/// sequences for the entire visible region (scrollback + current screen).
///
/// Additionally, the virtual term produces screen diffs every ~100ms.
/// A `DiffSnapshot` command returns the diff between the current screen
/// and the screen at the time of the last `DiffSnapshot`. This is used
/// by TerminalIO relays to send only changes instead of raw PTY bytes.
use std::collections::HashSet;
use tokio::sync::{mpsc, oneshot};

/// Maximum scrollback lines to keep for replay.
const SCROLLBACK_LINES: usize = 100;

// ── Messages sent to the virtual term thread ──

enum VtCommand {
    /// Feed PTY output bytes to the vt100 parser.
    Process(Vec<u8>),
    /// Resize the virtual terminal grid.
    Resize(u16, u16),
    /// Request a snapshot of the current screen state as escape sequences.
    Snapshot(oneshot::Sender<Vec<u8>>),
    /// Request a screen diff since the last DiffSnapshot.
    /// Returns (diff_bytes, is_full_repaint, scrollback_grew).
    DiffSnapshot(oneshot::Sender<(Vec<u8>, bool, bool)>),
    /// Register a new attached client.
    ClientAttached(String),
    /// Remove an attached client. Reply with true if it was the last one.
    ClientDetached(String, oneshot::Sender<bool>),
    /// Clear scrollback buffer (triggered by CSI 3J / clear-screen).
    ClearScrollback,
}

// ── Public handle (Send + Sync, used from the relay loop) ──

pub struct VirtualTermHandle {
    tx: mpsc::Sender<VtCommand>,
}

impl Clone for VirtualTermHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

impl VirtualTermHandle {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (tx, rx) = mpsc::channel(256);

        std::thread::spawn(move || {
            vt_loop(rx, rows, cols);
        });

        Self { tx }
    }

    /// Feed PTY output to the virtual terminal (non-blocking, drops if full).
    pub fn process(&self, data: Vec<u8>) {
        let _ = self.tx.try_send(VtCommand::Process(data));
    }

    /// Resize the virtual terminal grid (non-blocking, drops if full).
    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.tx.try_send(VtCommand::Resize(rows, cols));
    }

    /// Get the current screen state as escape sequences (async, awaits response).
    pub async fn serialize_screen(&self) -> Vec<u8> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send(VtCommand::Snapshot(reply_tx)).await.is_err() {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Get the screen diff since the last DiffSnapshot call.
    /// Returns (diff_bytes, is_full_repaint, is_fullscreen).
    /// - is_full_repaint: true after resize, first call, or alternate_screen transition
    /// - is_fullscreen: true when alternate screen buffer is active (vim/htop mode)
    pub async fn diff_snapshot(&self) -> (Vec<u8>, bool, bool) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(VtCommand::DiffSnapshot(reply_tx))
            .await
            .is_err()
        {
            return (Vec::new(), false, false);
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Register a new attached client (non-blocking, drops if full).
    pub fn on_client_attached(&self, client_id: String) {
        let _ = self.tx.try_send(VtCommand::ClientAttached(client_id));
    }

    /// Remove an attached client. Returns true if it was the last one.
    pub async fn on_client_detached(&self, client_id: String) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(VtCommand::ClientDetached(client_id, reply_tx))
            .await
            .is_err()
        {
            return false;
        }
        reply_rx.await.unwrap_or(false)
    }

    /// Clear scrollback buffer (non-blocking, drops if full).
    pub fn clear_scrollback(&self) {
        let _ = self.tx.try_send(VtCommand::ClearScrollback);
    }
}

// ── Internal: the vt100 parser running on a std thread ──

fn vt_loop(mut rx: mpsc::Receiver<VtCommand>, rows: u16, cols: u16) {
    let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
    let mut attached_clients: HashSet<String> = HashSet::new();
    let mut max_rows = rows;
    let mut max_cols = cols;

    // Screen state for diff computation.
    // None until the first DiffSnapshot request (lazy init).
    let mut diff_prev_screen: Option<vt100::Screen> = None;
    let mut needs_full_repaint = false;
    // Track alternate_screen transitions: when it changes (entering/exiting vim),
    // the screen content is completely different so diff is meaningless.
    let mut last_alternate_screen: bool = false;

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            VtCommand::Process(data) => {
                parser.process(&data);
            }
            VtCommand::Resize(rows, cols) => {
                // Only grow — never shrink the virtual terminal,
                // so screen content from larger sessions isn't truncated.
                if rows > max_rows || cols > max_cols {
                    max_rows = max_rows.max(rows);
                    max_cols = max_cols.max(cols);
                    parser.screen_mut().set_size(max_rows, max_cols);
                    needs_full_repaint = true;
                }
            }
            VtCommand::Snapshot(reply) => {
                let bytes = serialize_full(parser.screen_mut());
                let _ = reply.send(bytes);
            }
            VtCommand::DiffSnapshot(reply) => {
                let is_fullscreen = parser.screen().alternate_screen();

                // Detect alternate_screen transitions → full repaint needed
                // (entering/exiting vim: screen content is completely different)
                if is_fullscreen != last_alternate_screen {
                    needs_full_repaint = true;
                    last_alternate_screen = is_fullscreen;
                }

                let current = parser.screen().clone();
                let (diff, is_full) = if needs_full_repaint || diff_prev_screen.is_none() {
                    // First call, after resize, or alternate_screen transition:
                    // send full screen repaint
                    needs_full_repaint = false;
                    let full = current.contents_formatted().to_vec();
                    diff_prev_screen = Some(current);
                    (full, true)
                } else {
                    let prev = diff_prev_screen.as_ref().unwrap();
                    let diff = current.contents_diff(prev);
                    diff_prev_screen = Some(current);
                    (diff, false)
                };
                let _ = reply.send((diff, is_full, is_fullscreen));
            }
            VtCommand::ClientAttached(client_id) => {
                attached_clients.insert(client_id);
            }
            VtCommand::ClientDetached(client_id, reply) => {
                attached_clients.remove(&client_id);
                let _ = reply.send(attached_clients.is_empty());
            }
            VtCommand::ClearScrollback => {
                let (current_rows, current_cols) = parser.screen().size();
                parser = vt100::Parser::new(current_rows, current_cols, SCROLLBACK_LINES);
                needs_full_repaint = true;
                last_alternate_screen = false;
            }
        }
    }
}

/// Serialize the full terminal state (scrollback + visible screen) as
/// formatted escape sequences with colors and styles preserved.
fn serialize_full(screen: &mut vt100::Screen) -> Vec<u8> {
    screen.set_scrollback(usize::MAX);
    let scrollback_count = screen.scrollback();

    if scrollback_count == 0 {
        return screen.contents_formatted().to_vec();
    }

    let result = screen.contents_formatted().to_vec();
    screen.set_scrollback(0);
    result
}
