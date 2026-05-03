//! Local terminal I/O primitives for the agent.
//!
//! Provides raw mode guard, stdin reader, resize watcher, and stdout writer
//! so the agent can bridge local terminal I/O simultaneously with remote relay.

#[cfg(not(unix))]
use std::io::Write;
use std::io::{IsTerminal, Read};

pub use saw_core::util::terminal_filter::{
    final_defense_filter, final_defense_filter_owned, is_cpr_response, is_osc_query,
    skip_dcs_sequence, skip_osc_sequence,
};

// ---------------------------------------------------------------------------
// RawModeGuard
// ---------------------------------------------------------------------------

/// RAII guard that enables raw terminal mode and restores it on drop.
pub struct RawModeGuard {
    _private: (),
}

impl RawModeGuard {
    /// Enter raw mode. Returns `None` (and does nothing) if stdin is not a TTY.
    pub fn enter() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            log::info!("stdin is not a TTY, local I/O disabled");
            return None;
        }
        crossterm::terminal::enable_raw_mode().ok()?;
        Some(Self { _private: () })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// StdinReader
// ---------------------------------------------------------------------------

/// Async reader for local stdin, backed by a dedicated OS thread.
///
/// Uses a `std::thread` + `mpsc::channel` pattern (same as `PtyReader`)
/// so that blocking `std::io::Read` never blocks the tokio runtime.
pub struct StdinReader {
    inner: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

#[allow(clippy::new_without_default)]
impl StdinReader {
    /// Spawn a background thread that reads raw bytes from stdin.
    /// Returns a disabled reader if stdin is not a TTY.
    pub fn new() -> Self {
        if !std::io::stdin().is_terminal() {
            return Self { inner: None };
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut handle = stdin.lock();
            let mut buf = [0u8; 4096];

            loop {
                match handle.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self { inner: Some(rx) }
    }

    /// Receive next chunk from stdin.
    ///
    /// Returns `Some(data)` on input, `None` on EOF.
    /// After EOF the reader disables itself — subsequent calls never resolve.
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        let result = match self.inner.as_mut() {
            Some(rx) => rx.recv().await,
            None => return std::future::pending().await,
        };

        if result.is_none() {
            // EOF — disable further reads
            self.inner = None;
            log::info!("Local stdin EOF, local input disabled");
        }
        result
    }
}

// ---------------------------------------------------------------------------
// ResizeWatcher
// ---------------------------------------------------------------------------

/// Watches for SIGWINCH and yields new terminal sizes.
pub struct ResizeWatcher {
    rx: tokio::sync::mpsc::Receiver<(u16, u16)>,
}

#[allow(clippy::new_without_default)]
impl ResizeWatcher {
    /// Spawn a background task that watches for window resize signals.
    /// Returns a disabled watcher if stdin is not a TTY.
    pub fn new() -> Self {
        if !std::io::stdin().is_terminal() {
            let (_, rx) = tokio::sync::mpsc::channel::<(u16, u16)>(1);
            return Self { rx };
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<(u16, u16)>(8);

        #[cfg(unix)]
        tokio::spawn(async move {
            let mut sigwinch =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("Failed to install SIGWINCH handler: {}", e);
                        return;
                    }
                };

            loop {
                sigwinch.recv().await;
                if let Ok((cols, rows)) = crossterm::terminal::size()
                    && tx.send((cols, rows)).await.is_err()
                {
                    break;
                }
            }
        });

