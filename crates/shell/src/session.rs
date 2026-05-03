use crate::pty::{PtyProcess, PtyReader, PtyWriter};
use anyhow::Result;
use saw_core::protocol::control::SessionInfo;
use std::sync::Arc;

/// Generate a cryptographically random ID of given length.
/// Uses rand with lowercase alphanumeric alphabet (no lookalikes: 0/O, 1/l, o).
fn generate_alphanum_id(len: usize) -> String {
    // 0 1 l o removed to avoid visual ambiguity
    const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyz";
    // ALPHABET.len() = 32, next power of 2 = 32, mask = 31
    // For any len <= 32, find next power of 2 and subtract 1
    let next_pow2 = (ALPHABET.len() - 1).next_power_of_two();
    let mask = (next_pow2 - 1) as u32;
    let mut result = String::with_capacity(len);
    while result.len() < len {
        let val = rand::random::<u32>() & mask;
        if (val as usize) < ALPHABET.len() {
            result.push(ALPHABET[val as usize] as char);
        }
    }
    result
}

/// A single PTY session managed by the agent
pub struct Session {
    pub id: String,
    pub shell: String,
    pub reader: PtyReader,
    pub writer: Arc<PtyWriter>,
    pub process: Arc<PtyProcess>,
    pub cols: u16,
    pub rows: u16,
    pub cwd: String,
    pub first_command: Option<String>,
    pub terminal_program: Option<String>,
    pub last_activity_at: u64,
    pub started_at: u64,
    pub hostname: String,
    pub username: String,
    pub title: String,
}

impl Session {
    pub fn new(shell: &str, cols: u16, rows: u16) -> Result<Self> {
        let (process, reader, writer) = PtyProcess::spawn(shell, cols, rows)?;

        let id = format!("sess-{}", generate_alphanum_id(8));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cwd = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .to_string_lossy()
            .to_string();
        let hostname = get_hostname();
        let username = get_username();
        let terminal_program = get_terminal_program();

        Ok(Self {
            id,
            shell: shell.to_string(),
            reader,
            writer: Arc::new(writer),
            process: Arc::new(process),
            cols,
            rows,
            cwd,
            first_command: None,
            terminal_program,
            last_activity_at: now,
            started_at: now,
            hostname,
            username,
            title: String::new(),
        })
    }

    pub fn session_info(&self) -> SessionInfo {
        SessionInfo {
            session_id: self.id.clone(),
            shell: self.shell.clone(),
            started_at: self.started_at,
            cols: self.cols,
            rows: self.rows,
            cwd: self.cwd.clone(),
            first_command: self.first_command.clone(),
            terminal_program: self.terminal_program.clone(),
            last_activity_at: self.last_activity_at,
            hostname: self.hostname.clone(),
            username: self.username.clone(),
            title: self.title.clone(),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.process.resize(cols, rows)?;
        self.cols = cols;
        self.rows = rows;
        log::debug!("Session {} resized to {}x{}", self.id, cols, rows);
        Ok(())
    }
}

/// Manages all sessions for this agent
pub struct SessionManager {
    sessions: Vec<Session>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
        }
    }

    pub fn add_session(&mut self, session: Session) {
        self.sessions.push(session);
    }

    pub fn find_session(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|s| s.session_info()).collect()
    }
}

fn get_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).to_string()
        } else {
            String::new()
        }
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_default()
    }
}

fn get_username() -> String {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        // Try LOGNAME, USER, then fall back to uid
        std::env::var("LOGNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| format!("uid={}", uid))
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").unwrap_or_default()
    }
}

