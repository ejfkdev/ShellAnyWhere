//! Control-plane messages for the ShellAnyWhere protocol.
//!
//! Control messages are sent over a `ContextStream` with `stream_type = "control"`.
//! They handle authentication, session management, and coordination.
//! Data transfer (terminal I/O, files, media) uses separate `ContextStream`s.
//!
//! Use `ContextStream::send_control` / `recv_control` to exchange these messages.

use serde::{Deserialize, Serialize};

/// Session metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub shell: String,
    pub started_at: u64,
    pub cols: u16,
    pub rows: u16,
    pub cwd: String,
    pub first_command: Option<String>,
    pub terminal_program: Option<String>,
    pub last_activity_at: u64,
    /// Hostname of the machine running the agent.
    #[serde(default)]
    pub hostname: String,
    /// Username of the user running the agent.
    #[serde(default)]
    pub username: String,
    /// Terminal title set by OSC 0/2 sequences.
    #[serde(default)]
    pub title: String,
}

/// Format a session list menu for terminal display.
/// Returns the full menu string including header, entries, and prompt.
/// `input_buf` is the current user input to echo in the prompt line.
pub fn format_session_menu(sessions: &[SessionInfo], input_buf: &str) -> String {
    let mut out = String::new();
    out.push_str("\r\n═══ ShellAnyWhere Sessions ═══\r\n\r\n");
    if sessions.is_empty() {
        out.push_str("  No sessions available.\r\n");
    } else {
        for (i, s) in sessions.iter().enumerate() {
            out.push_str(&format!(
                "  [{}] {}  {}@{}\r\n",
                i + 1,
                s.session_id,
                s.username,
                s.hostname
            ));
            out.push_str(&format!(
                "       shell={}  {}x{}  cwd={}  term={}\r\n",
                s.shell,
                s.cols,
                s.rows,
                s.cwd,
                s.terminal_program.as_deref().unwrap_or("-")
            ));
        }
    }
    out.push_str(&format!("\r\nEnter session number or ID: {}", input_buf));
    out
}

/// Client attach mode
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum AttachMode {
    Observe,
    Interact,
}

/// Control-plane messages sent over the control stream.
///
/// The control stream is a `ContextStream` with `stream_type = "control"`.
/// All signaling (auth, session management, coordination) uses these messages.
/// Data transfer (terminal I/O, files, media) uses separate `ContextStream`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Control {
    // === Mutual Authentication (challenge-response) ===
    /// Client initiates mutual auth with its nonce
    AuthInit {
        client_nonce: Vec<u8>,
    },
    /// Server responds with its nonce + proof (HMAC-SHA256(token, client_nonce))
    AuthChallenge {
        nonce: Vec<u8>,
        proof: Vec<u8>,
    },
    /// Client responds with proof (HMAC-SHA256(token, server_nonce))
    AuthResponse {
        response: Vec<u8>,
    },
    /// Server returns authentication result
    AuthResult {
        ok: bool,
    },
    Ping,
    Pong,

    // === Session Management ===
    SessionRegister {
        session: SessionInfo,
        /// SSH public keys from agent's local system.
        #[serde(default)]
        ssh_public_keys: Vec<String>,
    },
    SessionUpdate {
        session: SessionInfo,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    SessionAttach {
        session_id: String,
        mode: AttachMode,
        /// If reconnecting, provide the previously assigned client_id.
        previous_client_id: Option<String>,
    },
    /// Server acknowledges client attachment.
    AttachAck {
        session_id: String,
        client_id: String,
        mode: AttachMode,
    },
    /// Server rejects the attach attempt.
    AttachReject {
        session_id: String,
        reason: String,
    },
    SessionDetach {
        session_id: String,
        client_id: String,
    },
    SessionClose {
        session_id: String,
    },

    /// Server notifies agent that a new client has attached.
    ClientAttached {
        session_id: String,
        client_id: String,
        mode: AttachMode,
    },

    /// Server broadcasts which client is currently active.
    ClientActive {
        session_id: String,
        client_id: String,
        cols: u16,
        rows: u16,
    },

    /// Client reports its terminal size, declaring itself as the active client.
    ClientResize {
        session_id: String,
        client_id: String,
        cols: u16,
        rows: u16,
    },

    // === Desktop Notification ===
    /// Desktop notification from the remote shell (OSC 9 or OSC 777).
    DesktopNotification {
        session_id: String,
        title: String,
        body: String,
    },

    /// Client sets a custom title for the session (user rename).
    /// Server stores it in session.title and broadcasts SessionUpdate.
    ClientSetTitle {
        session_id: String,
        client_id: String,
        title: String,
    },

    /// Client requests a full screen repaint from the agent.
    /// Agent responds by setting needs_replay, which triggers serialize_screen()
    /// and writes the result to the TerminalIO stream — same path as ClientAttached.
    ClientRefresh {
        session_id: String,
        client_id: String,
    },
}