        #[cfg(not(unix))]
        tokio::spawn(async move {
            // On Windows there is no SIGWINCH; poll terminal size periodically.
            let mut last_size = crossterm::terminal::size().ok();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let current = crossterm::terminal::size().ok();
                if current != last_size {
                    last_size = current;
                    if let Some((cols, rows)) = current {
                        if tx.send((cols, rows)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self { rx }
    }

    /// Receive the next resize event. Returns `None` if the watcher is shut down.
    pub async fn next_resize(&mut self) -> Option<(u16, u16)> {
        self.rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// filter_response_echoes
// ---------------------------------------------------------------------------

/// Filter terminal response echoes from PTY output data.
///
/// When ECHO is enabled on the PTY slave, terminal query responses written
/// to the PTY master are echoed back and appear as garbled text on the local
/// terminal. This function removes those echoes while preserving the original
/// query sequences (so the local terminal can still respond to them).
///
/// What's removed (response echoes):
/// - DA1 responses: ESC [ ? <params> c (e.g., ESC[?64;1;2;6;9;15;16;17;18;21;22c)
/// - DA2 responses: ESC [ > <params> c (e.g., ESC[>1;3;1c)
/// - DSR cursor position responses: ESC [ <row> ; <col> R (e.g., ESC[10;20R)
/// - DCS responses: ESC P > | ... ST (XTVERSION response)
///   ESC P 1 + r ... ST (XTGETTCAP response)
///   ESC P 1 $ r ... ST (DECRQSS response)
///
/// What's preserved (queries and display sequences):
/// - DA1 queries: ESC [ c, ESC [ 0 c (no `?` in params)
/// - DA2 queries: ESC [ > c, ESC [ > 0 c (`>` with no/one digit)
/// - DSR queries: ESC [ 5 n, ESC [ 6 n
/// - DCS queries: ESC P + q ... ST (XTGETTCAP)
///   ESC P $ q ... ST (DECRQSS)
///   ESC P > q ... ST (XTVERSION)
/// - All other CSI sequences (colors, cursor movement, etc.)
/// - All regular text
///
/// Distinction between queries and responses:
/// - DA1 query params don't contain `?` (0x3F); responses do
/// - DA2 query params are just `>` or `>0`; responses have `>` + semicolons/digits
/// - DSR responses end with `R` (cursor position report) with digit;digit params
/// - DCS queries start with +q, $q, >q; responses start with 1+r, 1$r, >|
pub fn filter_response_echoes(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();

    while i < len {
        if i + 1 < len && data[i] == 0x1b {
            match data[i + 1] {
                // ESC [ — CSI sequence
                b'[' => {
                    let (action, new_i) = classify_csi_response(data, i + 2);
                    match action {
                        CsiResponseAction::Echo => {
                            i = new_i;
                            continue;
                        }
                        CsiResponseAction::Keep => {
                            result.push(data[i]);
                            i += 1;
                            continue;
                        }
                    }
                }
                // ESC P — DCS (Device Control String)
                b'P' => {
                    if is_dcs_response(data, i + 2) {
                        // Skip the entire DCS response echo
                        i += 2;
                        i = skip_dcs_sequence(data, i);
                        continue;
                    } else {
                        // DCS query or set command — keep
                        result.push(data[i]);
                        i += 1;
                        continue;
                    }
                }
                // Other ESC sequences — keep
                _ => {
                    result.push(data[i]);
                    i += 1;
                    continue;
                }
            }
        }
        result.push(data[i]);
        i += 1;
    }

    result
}

/// Check if a DCS sequence (starting after ESC P) is a response echo.
///
/// DCS responses have these patterns:
/// - `>|` → XTVERSION response (e.g., ESC P > | ghostty 1.3.1 ST)
/// - `1+r` → XTGETTCAP response
/// - `1$r` → DECRQSS response
///
/// DCS queries have these patterns:
/// - `+q` → XTGETTCAP query
/// - `$q` → DECRQSS query
/// - `>q` → XTVERSION query
fn is_dcs_response(data: &[u8], start: usize) -> bool {
    let len = data.len();
    if start >= len {
        return false;
    }

    // XTVERSION response: >|
    if data[start] == b'>' && start + 1 < len && data[start + 1] == b'|' {
        return true;
    }

    // XTGETTCAP response: 1+r
    if data[start] == b'1' && start + 2 < len && data[start + 1] == b'+' && data[start + 2] == b'r'
    {
        return true;
    }

    // DECRQSS response: 1$r
    if data[start] == b'1' && start + 2 < len && data[start + 1] == b'$' && data[start + 2] == b'r'
    {
        return true;
    }

    false
}

enum CsiResponseAction {
    Echo, // Response echo — skip
    Keep, // Query or display sequence — keep
}

/// Classify a CSI sequence as either a response echo (to filter) or
/// a query/display sequence (to keep).
fn classify_csi_response(data: &[u8], start: usize) -> (CsiResponseAction, usize) {
    let mut i = start;
    let len = data.len();

    // Collect parameter bytes (0x30-0x3F)
    let param_start = i;
    while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
        i += 1;
    }
    let params = &data[param_start..i];

    // Skip intermediate bytes (0x20-0x2F)
    while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
        i += 1;
    }

    // Final byte (0x40-0x7E)
    if i >= len {
        return (CsiResponseAction::Keep, i);
    }

    let final_byte = data[i];
    i += 1;

    match final_byte {
        // DA responses end with 'c'
        b'c' => {
            if is_da_response(params) {
                (CsiResponseAction::Echo, i)
            } else {
                // This is a DA query (ESC[c, ESC[0c, ESC[>c, ESC[>0c)
                (CsiResponseAction::Keep, i)
            }
        }
        // DSR cursor position response ends with 'R'
        b'R' => {
            if is_cpr_response(params) {
                (CsiResponseAction::Echo, i)
            } else {
                (CsiResponseAction::Keep, i)
            }
        }
        // DSR response: ESC [ <n> n where n is 0-4
        // This is tricky: DSR queries also end with 'n' (ESC[5n, ESC[6n)
        // Responses: ESC[0n (OK), ESC[3n (malfunction)
        // Queries: ESC[5n (device status), ESC[6n (cursor position)
        // We keep all of these since they're rare and distinguishing is hard
        b'n' => (CsiResponseAction::Keep, i),
        // All other sequences — keep
        _ => (CsiResponseAction::Keep, i),
    }
}

/// Check if DA parameters indicate a response (not a query).
///
/// DA1 response: params contain `?` (e.g., `?64;1;2;6;9;15;16;17;18;21;22`)
/// DA1 query: params are empty or just digits (e.g., `0`, ``)
///
/// DA2 response: params start with `>` followed by semicolons/digits (e.g., `>1;3;1`)
/// DA2 query: params are just `>` or `>0`
fn is_da_response(params: &[u8]) -> bool {
    if params.is_empty() {
        return false;
    }

    // DA1 response: contains '?' in params
    if params.contains(&b'?') {
        return true;
    }

    // DA2: starts with '>'
    if params[0] == b'>' {
        // DA2 query: just ">" or ">0"
        // DA2 response: ">" followed by params with semicolons (e.g., ">1;3;1")
        if params.len() == 1 {
            return false; // ESC[>c — query
        }
        // Check if params after '>' contain semicolons (response indicator)
        // DA2 query: ">0" or just ">"
        // DA2 response: ">1;3;1" etc.
        let after_gt = &params[1..];
        // If there's a semicolon, it's a response
        if after_gt.contains(&b';') {
            return true;
        }
        // If after '>' is just '0' (or empty), it's a query
        // If after '>' is a non-zero digit, it could be a response
        // e.g., ESC[>1c could be a short DA2 response
        // To be safe, check if the digit is > 0
        if after_gt.len() == 1 && after_gt[0] != b'0' {
            return true; // e.g., ESC[>1c is a response
        }
    }

    false
}

// ---------------------------------------------------------------------------
// filter_query_sequences
// ---------------------------------------------------------------------------

/// Filter terminal query sequences from PTY output data.
///
/// This removes escape sequences that cause the terminal to respond,
/// preventing a feedback loop: shell sends query → terminal responds →
/// response written to PTY → PTY echoes response back → garbled text.
///
/// What's removed:
/// - OSC queries (ESC ] Ps ; ? ... ST/BEL) — e.g., OSC 11 background color query
/// - CSI Device Attributes requests (CSI c, CSI 0 c, CSI ? c)
/// - CSI Device Status Report requests (CSI 5 n, CSI 6 n, CSI ? 6 n)
/// - DCS queries (ESC P + q, ESC P $ q) — XTGETTCAP, DECRQSS
///
/// What's preserved:
/// - OSC set commands (ESC ] Ps ; data ST/BEL without ?)
/// - CSI display sequences (colors, cursor movement, etc.)
/// - DCS set/restore commands (not queries)
/// - All regular text
///
/// Use `extract_query_responses` to get cached responses for the extracted queries.
pub fn filter_query_sequences(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();

    while i < len {
        if i + 1 < len && data[i] == 0x1b {
            match data[i + 1] {
                // ESC ] — OSC sequence
                b']' => {
                    // Check if this is a query (contains ? after semicolon)
                    if is_osc_query(data, i + 2) {
                        // Skip the entire OSC query sequence
                        i += 2;
                        i = skip_osc_sequence(data, i);
                        continue;
                    } else {
                        // OSC set command — keep it
                        result.push(data[i]);
                        i += 1;
                        continue;
                    }
                }
                // ESC [ — CSI sequence
                b'[' => {
                    let (action, new_i) = classify_csi(data, i + 2);
                    match action {
                        CsiAction::Query => {
                            // Skip this CSI query sequence
                            i = new_i;
                            continue;
                        }
                        CsiAction::Keep => {
                            result.push(data[i]);
                            i += 1;
                            continue;
                        }
                    }
                }
                // ESC P — DCS (Device Control String)
                b'P' => {
                    if is_dcs_query(data, i + 2) {
                        // Skip the entire DCS query sequence
                        i += 2;
                        i = skip_dcs_sequence(data, i);
                        continue;
                    } else {
                        // DCS set/restore command — keep it
                        result.push(data[i]);
                        i += 1;
                        continue;
                    }
                }
                // Other ESC sequences (not queries) — keep
                _ => {
                    result.push(data[i]);
                    i += 1;
                    continue;
                }
            }
        }
        result.push(data[i]);
        i += 1;
    }

    result
}

/// Try to extract cwd from an OSC 7 sequence (starting after ESC ]).
/// OSC 7 format: 7;file://HOST/PATH (terminated by BEL or ST)
/// Returns the PATH part (URL-decoded), or None if not an OSC 7.
fn extract_osc7_cwd(data: &[u8], start: usize) -> Option<String> {
    if start + 2 >= data.len() || data[start] != b'7' || data[start + 1] != b';' {
        return None;
    }
    let mut i = start + 2;
    let content_start = i;
    let len = data.len();
    while i < len {
        if data[i] == 0x07 {
            break;
        } // BEL
        if i + 1 < len && data[i] == 0x1b && data[i + 1] == b'\\' {
            break;
        } // ST
        i += 1;
    }
    let content = &data[content_start..i];
    let content_str = std::str::from_utf8(content).ok()?;
    if let Some(rest) = content_str.strip_prefix("file://")
        && let Some(slash_pos) = rest.find('/')
    {
        return Some(percent_decode(&rest[slash_pos..]));
    }
    None
}

/// OSC 9;9 format: 9;9;PATH (terminated by BEL or ST)
/// This is the Windows Terminal cwd reporting format. PATH is a native
/// filesystem path (may use backslashes on Windows).
/// Returns the path with backslashes converted to forward slashes, or None.
fn extract_osc99_cwd(data: &[u8], start: usize) -> Option<String> {
    if start + 4 >= data.len()
        || data[start] != b'9'
        || data[start + 1] != b';'
        || data[start + 2] != b'9'
        || data[start + 3] != b';'
    {
        return None;
    }
    let mut i = start + 4;
    let content_start = i;
    let len = data.len();
    while i < len {
        if data[i] == 0x07 {
            break;
        } // BEL
        if i + 1 < len && data[i] == 0x1b && data[i + 1] == b'\\' {
            break;
        } // ST
        i += 1;
    }
    let content = &data[content_start..i];
    let content_str = std::str::from_utf8(content).ok()?;
    if content_str.is_empty() {
        return None;
    }
    // Convert Windows backslashes to forward slashes
    Some(content_str.replace('\\', "/"))
}

/// Read OSC content until terminator (BEL or ST), returning (content slice, index after terminator).
/// Returns None if the sequence is not terminated within the data.
fn read_osc_content(data: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let mut i = start;
    while i < data.len() {
        if data[i] == 0x07 {
            return Some((&data[start..i], i + 1));
        }
        if i + 1 < data.len() && data[i] == 0x1b && data[i + 1] == b'\\' {
            return Some((&data[start..i], i + 2));
        }
        i += 1;
    }
    None
}

/// Extract OSC 9 desktop notification: ESC ] 9 ; <message> BEL/ST
/// Note: the 9;9; cwd format is handled by extract_osc99_cwd first;
/// this function only matches 9; when the next byte is NOT '9'.
/// Returns Some(("Notification", message)) on success.
fn extract_osc9_notification(data: &[u8], start: usize) -> Option<(String, String)> {
    if start + 2 >= data.len() || data[start] != b'9' || data[start + 1] != b';' {
        return None;
    }
    if data[start + 2] == b'9' {
        return None;
    } // exclude 9;9; cwd format
    let (content, end) = read_osc_content(data, start + 2)?;
    let msg = std::str::from_utf8(content).ok()?;
    if msg.is_empty() {
        return None;
    }
    // Reject patterns that look like another OSC code's payload leaking in
    // (e.g. "4;0;#1e1e1e" from OSC 4 color, "12;..." from OSC 12).
    // A legitimate OSC 9 notification message should not start with
    // digits-semicolon-digit pattern.
    if let Some(b) = content.first() {
        if b.is_ascii_digit() {
            // Check for "N;N" pattern (digit ; digit) — very likely a
            // misidentified OSC sequence, not a real notification.
            let mut pos = 0;
            while pos < content.len() && content[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos < content.len() && content[pos] == b';' {
                let after = &content[pos + 1..];
                if after.first().is_some_and(|b| b.is_ascii_digit()) {
                    log::warn!(
                        "OSC 9 rejected (looks like OSC {} payload): msg={:?}, raw_osc={:?}",
                        content[0] as char,
                        msg,
                        &data[start.saturating_sub(2)..end.min(data.len())],
                    );
                    return None;
                }
            }
        }
    }
    log::info!("OSC 9 notification: msg={:?}", msg);
    Some(("Notification".into(), msg.to_string()))
}

/// Extract OSC 777 desktop notification: ESC ] 777 ; notify ; <title> ; <body> BEL/ST
/// This is the iTerm2/foot/kitty notification format.
/// Returns Some((title, body)) on success.
fn extract_osc777_notification(data: &[u8], start: usize) -> Option<(String, String)> {
    let prefix = b"777;notify;";
    if !data.get(start..start + prefix.len())?.starts_with(prefix) {
        return None;
    }
    let (content, _) = read_osc_content(data, start + prefix.len())?;
    let content_str = std::str::from_utf8(content).ok()?;
    let mut parts = content_str.splitn(2, ';');
    let title = parts.next()?.to_string();
    let body = parts.next().unwrap_or("").to_string();
    if title.is_empty() && body.is_empty() {
        return None;
    }
    Some((title, body))
}

/// Extract OSC 0/2 terminal title: ESC ] 0 ; <title> BEL/ST or ESC ] 2 ; <title> BEL/ST
/// OSC 0 sets both window title and icon name; OSC 2 sets window title only.
/// Returns Some(title) on success.
fn extract_osc_title(data: &[u8], start: usize) -> Option<String> {
    if start + 2 >= data.len() {
        return None;
    }
    if (data[start] != b'0' && data[start] != b'2') || data[start + 1] != b';' {
        return None;
    }
    let (content, _) = read_osc_content(data, start + 2)?;
    let title = std::str::from_utf8(content).ok()?;
    if title.is_empty() {
        return None;
    }
    Some(title.to_string())
}

/// Decode percent-encoded URL path (e.g., "My%20Projects" → "My Projects").
fn percent_decode(s: &str) -> String {
    let mut result = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            result.push(byte);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

/// Check if a DCS sequence (starting after ESC P) is a query.
/// DCS queries request information from the terminal:
/// - `+q` — XTGETTCAP (request termcap/terminfo capability)
/// - `$q` — DECRQSS (request selection or setting)
/// - `>q` — XTVERSION (request terminal version)
fn is_dcs_query(data: &[u8], start: usize) -> bool {
    let len = data.len();
    if start >= len {
        return false;
    }
    // Check for +q (XTGETTCAP), $q (DECRQSS), or >q (XTVERSION)
    start + 1 < len && data[start + 1] == b'q' && matches!(data[start], b'+' | b'$' | b'>')
}

enum CsiAction {
    Query, // Terminal will respond — skip
    Keep,  // Display sequence — keep
}

/// Classify a CSI sequence starting at position after ESC [.
/// Returns the action and the index after the sequence.
fn classify_csi(data: &[u8], start: usize) -> (CsiAction, usize) {
    let mut i = start;
    let len = data.len();

    // Skip parameter bytes (0x30-0x3F: digits, semicolons, etc.)
    while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
        i += 1;
    }

    // Skip intermediate bytes (0x20-0x2F: space, !, ", etc.)
    let intermediate_start = i;
    while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
        i += 1;
    }
    let has_intermediate = i > intermediate_start;

    // Final byte (0x40-0x7E)
    if i >= len {
        return (CsiAction::Keep, i);
    }

    let final_byte = data[i];
    i += 1;

    match final_byte {
        // Device Attributes (DA) — terminal responds
        b'c' => (CsiAction::Query, i),
        // Device Status Report (DSR) requests — terminal responds
        b'n' => (CsiAction::Query, i),
        // XTVERSION/DECRQM query — terminal responds (only without intermediate bytes)
        b'q' => {
            if has_intermediate {
                (CsiAction::Keep, i) // DECSCUSR — cursor style, not a query
            } else {
                (CsiAction::Query, i) // XTVERSION/DECRQM — terminal responds
            }
        }
        // All other CSI sequences — keep (colors, cursor, etc.)
        _ => (CsiAction::Keep, i),
    }
}

// ---------------------------------------------------------------------------
// unified_filter — single-pass query interception + query/response filtering
// ---------------------------------------------------------------------------

/// Unified single-pass filter that combines three operations:
/// 1. Intercept known queries (DA, DSR, XTVERSION) → generate synthetic responses
/// 2. Filter remaining query sequences (CSI, OSC, DCS queries)
/// 3. Filter response echoes (DA responses, CPR, DCS responses)
///
/// This replaces the previous three-pass approach and eliminates 2 heap allocations
/// and 2 full data copies per PTY output chunk.
fn unified_filter(data: &[u8], cursor_row: u16, cursor_col: u16) -> InterceptResult {
    let mut display_data = Vec::with_capacity(data.len());
    let mut responses = Vec::with_capacity(4);
    let mut has_screen_boundary = false;
    let mut has_focus_enable = false;
    let mut has_clear_scrollback = false;
    let mut any_filter = false; // true once we see Intercept/Filter/OSC-skip/DCS-skip
    let mut extracted_cwd = None;
    let mut extracted_notification = None;
    let mut extracted_title = None;
    let mut i = 0;
    let len = data.len();

    while i < len {
        if i + 1 < len && data[i] == 0x1b {
            match data[i + 1] {
                // ESC [ — CSI sequence
                b'[' => {
                    let (action, new_i) = classify_csi_unified(data, i + 2, cursor_row, cursor_col);
                    match action {
                        CsiUnifiedAction::Intercept { response } => {
                            any_filter = true;
                            i = new_i;
                            if let Some(resp) = response {
                                responses.push(resp);
                            }
                        }
                        CsiUnifiedAction::Filter => {
                            any_filter = true;
                            // Query or response echo — skip entirely
                            i = new_i;
                        }
                        CsiUnifiedAction::Keep {
                            screen_boundary: sb,
                            focus_enable: fe,
                            clear_scrollback: csb,
                        } => {
                            if sb {
                                has_screen_boundary = true;
                            }
                            if fe {
                                has_focus_enable = true;
                            }
                            if csb {
                                has_clear_scrollback = true;
                            }
                            display_data.push(data[i]);
                            i += 1;
                        }
                    }
                }
                // ESC ] — OSC sequence
                b']' => {
                    if is_osc_query(data, i + 2) {
                        any_filter = true;
                        // OSC query — skip
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else if let Some(cwd) = extract_osc7_cwd(data, i + 2) {
                        // OSC 7 — extract cwd, skip from display (metadata, not visible)
                        extracted_cwd = Some(cwd);
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else if let Some(cwd) = extract_osc99_cwd(data, i + 2) {
                        // OSC 9;9 — Windows Terminal cwd format, extract and skip
                        extracted_cwd = Some(cwd);
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else if let Some((title, body)) = extract_osc9_notification(data, i + 2) {
                        // OSC 9 — desktop notification, extract and skip
                        log::info!(
                            "OSC 9 notification extracted: title={:?} body={:?}",
                            title,
                            body
                        );
                        extracted_notification = Some((title, body));
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else if let Some((title, body)) = extract_osc777_notification(data, i + 2) {
                        // OSC 777 — iTerm2/foot notification, extract and skip
                        extracted_notification = Some((title, body));
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else if let Some(title) = extract_osc_title(data, i + 2) {
                        // OSC 0/2 — terminal title, extract and skip
                        extracted_title = Some(title);
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    } else {
                        // Unknown OSC set command — strip from remote display.
                        // wterm only supports OSC 0/2 (title) and OSC 9/777
                        // (notification). Other OSC sequences (e.g. OSC 4 set
                        // color palette, OSC 52 clipboard) are not handled by
                        // wterm and could leak partial text like "4;0;" if the
                        // ESC byte is lost in transit.
                        any_filter = true;
                        i += 2;
                        i = skip_osc_sequence(data, i);
                    }
                }
                // ESC P — DCS (Device Control String)
                b'P' => {
                    if is_intercept_dcs_query(data, i + 2) {
                        any_filter = true;
                        // DCS query we can intercept — skip and generate response
                        let response = generate_dcs_response(data, i + 2);
                        i += 2;
                        i = skip_dcs_sequence(data, i);
                        if let Some(resp) = response {
                            responses.push(resp);
                        }
                    } else if is_dcs_query(data, i + 2) {
                        any_filter = true;
                        // DCS query we can't intercept — skip to prevent response
                        i += 2;
                        i = skip_dcs_sequence(data, i);
                    } else if is_dcs_response(data, i + 2) {
                        any_filter = true;
                        // DCS response echo — skip
                        i += 2;
                        i = skip_dcs_sequence(data, i);
                    } else {
                        // DCS set/restore command — keep
                        display_data.push(data[i]);
                        i += 1;
                    }
                }
                // Other ESC sequences — keep
                _ => {
                    display_data.push(data[i]);
                    i += 1;
                }
            }
        } else {
            display_data.push(data[i]);
            i += 1;
        }
    }

    InterceptResult {
        display_data,
        responses,
        has_screen_boundary,
        has_clear_scrollback,
        has_focus_enable,
        was_filtered: any_filter,
        extracted_cwd,
        extracted_notification,
        extracted_title,
    }
}

/// Unified CSI classification that handles all three concerns:
/// - Intercept: known queries where we generate synthetic responses (DA, DSR, XTVERSION)
/// - Filter: other queries and response echoes that should be removed
/// - Keep: display sequences that pass through
enum CsiUnifiedAction {
    /// Known query — intercept, skip from output, optionally generate synthetic response.
    Intercept { response: Option<Vec<u8>> },
    /// Query or response echo — skip from output (no synthetic response needed).
    Filter,
    /// Display sequence — keep in output.
    Keep {
        screen_boundary: bool,
        focus_enable: bool,
        clear_scrollback: bool,
    },
}

/// Classify a CSI sequence for the unified single-pass filter.
/// Returns the action and the index after the sequence.
fn classify_csi_unified(
    data: &[u8],
    start: usize,
    cursor_row: u16,
    cursor_col: u16,
) -> (CsiUnifiedAction, usize) {
    let mut i = start;
    let len = data.len();
    let mut screen_boundary = false;
    let mut has_focus_enable = false;
    let mut has_clear_scrollback = false;

    // Collect parameter bytes (0x30-0x3F)
    let param_start = i;
    while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
        i += 1;
    }
    let params = &data[param_start..i];

    // Skip intermediate bytes (0x20-0x2F)
    let intermediate_start = i;
    while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
        i += 1;
    }
    let has_intermediate = i > intermediate_start;

    // Final byte (0x40-0x7E)
    if i >= len {
        return (
            CsiUnifiedAction::Keep {
                screen_boundary: false,
                focus_enable: false,
                clear_scrollback: false,
            },
            i,
        );
    }

    let final_byte = data[i];
    i += 1;

    // Detect screen boundary sequences
    match final_byte {
        // ED (Erase in Display) — screen clear
        b'J' if (params == b"2" || params == b"3") => {
            screen_boundary = true;
            has_clear_scrollback = true;
        }
        // SM/RM (Set/Reset Mode) — alternate screen
        b'h' | b'l' => {
            if params == b"?1049" {
                screen_boundary = true;
            }
            // Intercept focus tracking mode reset: \x1b[?1004l
            // Programs like vim/less disable focus tracking on startup.
            // We must prevent this to keep our focus detection working.
            if params == b"?1004" && final_byte == b'l' {
                return (CsiUnifiedAction::Filter, i);
            }
            // Detect focus tracking enable: \x1b[?1004h
            // When shell/program enables focus tracking, it wants to receive
            // focus events — we should pass them through to PTY instead of
            // consuming them ourselves.
            if params == b"?1004" && final_byte == b'h' {
                has_focus_enable = true;
            }
        }
        _ => {}
    }

    let keep = CsiUnifiedAction::Keep {
        screen_boundary,
        focus_enable: has_focus_enable,
        clear_scrollback: has_clear_scrollback,
    };

    match final_byte {
        // DA (Device Attributes)
        b'c' => {
            if is_da_query(params) {
                // Known query — intercept with synthetic response
                let response = generate_da_response(params);
                (CsiUnifiedAction::Intercept { response }, i)
            } else if is_da_response(params) {
                // DA response echo — filter out
                (CsiUnifiedAction::Filter, i)
            } else {
                (keep, i)
            }
        }
        // DSR (Device Status Report)
        b'n' => {
            if params == b"6" || params == b"5" {
                // Cursor position / device status query — intercept
                let response = generate_dsr_response(params, cursor_row, cursor_col);
                (CsiUnifiedAction::Intercept { response }, i)
            } else if is_cpr_response(params) {
                // CPR response echo — filter out
                (CsiUnifiedAction::Filter, i)
            } else {
                (keep, i)
            }
        }
        // XTVERSION / DECRQM query
        b'q' => {
            if has_intermediate {
                // DECSCUSR — cursor style, not a query
                (keep, i)
            } else if params.starts_with(b">") {
                // XTVERSION query — intercept with synthetic response
                let response = Some(b"\x1bP>|ShellRemote\x1b\\".to_vec());
                (CsiUnifiedAction::Intercept { response }, i)
            } else {
                // Other q-ending queries (DECRQM etc.) — filter out
                (CsiUnifiedAction::Filter, i)
            }
        }
        // All other CSI sequences — keep (colors, cursor movement, etc.)
        _ => (keep, i),
    }
}

/// Result of intercepting terminal queries from PTY output.
///
/// Following Zellij's approach: instead of letting queries reach the local
/// terminal (which responds → response echoed back → garbled text), we
/// intercept queries, generate synthetic responses, and write them directly
/// to the PTY with ECHO off. Queries are filtered from local stdout output.
pub struct InterceptResult {
    /// Data with queries and response echoes removed (for writing to local stdout).
    /// When `was_filtered` is false, this contains the same bytes as the input.
    pub display_data: Vec<u8>,
    /// Synthetic responses to write to PTY input (via `write_response`).
    pub responses: Vec<Vec<u8>>,
    /// True if data contains a screen-clearing sequence (e.g., CSI 2J, CSI H).
    /// Used to reset the replay buffer without a separate scan.
    pub has_screen_boundary: bool,
    /// True if data contains a scrollback-clearing sequence (CSI 3J or CSI 2J).
    /// When set, the virtual terminal's scrollback buffer should be cleared.
    pub has_clear_scrollback: bool,
    /// True if PTY output contained \x1b[?1004h (shell/program enabling focus tracking).
    /// When this is set, the StdinResponseFilter should pass focus events to the PTY
    /// instead of consuming them, because the shell wants to handle focus events itself.
    pub has_focus_enable: bool,
    /// True if any filtering occurred (queries or responses removed).
    /// When false, `display_data` is identical to the input data.
    pub was_filtered: bool,
    /// CWD extracted from OSC 7 sequence (e.g. from PROMPT_COMMAND injection).
    /// Set when the shell reports its current working directory via OSC 7.
    pub extracted_cwd: Option<String>,
    /// Notification extracted from OSC 9 or OSC 777 sequences.
    /// Contains (title, body). Set when the shell emits a desktop notification.
    pub extracted_notification: Option<(String, String)>,
    /// Terminal title extracted from OSC 0/2 sequences.
    /// Set when the shell sets its window title.
    pub extracted_title: Option<String>,
}

/// Stateful query interceptor that handles escape sequences split across reads.
///
/// PTY output may arrive in arbitrary chunks. An escape sequence like
/// `ESC P > q ESC \` (XTVERSION query) can be split as `ESC P` in one
/// read and `> q ESC \` in the next. A stateless parser would fail to
/// recognize the split sequence.
///
/// This struct buffers incomplete escape sequences at the end of a chunk
/// and prepends them to the next chunk, ensuring complete sequences are
/// always processed together.
///
/// Usage:
/// ```ignore
/// let mut interceptor = QueryInterceptor::new();
/// loop {
///     let data = reader.read_output().await;
///     let result = interceptor.process(&data);
///     // write result.display_data to local stdout
///     // write result.responses to PTY via write_response()
/// }
/// ```
pub struct QueryInterceptor {
    /// Buffered incomplete escape sequence from previous chunk.
    pending: Vec<u8>,
    /// Tracked cursor position (1-based). Updated based on PTY output
    /// so DSR cursor position queries can be answered accurately.
    cursor_row: u16,
    cursor_col: u16,
    /// Terminal dimensions (for cursor position clamping).
    term_rows: u16,
    term_cols: u16,
}

impl Default for QueryInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryInterceptor {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            cursor_row: 1,
            cursor_col: 1,
            term_rows: 24,
            term_cols: 80,
        }
    }

    /// Update the terminal dimensions (call on resize).
    pub fn set_terminal_size(&mut self, rows: u16, cols: u16) {
        self.term_rows = rows;
        self.term_cols = cols;
    }

    /// Process a chunk of PTY output data.
    ///
    /// Single-pass filtering that simultaneously:
    /// 1. Intercepts known queries (DA, DSR, XTVERSION) and generates synthetic responses
    /// 2. Filters all query sequences (CSI, OSC, DCS queries)
    /// 3. Filters response echoes (DA responses, CPR, DCS responses)
    ///
    /// This replaces the previous three-pass approach (intercept → filter_query → filter_response)
    /// which required 3 heap allocations and 3 full data copies per chunk.
    pub fn process(&mut self, data: &[u8]) -> InterceptResult {
        // Prepend any buffered incomplete sequence from previous chunk
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(data);

        // Find the start of the last incomplete escape sequence
        let split_pos = find_trailing_escape_start(&combined);

        let result = if split_pos < combined.len() {
            // Buffer the incomplete part for next call
            self.pending = combined[split_pos..].to_vec();
            unified_filter(&combined[..split_pos], self.cursor_row, self.cursor_col)
        } else {
            unified_filter(&combined, self.cursor_row, self.cursor_col)
        };

        // Update cursor position based on the filtered display data
        self.update_cursor(&result.display_data);

        result
    }

    /// Update tracked cursor position based on output data.
    /// This gives us a reasonable estimate for responding to DSR queries.
    fn update_cursor(&mut self, data: &[u8]) {
        let mut i = 0;
        let len = data.len();
        while i < len {
            if data[i] == 0x1b && i + 1 < len {
                match data[i + 1] {
                    // CSI sequence — check for cursor movement
                    b'[' => {
                        let (_, new_i) = self.update_cursor_csi(data, i + 2);
                        i = new_i;
                        continue;
                    }
                    // Skip other ESC sequences (2 bytes)
                    _ => {
                        i += 2;
                        continue;
                    }
                }
            }
            match data[i] {
                b'\n' => {
                    self.cursor_row = self.cursor_row.saturating_add(1);
                    // Cursor stays at current column after LF (terminal does auto-CR)
                }
                b'\r' => {
                    self.cursor_col = 1;
                }
                b'\t' => {
                    // Advance to next tab stop (every 8 columns)
                    let next_tab = (self.cursor_col as usize).div_ceil(8) * 8 + 1;
                    self.cursor_col = next_tab.min(self.term_cols as usize) as u16;
                }
                b'\x08' => {
                    // Backspace
                    self.cursor_col = self.cursor_col.saturating_sub(1).max(1);
                }
                _ => {
                    // Printable character — advance cursor
                    if data[i] >= 0x20 {
                        self.cursor_col = self.cursor_col.saturating_add(1);
                        if self.cursor_col > self.term_cols {
                            // Line wrap
                            self.cursor_col = 1;
                            self.cursor_row = self.cursor_row.saturating_add(1);
                        }
                    }
                }
            }
            i += 1;
        }
        // Clamp row to terminal height (with 1 extra for scroll)
        if self.cursor_row > self.term_rows + 1 {
            self.cursor_row = self.term_rows + 1;
        }
    }

    /// Handle cursor movement CSI sequences.
    fn update_cursor_csi(&mut self, data: &[u8], start: usize) -> ((), usize) {
        let mut i = start;
        let len = data.len();

        // Collect parameter bytes
        let param_start = i;
        while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
            i += 1;
        }
        let params = &data[param_start..i];

        // Skip intermediate bytes
        while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
            i += 1;
        }

        // Final byte
        if i >= len {
            return ((), i);
        }
        let final_byte = data[i];
        i += 1;

        match final_byte {
            // CUP — Cursor Position: ESC [ row ; col H  (also ESC [ H = home)
            b'H' | b'f' => {
                let (row, col) = parse_two_params(params, 1, 1);
                self.cursor_row = row.max(1);
                self.cursor_col = col.max(1);
            }
            // CUU — Cursor Up: ESC [ n A
            b'A' => {
                let n = parse_one_param(params, 1);
                self.cursor_row = self.cursor_row.saturating_sub(n).max(1);
            }
            // CUD — Cursor Down: ESC [ n B
            b'B' => {
                let n = parse_one_param(params, 1);
                self.cursor_row = self.cursor_row.saturating_add(n);
            }
            // CUF — Cursor Forward: ESC [ n C
            b'C' => {
                let n = parse_one_param(params, 1);
                self.cursor_col = self.cursor_col.saturating_add(n).min(self.term_cols);
            }
            // CUB — Cursor Back: ESC [ n D
            b'D' => {
                let n = parse_one_param(params, 1);
                self.cursor_col = self.cursor_col.saturating_sub(n).max(1);
            }
            // ED — Erase in Display: ESC [ 2 J = clear screen → reset cursor
            b'J' if (params == b"2" || params == b"3") => {
                self.cursor_row = 1;
                self.cursor_col = 1;
            }
            // EL — Erase in Line: ESC [ K etc. — cursor stays, no movement
            b'K' => {}
            _ => {}
        }
        ((), i)
    }
}

/// Parse a single numeric parameter from CSI params, returning `default` if absent.
fn parse_one_param(params: &[u8], default: u16) -> u16 {
    if params.is_empty() {
        return default;
    }
    std::str::from_utf8(params)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(default)
}

/// Parse two semicolon-separated numeric parameters from CSI params.
/// Returns (default1, default2) if params are absent.
fn parse_two_params(params: &[u8], default1: u16, default2: u16) -> (u16, u16) {
    if params.is_empty() {
        return (default1, default2);
    }
    let s = std::str::from_utf8(params).unwrap_or("");
    let mut parts = s.split(';');
    let p1 = parts
        .next()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default1);
    let p2 = parts
        .next()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default2);
    (p1, p2)
}

/// Find the start position of a trailing incomplete escape sequence.
///
/// Scans backwards from the end of data to find an `ESC` byte that starts
/// an incomplete sequence. Returns the index where the incomplete sequence
/// starts, or `data.len()` if the data is complete.
///
/// Incomplete patterns:
/// - Trailing `ESC` alone (0x1b)
/// - Trailing `ESC [` without final byte (incomplete CSI)
/// - Trailing `ESC P` without ST (incomplete DCS)
/// - Trailing `ESC ]` without ST/BEL (incomplete OSC)
fn find_trailing_escape_start(data: &[u8]) -> usize {
    let len = data.len();
    if len == 0 {
        return 0;
    }

    // Scan from the end to find the last ESC byte
    let mut i = len;
    while i > 0 {
        i -= 1;
        if data[i] == 0x1b {
            // Found ESC at position i — check if the sequence starting here is complete
            if is_sequence_complete(data, i) {
                // This sequence is complete, keep scanning backwards
                continue;
            } else {
                // Incomplete sequence starting at i
                return i;
            }
        }
    }

    // No incomplete ESC sequence found
    len
}

/// Check if the escape sequence starting at `start` is complete within `data`.
fn is_sequence_complete(data: &[u8], start: usize) -> bool {
    let len = data.len();
    if start >= len {
        return true;
    }

    if data[start] != 0x1b {
        return true; // Not an escape sequence
    }

    if start + 1 >= len {
        return false; // ESC alone — incomplete
    }

    match data[start + 1] {
        // CSI: ESC [ [params] [intermediates] <final>
        b'[' => {
            let mut j = start + 2;
            // Skip parameter bytes (0x30-0x3F)
            while j < len && data[j] >= 0x30 && data[j] <= 0x3F {
                j += 1;
            }
            // Skip intermediate bytes (0x20-0x2F)
            while j < len && data[j] >= 0x20 && data[j] <= 0x2F {
                j += 1;
            }
            // Need final byte (0x40-0x7E)
            j < len && data[j] >= 0x40 && data[j] <= 0x7E
        }
        // DCS: ESC P ... ST
        b'P' => {
            // Look for ST (ESC \) or BEL
            let mut j = start + 2;
            while j < len {
                if data[j] == 0x07 {
                    return true; // BEL terminates DCS
                }
                if j + 1 < len && data[j] == 0x1b && data[j + 1] == b'\\' {
                    return true; // ST terminates DCS
                }
                j += 1;
            }
            false // No terminator found — incomplete
        }
        // OSC: ESC ] ... ST/BEL
        b']' => {
            let mut j = start + 2;
            while j < len {
                if data[j] == 0x07 {
                    return true; // BEL terminates OSC
                }
                if j + 1 < len && data[j] == 0x1b && data[j + 1] == b'\\' {
                    return true; // ST terminates OSC
                }
                j += 1;
            }
            false // No terminator found — incomplete
        }
        // SS2, SS3: ESC N / ESC O — 2-byte sequences, always complete
        b'N' | b'O' => true,
        // Other ESC sequences: ESC <byte> — 2-byte, always complete
        _ => true,
    }
}

/// Check if DA parameters indicate a query (not a response).
///
/// DA1 query: params are empty or just digits (e.g., `0`, ``)
/// DA2 query: params are just `>` or `>0`
///
/// DA1 response: params contain `?` (e.g., `?64;1;2;6;9;15;16;17;18;21;22`)
/// DA2 response: params start with `>` followed by semicolons/digits (e.g., `>1;3;1`)
fn is_da_query(params: &[u8]) -> bool {
    if params.is_empty() {
        return true; // ESC[c — DA1 query
    }

    // Contains '?' — this is a DA1 response, not a query
    if params.contains(&b'?') {
        return false;
    }

    // Starts with '>'
    if params[0] == b'>' {
        // DA2 query: just ">" or ">0"
        // DA2 response: ">1;3;1" etc.
        if params.len() == 1 {
            return true; // ESC[>c — DA2 query
        }
        let after_gt = &params[1..];
        // If there's a semicolon, it's a response
        if after_gt.contains(&b';') {
            return false;
        }
        // If after '>' is just '0', it's a query
        if after_gt.len() == 1 && after_gt[0] == b'0' {
            return true; // ESC[>0c — DA2 query
        }
        // Non-zero digit after '>' without semicolon: likely a response
        return false;
    }

    // Just digits (like "0") — DA1 query
    if params.iter().all(|b| b.is_ascii_digit()) {
        return true;
    }

    // Unknown pattern — don't intercept (safe default)
    false
}

/// Check if a DCS sequence should be intercepted.
fn is_intercept_dcs_query(data: &[u8], start: usize) -> bool {
    let len = data.len();
    if start >= len {
        return false;
    }
    // XTVERSION query: >q
    if data[start] == b'>' && start + 1 < len && data[start + 1] == b'q' {
        return true;
    }
    // XTGETTCAP query: +q
    // DECRQSS query: $q
    // These are harder to generate synthetic responses for — let them through
    // and rely on filter_response_echoes for any echo artifacts.
    false
}

/// Generate a synthetic DA (Device Attributes) response.
///
/// We claim VT220 (level 2) with some common extensions, similar to Zellij.
/// This is only called for queries (not responses), so no need to check for `?`.
fn generate_da_response(params: &[u8]) -> Option<Vec<u8>> {
    if params.starts_with(b">") {
        // DA2 query (Secondary DA): ESC[>c or ESC[>0c
        // Respond as VT220, version 0.0.1
        Some(b"\x1b[>0;0;1c".to_vec())
    } else {
        // DA1 query (Primary DA): ESC[c or ESC[0c
        // Respond as VT220 with extensions: service class 2, with these extensions:
        // 1=132 columns, 2=printer, 6=selective erase, 9=national charset,
        // 15=technical charset, 16=user defined keys, 17=downline loadable chars,
        // 18=user defined keys downline loadable, 21=horizontal scrolling,
        // 22=has color
        Some(b"\x1b[?62;1;2;6;9;15;16;17;18;21;22c".to_vec())
    }
}

/// Generate a synthetic DSR (Device Status Report) response.
fn generate_dsr_response(params: &[u8], cursor_row: u16, cursor_col: u16) -> Option<Vec<u8>> {
    if params == b"6" {
        // Cursor position report — respond with tracked cursor position.
        // This is more accurate than the old hardcoded (1,1) response,
        // which caused fish shell to add spurious "⏎" line-break indicators.
        Some(format!("\x1b[{};{}R", cursor_row, cursor_col).into_bytes())
    } else if params == b"5" {
        // Device status: OK
        Some(b"\x1b[0n".to_vec())
    } else {
        None
    }
}

/// Generate a synthetic DCS response.
fn generate_dcs_response(data: &[u8], start: usize) -> Option<Vec<u8>> {
    let len = data.len();
    if start >= len {
        return None;
    }

    // XTVERSION query: >q
    if data[start] == b'>' && start + 1 < len && data[start + 1] == b'q' {
        // Respond with our version info
        Some(b"\x1bP>|ShellRemote\x1b\\".to_vec())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// write_local_stdout
// ---------------------------------------------------------------------------

// Thread-local pending buffer for the stateful stdout filter.
// Holds incomplete escape sequences at the end of a write that
// need to be prepended to the next write.
//
// NOTE: This relies on the agent's main loop running on a single thread.
// If the runtime migrates the task to a different OS thread, the pending
// buffer state will be lost. In practice, the agent's select! loop is
// single-task and stays on one thread, but this is an implicit assumption.
// If multi-threaded output becomes necessary, refactor to pass the buffer
// explicitly or use tokio::task_local! with async calls.
thread_local! {
    static STDOUT_PENDING: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Write data to local stdout synchronously.
///
/// Uses a stateful final defense filter that:
/// 1. Buffers incomplete escape sequences at the end of a write
/// 2. Prepends buffered data to the next write
/// 3. Strips ALL query and response escape sequences before writing
///
/// This ensures that even if an escape sequence is split across
/// two write_local_stdout calls (e.g., `\x1b` at end of one chunk
/// and `P>q\x1b\\` at start of next), it will still be filtered.
pub fn write_local_stdout(data: &[u8]) -> std::io::Result<()> {
    STDOUT_PENDING.with(|pending_cell| {
        let mut pending = pending_cell.borrow_mut();

        // Prepend any buffered incomplete sequence from previous write
        let mut combined = std::mem::take(&mut *pending);
        combined.extend_from_slice(data);

        // Find the start of any trailing incomplete escape sequence
        let split_pos = find_trailing_escape_start(&combined);

        let to_write = if split_pos < combined.len() {
            // Buffer the incomplete part for next call
            *pending = combined[split_pos..].to_vec();
            combined.truncate(split_pos);
            combined
        } else {
            combined
        };

        // Apply final defense filter to complete data (owned version avoids allocation when no ESC)
        let original_len = to_write.len();
        let filtered = final_defense_filter_owned(to_write);

        if filtered.len() != original_len {
            let diff = original_len - filtered.len();
            log::debug!(
                "write_local_stdout: final defense removed {} bytes (pending={})",
                diff,
                pending.len()
            );
        }

        if filtered.is_empty() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            unsafe {
                libc::write(1, filtered.as_ptr() as *const libc::c_void, filtered.len());
            }
        }
        #[cfg(not(unix))]
        {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&filtered)?;
            stdout.flush()?;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_mode_guard() {
        // Should succeed on a TTY, or gracefully return None
        let guard = RawModeGuard::enter();
        if std::io::stdin().is_terminal() {
            assert!(guard.is_some());
        }
        // Guard dropped here, raw mode restored
    }

    #[test]
    fn test_stdin_reader_new() {
        let _reader = StdinReader::new();
        // If TTY, inner should be Some; otherwise None
        if std::io::stdin().is_terminal() {
            assert!(_reader.inner.is_some());
        }
    }

    #[test]
    fn test_resize_watcher_new() {
        let _watcher = ResizeWatcher::new();
        // Should not panic
    }

    #[test]
    fn test_write_local_stdout() {
        // Writing empty data should always succeed
        write_local_stdout(&[]).unwrap();
    }

    #[test]
    fn test_filter_query_passthrough() {
        // Normal text passes through unchanged
        let input = b"hello world".to_vec();
        assert_eq!(filter_query_sequences(&input), input);
    }

    #[test]
    fn test_filter_osc_query_stripped() {
        // OSC 11 query: ESC ] 11 ; ? BEL — should be stripped
        let input = b"some text\x1b]11;?\x07more text";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"some textmore text");
    }

    #[test]
    fn test_filter_osc_set_kept() {
        // OSC 0 set title: ESC ] 0 ; title BEL — should be KEPT (not a query)
        let input = b"\x1b]0;my title\x07hello";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"\x1b]0;my title\x07hello");
    }

    #[test]
    fn test_filter_osc_query_st() {
        // OSC 11 query with ST terminator: ESC ] 11 ; ? ESC \
        let input = b"abc\x1b]11;?\x1b\\def";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"abcdef");
    }

    #[test]
    fn test_filter_csi_da_query() {
        // CSI DA query: ESC [ c — should be stripped
        let input = b"hello\x1b[cworld";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"helloworld");
    }

    #[test]
    fn test_filter_csi_dsr_query() {
        // CSI DSR query: ESC [ 6 n — should be stripped
        let input = b"hello\x1b[6nworld";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"helloworld");
    }