/// Detect the terminal emulator that launched this agent.
///
/// Detection priority:
/// 1. Terminal-specific unique env vars (KONSOLE_DBUS_SESSION, GNOME_TERMINAL_SERVICE, etc.)
/// 2. TERM_PROGRAM (macOS / Ghostty / iTerm2 / VS Code / Hyper / Warp)
/// 3. COLORTERM for VTE-based terminals (xfce4-terminal, Guake, Tilix, etc.)
fn get_terminal_program() -> Option<String> {
    // ── Tier 1: unique env vars (set by exactly one terminal) ──

    // Konsole (KDE)
    if std::env::var("KONSOLE_DBUS_SESSION").is_ok() {
        let version = std::env::var("KONSOLE_VERSION").ok();
        return Some(match version {
            Some(v) => format!("Konsole {}", v),
            None => "Konsole".to_string(),
        });
    }

    // GNOME Terminal
    if std::env::var("GNOME_TERMINAL_SERVICE").is_ok() {
        return Some("GNOME Terminal".to_string());
    }

    // Terminator
    if std::env::var("TERMINATOR_UUID").is_ok() {
        return Some("Terminator".to_string());
    }

    // foot (Wayland)
    if std::env::var("FOOT_WINDOW_ID").is_ok() {
        return Some("foot".to_string());
    }

    // kitty
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return Some("kitty".to_string());
    }

    // Alacritty
    if std::env::var("ALACRITTY_WINDOW_ID").is_ok() {
        return Some("Alacritty".to_string());
    }

    // WezTerm (also sets TERM_PROGRAM=WezTerm, but WEZTERM_PANE is more specific)
    if std::env::var("WEZTERM_PANE").is_ok() {
        return Some("WezTerm".to_string());
    }

    // iTerm2 (also sets TERM_PROGRAM=iTerm.app, but ITERM_SESSION_ID is more specific)
    if std::env::var("ITERM_SESSION_ID").is_ok() {
        let version = std::env::var("TERM_PROGRAM_VERSION").ok();
        return Some(match version {
            Some(v) => format!("iTerm2 {}", v),
            None => "iTerm2".to_string(),
        });
    }

    // Windows Terminal
    if std::env::var("WT_SESSION").is_ok() {
        return Some("Windows Terminal".to_string());
    }

    // Warp
    if std::env::var("WARP_HONOR_PS1").is_ok() {
        return Some("Warp".to_string());
    }

    // ── Tier 2: TERM_PROGRAM (shared variable, different values) ──

    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        let version = std::env::var("TERM_PROGRAM_VERSION").ok();
        let name = match tp.as_str() {
            "ghostty" => "Ghostty",
            "iTerm.app" => "iTerm2",
            "Apple_Terminal" => "Terminal",
            "WezTerm" => "WezTerm",
            "vscode" => "VS Code",
            "Hyper" => "Hyper",
            "WarpTerminal" => "Warp",
            "rio" => "Rio",
            "Tabby" => "Tabby",
            "contour" => "Contour",
            "BlackBox" => "Black Box",
            _ => &tp,
        };
        return Some(match version {
            Some(v) => format!("{} {}", name, v),
            None => name.to_string(),
        });
    }

    // ── Tier 3: COLORTERM (VTE-based and other terminals) ──

    if let Ok(ct) = std::env::var("COLORTERM") {
        let name = match ct.as_str() {
            "xfce4-terminal" => "Xfce4 Terminal",
            "guake" => "Guake",
            "tilix" => "Tilix",
            "mate-terminal" => "MATE Terminal",
            "sakura" => "Sakura",
            "roxterm" => "ROXTerm",
            "lxterminal" => "LXDE Terminal",
            "yakuake" => "Yakuake",
            "termite" => "Termite",
            _ => return None,
        };
        return Some(name.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_type_detect() {
        let detected = std::env::var("SHELL");
        assert!(detected.is_ok());
    }

    #[test]
    fn test_session_new() {
        let shell = crate::pty::detect_shell();
        let session = Session::new(&shell, 80, 24);
        assert!(session.is_ok());
        let session = session.unwrap();
        assert!(session.id.starts_with("sess-"));
        assert_eq!(session.cols, 80);
        assert_eq!(session.rows, 24);
    }
}