impl Control {
    /// Encode this control message to bytes (bincode serialization).
    /// Used by session_mgr for pre-encoding messages to network clients.
    pub fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(bincode::serde::encode_to_vec(
            self,
            bincode::config::standard(),
        )?)
    }

    /// Decode a control message from bytes.
    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        let (ctrl, _): (Control, _) =
            bincode::serde::decode_from_slice(data, bincode::config::standard())?;
        Ok(ctrl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ctrl: Control) {
        let encoded = bincode::serde::encode_to_vec(&ctrl, bincode::config::standard())
            .expect("encode failed");
        let (decoded, _): (Control, _) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                .expect("decode failed");
        let re_encoded = bincode::serde::encode_to_vec(&decoded, bincode::config::standard())
            .expect("re-encode failed");
        assert_eq!(encoded, re_encoded);
    }

    #[test]
    fn test_auth_roundtrip() {
        roundtrip(Control::AuthInit {
            client_nonce: vec![1, 2, 3, 4],
        });
        roundtrip(Control::AuthChallenge {
            nonce: vec![5, 6, 7, 8],
            proof: vec![0xCD; 32],
        });
        roundtrip(Control::AuthResponse {
            response: vec![0xAB; 32],
        });
        roundtrip(Control::AuthResult { ok: true });
        roundtrip(Control::AuthResult { ok: false });
    }

    #[test]
    fn test_ping_pong_roundtrip() {
        roundtrip(Control::Ping);
        roundtrip(Control::Pong);
    }

    #[test]
    fn test_session_roundtrip() {
        let info = SessionInfo {
            session_id: "sid-001".into(),
            shell: "/bin/zsh".into(),
            started_at: 1713400000,
            cols: 120,
            rows: 40,
            cwd: "/home/user".into(),
            first_command: Some("ls".into()),
            terminal_program: Some("Ghostty".into()),
            last_activity_at: 1713400100,
            hostname: "myhost".into(),
            username: "user".into(),
            title: String::new(),
        };
        roundtrip(Control::SessionRegister {
            session: info.clone(),
            ssh_public_keys: Vec::new(),
        });
        roundtrip(Control::SessionList {
            sessions: vec![info],
        });
        roundtrip(Control::SessionAttach {
            session_id: "sid-001".into(),
            mode: AttachMode::Interact,
            previous_client_id: None,
        });
        roundtrip(Control::AttachAck {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
            mode: AttachMode::Interact,
        });
        roundtrip(Control::AttachReject {
            session_id: "sid-001".into(),
            reason: "Session not found".into(),
        });
    }

    #[test]
    fn test_client_attached_roundtrip() {
        roundtrip(Control::ClientAttached {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
            mode: AttachMode::Interact,
        });
    }

    #[test]
    fn test_client_active_roundtrip() {
        roundtrip(Control::ClientActive {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
            cols: 120,
            rows: 40,
        });
    }

    #[test]
    fn test_client_resize_roundtrip() {
        roundtrip(Control::ClientResize {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
            cols: 200,
            rows: 50,
        });
    }

    #[test]
    fn test_desktop_notification_roundtrip() {
        roundtrip(Control::DesktopNotification {
            session_id: "sid-001".into(),
            title: "Build Complete".into(),
            body: "All tests passed".into(),
        });
    }

    #[test]
    fn test_client_set_title_roundtrip() {
        roundtrip(Control::ClientSetTitle {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
            title: "My Session".into(),
        });
    }

    #[test]
    fn test_client_refresh_roundtrip() {
        roundtrip(Control::ClientRefresh {
            session_id: "sid-001".into(),
            client_id: "client-1".into(),
        });
    }
}