    #[test]
    fn test_filter_csi_color_kept() {
        // CSI color set: ESC [ 31 m — should be KEPT
        let input = b"\x1b[31mred text";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"\x1b[31mred text");
    }

    #[test]
    fn test_filter_esc_arrow_keys_kept() {
        // ESC [ A — arrow key sequence (not a query) — should be KEPT
        let input = b"\x1b[A\x1b[B";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, input);
    }

    #[test]
    fn test_filter_multiple_queries() {
        // Multiple OSC queries mixed with regular text
        let input = b"\x1b]11;?\x07hello\x1b]10;?\x1b\\world\x1b[c";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"helloworld");
    }

    // --- Tests for DCS query filtering ---

    #[test]
    fn test_filter_dcs_xtgettcap_stripped() {
        // DCS XTGETTCAP: ESC P + q <hex-caps> ST — should be stripped
        let input = b"text\x1bP+q636f6c6f7273\x1b\\more";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"textmore");
    }

    #[test]
    fn test_filter_dcs_decrqss_stripped() {
        // DCS DECRQSS: ESC P $ q <params> ST — should be stripped
        let input = b"text\x1bP$q\"p\x1b\\more";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"textmore");
    }

    #[test]
    fn test_filter_dcs_set_kept() {
        // DCS set/restore command (not a query): ESC P ! r ... ST — should be KEPT
        let input = b"\x1bP!r123\x1b\\hello";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"\x1bP!r123\x1b\\hello");
    }

    #[test]
    fn test_filter_dcs_xtgettcap_bel_terminator() {
        // DCS XTGETTCAP with BEL terminator
        let input = b"abc\x1bP+q636f6c6f7273\x07def";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"abcdef");
    }

    #[test]
    fn test_filter_mixed_osc_csi_dcs_queries() {
        // Mix of OSC, CSI, and DCS queries
        let input = b"\x1b]11;?\x07hello\x1b[6nworld\x1bP+q636f6c6f7273\x1b\\end";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"helloworldend");
    }

    #[test]
    fn test_filter_csi_mouse_mode_kept() {
        // Mouse tracking mode set: ESC [ ? 1000 h — should be KEPT (not a query)
        let input = b"\x1b[?1000h\x1b[?1002l";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"\x1b[?1000h\x1b[?1002l");
    }

    #[test]
    fn test_filter_sgr_mouse_kept() {
        // SGR mouse event: ESC [ < 0 ; 35 ; 10 M — should be KEPT (not a query)
        let input = b"\x1b[<0;35;10M";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"\x1b[<0;35;10M");
    }

    // --- Tests for filter_response_echoes ---

    #[test]
    fn test_filter_response_da1_echo_stripped() {
        // DA1 response echo: ESC [ ? 64 ; 1 ; 2 ; 6 c — should be stripped
        let input = b"hello\x1b[?64;1;2;6cworld";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"helloworld");
    }

    #[test]
    fn test_filter_response_da2_echo_stripped() {
        // DA2 response echo: ESC [ > 1 ; 3 ; 1 c — should be stripped (ghostty-style)
        let input = b"prompt\x1b[>1;3;1cmore";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"promptmore");
    }

    #[test]
    fn test_filter_response_da2_short_echo_stripped() {
        // DA2 short response: ESC [ > 1 c — should be stripped
        let input = b"text\x1b[>1cend";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"textend");
    }

    #[test]
    fn test_filter_response_cpr_echo_stripped() {
        // Cursor Position Report: ESC [ 10 ; 20 R — should be stripped
        let input = b"abc\x1b[10;20Rdef";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"abcdef");
    }

    #[test]
    fn test_filter_response_da1_query_kept() {
        // DA1 query: ESC [ c — should be KEPT (terminal needs to respond)
        let input = b"\x1b[chello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[chello");
    }

    #[test]
    fn test_filter_response_da1_zero_query_kept() {
        // DA1 query: ESC [ 0 c — should be KEPT
        let input = b"\x1b[0chello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[0chello");
    }

    #[test]
    fn test_filter_response_da2_query_kept() {
        // DA2 query: ESC [ > c — should be KEPT (terminal needs to respond)
        let input = b"\x1b[>chello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[>chello");
    }

    #[test]
    fn test_filter_response_da2_zero_query_kept() {
        // DA2 query: ESC [ > 0 c — should be KEPT
        let input = b"\x1b[>0chello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[>0chello");
    }

    #[test]
    fn test_filter_response_color_kept() {
        // CSI color set: ESC [ 31 m — should be KEPT
        let input = b"\x1b[31mred text";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[31mred text");
    }

    #[test]
    fn test_filter_response_normal_text_kept() {
        // Normal text passes through unchanged
        let input = b"hello world";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, input);
    }

    #[test]
    fn test_filter_response_dsr_query_kept() {
        // DSR query: ESC [ 6 n — should be KEPT
        let input = b"\x1b[6nhello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[6nhello");
    }

    #[test]
    fn test_filter_response_multiple_echoes() {
        // Multiple response echoes mixed with text
        let input = b"prompt\x1b[>1;3;1cmid\x1b[?64;1;2cend";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"promptmidend");
    }

    #[test]
    fn test_filter_response_da_query_and_echo_together() {
        // DA query followed by its echo — query should be kept, echo stripped
        let input = b"\x1b[c\x1b[?64;1;2;6c";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[c");
    }

    #[test]
    fn test_filter_response_cpr_without_semicolon_kept() {
        // ESC [ 10 R — not a valid CPR (no semicolon), should be kept
        let input = b"\x1b[10Rhello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[10Rhello");
    }

    #[test]
    fn test_filter_response_mouse_mode_kept() {
        // Mouse tracking: ESC [ ? 1000 h — should be KEPT
        let input = b"\x1b[?1000h\x1b[?1002l";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[?1000h\x1b[?1002l");
    }

    #[test]
    fn test_filter_response_sgr_mouse_kept() {
        // SGR mouse event: ESC [ < 0 ; 35 ; 10 M — should be KEPT
        let input = b"\x1b[<0;35;10M";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1b[<0;35;10M");
    }

    #[test]
    fn test_filter_response_dcs_xtversion_echo_stripped() {
        // XTVERSION response echo: ESC P > | ghostty 1.3.1 ST — should be stripped
        let input = b"prompt\x1bP>|ghostty 1.3.1\x1b\\more";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"promptmore");
    }

    #[test]
    fn test_filter_response_dcs_xtversion_bel_stripped() {
        // XTVERSION response with BEL terminator
        let input = b"abc\x1bP>|ghostty 1.3.1\x07def";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"abcdef");
    }

    #[test]
    fn test_filter_response_dcs_xtgettcap_echo_stripped() {
        // XTGETTCAP response: ESC P 1 + r <hex> ST — should be stripped
        let input = b"text\x1bP1+r636f6c6f7273\x1b\\more";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"textmore");
    }

    #[test]
    fn test_filter_response_dcs_decrqss_echo_stripped() {
        // DECRQSS response: ESC P 1 $ r <params> ST — should be stripped
        let input = b"text\x1bP1$r\"p\x1b\\more";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"textmore");
    }

    #[test]
    fn test_filter_response_dcs_query_kept() {
        // DCS queries should be KEPT (local terminal needs to respond)
        // XTGETTCAP query: ESC P + q ... ST
        let input = b"\x1bP+q636f6c6f7273\x1b\\hello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1bP+q636f6c6f7273\x1b\\hello");
    }

    #[test]
    fn test_filter_response_dcs_xtversion_query_kept() {
        // XTVERSION query: ESC P > q ST — should be KEPT
        let input = b"\x1bP>q\x1b\\hello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1bP>q\x1b\\hello");
    }

    #[test]
    fn test_filter_response_dcs_decrqss_query_kept() {
        // DECRQSS query: ESC P $ q <params> ST — should be KEPT
        let input = b"\x1bP$q\"p\x1b\\hello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1bP$q\"p\x1b\\hello");
    }

    #[test]
    fn test_filter_response_dcs_set_kept() {
        // DCS set command: ESC P ! r ... ST — should be KEPT
        let input = b"\x1bP!r123\x1b\\hello";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"\x1bP!r123\x1b\\hello");
    }

    #[test]
    fn test_filter_response_combined_csi_dcs_echoes() {
        // DA2 response echo + XTVERSION response echo mixed with text
        let input = b"prompt\x1b[>1;3;1cmid\x1bP>|ghostty 1.3.1\x1b\\end";
        let filtered = filter_response_echoes(input);
        assert_eq!(filtered, b"promptmidend");
    }

    // --- Update existing filter_query_sequences test for XTVERSION ---

    #[test]
    fn test_filter_dcs_xtversion_query_stripped() {
        // XTVERSION query: ESC P > q ST — should be stripped by filter_query_sequences
        let input = b"text\x1bP>q\x1b\\more";
        let filtered = filter_query_sequences(input);
        assert_eq!(filtered, b"textmore");
    }

    // --- Tests for unified_filter (single-pass) ---

    #[test]
    fn test_intercept_da1_query() {
        // DA1 query: ESC [ c — should be intercepted, synthetic response generated
        let input = b"\x1b[c";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
        assert_eq!(result.responses[0], b"\x1b[?62;1;2;6;9;15;16;17;18;21;22c");
    }

    #[test]
    fn test_intercept_da1_zero_query() {
        // DA1 query: ESC [ 0 c
        let input = b"\x1b[0c";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
    }

    #[test]
    fn test_intercept_da2_query() {
        // DA2 query: ESC [ > c
        let input = b"\x1b[>c";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
        assert_eq!(result.responses[0], b"\x1b[>0;0;1c");
    }

    #[test]
    fn test_intercept_da2_zero_query() {
        // DA2 query: ESC [ > 0 c
        let input = b"\x1b[>0c";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
    }

    #[test]
    fn test_intercept_dsr_cursor_query() {
        // DSR cursor position query: ESC [ 6 n
        let input = b"\x1b[6n";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
        assert_eq!(result.responses[0], b"\x1b[1;1R");
    }

    #[test]
    fn test_intercept_xtversion_query() {
        // XTVERSION query: ESC P > q ST
        let input = b"\x1bP>q\x1b\\";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert_eq!(result.responses.len(), 1);
        assert_eq!(result.responses[0], b"\x1bP>|ShellRemote\x1b\\");
    }

    #[test]
    fn test_intercept_mixed_with_text() {
        // Queries mixed with regular text
        let input = b"hello\x1b[cworld\x1b[6nend";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"helloworldend");
        assert_eq!(result.responses.len(), 2);
    }

    #[test]
    fn test_intercept_csi_color_passthrough() {
        // CSI color: ESC [ 31 m — should pass through
        let input = b"\x1b[31mred text";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"\x1b[31mred text");
        assert!(result.responses.is_empty());
    }

    #[test]
    fn test_intercept_normal_text_passthrough() {
        let input = b"hello world";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"hello world");
        assert!(result.responses.is_empty());
    }

    #[test]
    fn test_intercept_dcs_xtgettcap_filtered() {
        // XTGETTCAP query: ESC P + q — filtered out (would cause terminal response)
        let input = b"\x1bP+q636f6c6f7273\x1b\\hello";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"hello");
        assert!(result.responses.is_empty());
    }

    #[test]
    fn test_intercept_da_response_filtered() {
        // DA1 response: ESC [ ? 64 ; 1 ; 2 c — filtered out (response echo)
        let input = b"\x1b[?64;1;2c";
        let result = unified_filter(input, 1, 1);
        assert_eq!(result.display_data, b"");
        assert!(result.responses.is_empty());
    }

    // --- Tests for QueryInterceptor (split sequence handling) ---

    #[test]
    fn test_interceptor_split_dcs_query() {
        // XTVERSION query split across two chunks: ESC P | > q ESC \
        let mut interceptor = QueryInterceptor::new();
        let r1 = interceptor.process(b"prompt\x1bP");
        // First chunk: ESC P is incomplete, buffered
        assert_eq!(r1.display_data, b"prompt");
        assert!(r1.responses.is_empty());

        let r2 = interceptor.process(b">q\x1b\\more");
        // Second chunk: >q completes the XTVERSION query → intercepted
        assert_eq!(r2.display_data, b"more");
        assert_eq!(r2.responses.len(), 1);
        assert_eq!(r2.responses[0], b"\x1bP>|ShellRemote\x1b\\");
    }

    #[test]
    fn test_interceptor_split_csi_query() {
        // DA1 query split: ESC [ | c
        let mut interceptor = QueryInterceptor::new();
        let r1 = interceptor.process(b"hello\x1b[");
        assert_eq!(r1.display_data, b"hello");
        assert!(r1.responses.is_empty());

        let r2 = interceptor.process(b"cmore");
        assert_eq!(r2.display_data, b"more");
        assert_eq!(r2.responses.len(), 1);
    }

    #[test]
    fn test_interceptor_split_dcs_response_echo() {
        // XTVERSION response echo split: ESC P | > | ghostty 1.3.1 ESC \
        let mut interceptor = QueryInterceptor::new();
        let r1 = interceptor.process(b"\x1bP");
        assert!(r1.responses.is_empty());

        let r2 = interceptor.process(b">|ghostty 1.3.1\x1b\\text");
        // Response echo should be filtered by filter_response_echoes
        assert_eq!(r2.display_data, b"text");
        assert!(r2.responses.is_empty());
    }

    #[test]
    fn test_interceptor_complete_sequence_not_split() {
        // Complete sequence in one chunk — no buffering needed
        let mut interceptor = QueryInterceptor::new();
        let r = interceptor.process(b"hello\x1b[cworld");
        assert_eq!(r.display_data, b"helloworld");
        assert_eq!(r.responses.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// filter_stdin_responses — filter terminal responses from stdin data
// ---------------------------------------------------------------------------

/// Stateful filter that removes terminal response sequences from stdin data.
///
/// When the local terminal receives a query (DA1, DA2, XTVERSION, etc.) from
/// the PTY output, it responds via stdin. These responses should NOT be written
/// to the PTY input, because:
/// 1. The shell already gets synthetic responses from our query interceptor
/// 2. PTY ECHO would echo these responses back, creating garbled text
///
/// This filter removes:
/// - DA1 responses: ESC [ ? <params> c
/// - DA2 responses: ESC [ > <params> c  (with semicolons)
/// - CPR responses: ESC [ <row> ; <col> R
/// - DCS responses: ESC P > | ... ST, ESC P 1 + r ... ST, ESC P 1 $ r ... ST
///
/// It preserves all user keystrokes, including ESC-based key sequences
/// (arrow keys, function keys, etc.).
pub struct StdinResponseFilter {
    /// Buffered incomplete escape sequence from previous chunk.
    pending: Vec<u8>,
    /// Detected focus event during last filter() call.
    /// Some(true) = focus gained, Some(false) = focus lost, None = no focus event.
    last_focus_event: Option<bool>,
    /// Whether the shell/program has enabled focus tracking via \x1b[?1004h.
    /// When true, focus events should be passed through to the PTY (Keep),
    /// because the shell wants to handle them itself.
    /// When false (default), we consume focus events ourselves.
    shell_focus_enabled: bool,
}

impl Default for StdinResponseFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StdinResponseFilter {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            last_focus_event: None,
            shell_focus_enabled: false,
        }
    }

    /// Filter terminal responses from stdin data.
    /// Returns the filtered data (user keystrokes only).
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        self.last_focus_event = None;
        // Prepend any buffered incomplete sequence
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(data);

        let shell_focus = self.shell_focus_enabled;
        let split_pos = find_trailing_escape_start(&combined);
        if split_pos < combined.len() {
            self.pending = combined[split_pos..].to_vec();
            filter_stdin_responses_inner(
                &combined[..split_pos],
                &mut self.last_focus_event,
                shell_focus,
            )
        } else {
            filter_stdin_responses_inner(&combined, &mut self.last_focus_event, shell_focus)
        }
    }

    /// Set whether the shell has enabled focus tracking via \x1b[?1004h.
    /// When true, focus events are passed through to the PTY.
    /// When false, focus events are consumed by us.
    pub fn set_shell_focus_enabled(&mut self, enabled: bool) {
        self.shell_focus_enabled = enabled;
    }

    /// Take the detected focus event (if any) from the last filter() call.
    /// Returns Some(true) for focus gained, Some(false) for focus lost, None if none.
    pub fn take_focus_event(&mut self) -> Option<bool> {
        self.last_focus_event.take()
    }
}

impl Drop for StdinResponseFilter {
    fn drop(&mut self) {
        // Pending buffer contains stdin data (user keystrokes), not stdout.
        // Writing it to stdout would echo keystrokes to the terminal.
        // Since we have no PTY writer reference here, the safest action
        // is to discard the pending data — it's typically an incomplete
        // escape sequence at session close and losing it is acceptable.
        if !self.pending.is_empty() {
            log::debug!(
                "StdinResponseFilter dropped with {} bytes pending (discarded)",
                self.pending.len()
            );
        }
    }
}

/// Stateless filter for terminal responses in stdin data.
fn filter_stdin_responses_inner(
    data: &[u8],
    focus_event: &mut Option<bool>,
    shell_focus_enabled: bool,
) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();

    while i < len {
        if i + 1 < len && data[i] == 0x1b {
            match data[i + 1] {
                // ESC [ — CSI sequence
                b'[' => {
                    let (action, new_i) = classify_stdin_csi(data, i + 2, shell_focus_enabled);
                    match action {
                        StdinCsiAction::Response => {
                            // Terminal response — skip it
                            i = new_i;
                        }
                        StdinCsiAction::FocusGained => {
                            *focus_event = Some(true);
                            i = new_i;
                        }
                        StdinCsiAction::FocusLost => {
                            *focus_event = Some(false);
                            i = new_i;
                        }
                        StdinCsiAction::FocusGainedKeep => {
                            // Shell wants focus events — pass through AND capture
                            *focus_event = Some(true);
                            result.push(data[i]);
                            i += 1;
                        }
                        StdinCsiAction::FocusLostKeep => {
                            // Shell wants focus events — pass through AND capture
                            *focus_event = Some(false);
                            result.push(data[i]);
                            i += 1;
                        }
                        StdinCsiAction::Keep => {
                            result.push(data[i]);
                            i += 1;
                        }
                    }
                }
                // ESC P — DCS (Device Control String)
                // ALL DCS sequences in stdin are terminal responses.
                // User keypresses NEVER generate DCS sequences.
                // Filter them all to prevent multi-client response storms.
                b'P' => {
                    i += 2;
                    i = skip_dcs_sequence(data, i);
                }
                // Other ESC sequences (key presses: ESC O A = up arrow, etc.)
                // Keep them all
                _ => {
                    result.push(data[i]);
                    i += 1;
                }
            }
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    result
}

enum StdinCsiAction {
    Response,        // Terminal response — filter out
    FocusGained,     // Focus event (CSI I) — filter out, signal focus gained
    FocusLost,       // Focus event (CSI O) — filter out, signal focus lost
    FocusGainedKeep, // Focus event — pass through to PTY AND signal focus gained (shell wants it too)
    FocusLostKeep,   // Focus event — pass through to PTY AND signal focus lost (shell wants it too)
    Keep,            // User keystroke — keep
}

/// Classify a CSI sequence in stdin data.
fn classify_stdin_csi(
    data: &[u8],
    start: usize,
    shell_focus_enabled: bool,
) -> (StdinCsiAction, usize) {
    let mut i = start;
    let len = data.len();

    let param_start = i;
    while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
        i += 1;
    }
    let params = &data[param_start..i];

    let intermediate_start = i;
    while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
        i += 1;
    }
    let has_intermediate = i > intermediate_start;

    if i >= len {
        return (StdinCsiAction::Keep, i);
    }

    let final_byte = data[i];
    i += 1;

    match final_byte {
        // DA (Device Attributes) response: ends with 'c'.
        // ALL CSI sequences ending with 'c' in stdin are DA responses.
        // User keypresses NEVER generate DA sequences.
        b'c' => (StdinCsiAction::Response, i),
        // XTVERSION/DECRQM query without intermediate bytes — not a user keystroke.
        // With intermediate bytes (DECSCUSR cursor style) — keep (user keypress won't
        // generate this, but keep for safety).
        b'q' => {
            if has_intermediate {
                (StdinCsiAction::Keep, i)
            } else {
                (StdinCsiAction::Response, i)
            }
        }
        // CPR: ESC [ <row> ; <col> R
        b'R' => {
            if is_cpr_response(params) {
                (StdinCsiAction::Response, i)
            } else {
                (StdinCsiAction::Keep, i)
            }
        }
        // DSR response: ESC [ <n> n (0=OK, 3=malfunction)
        // But also DSR query (ESC[5n, ESC[6n) — keep all to be safe
        // Focus events: ESC[I (focus gained), ESC[O (focus lost)
        // Only match if no params and no intermediate bytes.
        // When shell_focus_enabled: pass through to PTY AND capture for ourselves
        // When not: capture for ourselves only (filter from PTY input)
        b'I' => {
            if params.is_empty() && !has_intermediate {
                if shell_focus_enabled {
                    (StdinCsiAction::FocusGainedKeep, i)
                } else {
                    (StdinCsiAction::FocusGained, i)
                }
            } else {
                (StdinCsiAction::Keep, i)
            }
        }
        b'O' => {
            if params.is_empty() && !has_intermediate {
                if shell_focus_enabled {
                    (StdinCsiAction::FocusLostKeep, i)
                } else {
                    (StdinCsiAction::FocusLost, i)
                }
            } else {
                (StdinCsiAction::Keep, i)
            }
        }
        _ => (StdinCsiAction::Keep, i),
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    #[test]
    fn test_stdin_filter_focus_gained_default() {
        // Default: shell_focus_enabled = false, focus events are consumed
        let mut filter = StdinResponseFilter::new();
        let input = b"abc\x1b[Idef";
        let output = filter.filter(input);
        assert_eq!(output, b"abcdef");
        assert_eq!(filter.take_focus_event(), Some(true));
    }

    #[test]
    fn test_stdin_filter_focus_lost_default() {
        let mut filter = StdinResponseFilter::new();
        let input = b"abc\x1b[Odef";
        let output = filter.filter(input);
        assert_eq!(output, b"abcdef");
        assert_eq!(filter.take_focus_event(), Some(false));
    }

    #[test]
    fn test_stdin_filter_focus_with_shell_enabled() {
        // shell_focus_enabled = true: focus events pass through AND are captured
        let mut filter = StdinResponseFilter::new();
        filter.set_shell_focus_enabled(true);
        let input = b"abc\x1b[Idef";
        let output = filter.filter(input);
        // Focus event should be kept in output (passed to PTY)
        assert_eq!(output, b"abc\x1b[Idef");
        // AND also captured for our processing
        assert_eq!(filter.take_focus_event(), Some(true));
    }

    #[test]
    fn test_stdin_filter_focus_lost_with_shell_enabled() {
        let mut filter = StdinResponseFilter::new();
        filter.set_shell_focus_enabled(true);
        let input = b"abc\x1b[Odef";
        let output = filter.filter(input);
        assert_eq!(output, b"abc\x1b[Odef");
        assert_eq!(filter.take_focus_event(), Some(false));
    }

    #[test]
    fn test_stdin_filter_focus_not_confused_with_csi_params() {
        // CSI 1 I should NOT be interpreted as focus event (has params)
        let mut filter = StdinResponseFilter::new();
        let input = b"\x1b[1I";
        let output = filter.filter(input);
        // Should keep the sequence (it's a key press, not focus event)
        assert_eq!(output, b"\x1b[1I");
        assert_eq!(filter.take_focus_event(), None);
    }

    #[test]
    fn test_unified_filter_detects_focus_enable() {
        // \x1b[?1004h should set has_focus_enable = true
        let result = unified_filter(b"\x1b[?1004h", 0, 0);
        assert!(result.has_focus_enable);
        // The sequence should be kept in display_data (passed through to terminal)
        assert!(result.display_data.contains(&b'h'));
    }

    #[test]
    fn test_unified_filter_strips_focus_disable() {
        // \x1b[?1004l should be filtered (not in display_data)
        let result = unified_filter(b"\x1b[?1004l", 0, 0);
        assert!(!result.has_focus_enable);
        // The disable sequence should be filtered out
        assert!(!result.display_data.windows(6).any(|w| w == b"\x1b[?1004l"));
        assert!(result.was_filtered);
    }

    #[test]
    fn test_unified_filter_no_focus_enable_by_default() {
        let result = unified_filter(b"hello world", 0, 0);
        assert!(!result.has_focus_enable);
    }
}
