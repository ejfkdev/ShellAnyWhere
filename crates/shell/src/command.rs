//! Simple command tracker for agent sessions.
//!
//! Buffers stdin data and extracts command lines when Enter is pressed.
//! Tracks first_command and updates last_activity_at.
//! Properly filters terminal escape sequences (CSI, OSC, etc.).

/// Parser state for escape sequence handling.
#[derive(Default)]
enum EscState {
    #[default]
    Ground,
    Esc,    // After ESC
    Csi,    // ESC [ — collecting parameter/intermediate bytes
    Osc,    // ESC ] — until BEL or ST
    OscEsc, // ESC ] ... ESC — potential ST (ESC \)
}

/// Tracks commands entered in a PTY session by monitoring stdin data.
pub struct CommandTracker {
    line_buf: Vec<u8>,
    esc_state: EscState,
}

impl Default for CommandTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandTracker {
    pub fn new() -> Self {
        Self {
            line_buf: Vec::new(),
            esc_state: EscState::Ground,
        }
    }

    /// Feed stdin data to the tracker. Returns Some(command) when a line is
    /// completed (Enter pressed), None otherwise.
    pub fn feed(&mut self, data: &[u8]) -> Option<String> {
        let mut completed_command = None;

        for &byte in data {
            // Handle escape sequence states first
            match self.esc_state {
                EscState::Esc => {
                    match byte {
                        b'[' => {
                            self.esc_state = EscState::Csi;
                        }
                        b']' => {
                            self.esc_state = EscState::Osc;
                        }
                        _ => {
                            // Two-byte ESC sequence done (e.g. ESC O, ESC (, etc.)
                            self.esc_state = EscState::Ground;
                        }
                    }
                    continue;
                }
                EscState::Csi => {
                    match byte {
                        0x40..=0x7e => {
                            // Final byte of CSI sequence
                            self.esc_state = EscState::Ground;
                        }
                        0x20..=0x3f => {
                            // Parameter/intermediate bytes — continue
                        }
                        _ => {
                            // Unexpected byte, abort sequence
                            self.esc_state = EscState::Ground;
                        }
                    }
                    continue;
                }
                EscState::Osc => {
                    match byte {
                        0x07 => {
                            // BEL terminates OSC
                            self.esc_state = EscState::Ground;
                        }
                        0x1b => {
                            self.esc_state = EscState::OscEsc;
                        }
                        _ => {
                            // Continue OSC string
                        }
                    }
                    continue;
                }
                EscState::OscEsc => {
                    // After ESC within OSC — check for ST (ESC \)
                    match byte {
                        b'\\' => {
                            self.esc_state = EscState::Ground;
                        }
                        b'[' => {
                            self.esc_state = EscState::Csi;
                        }
                        _ => {
                            self.esc_state = EscState::Ground;
                        }
                    }
                    continue;
                }
                EscState::Ground => {}
            }

            // Ground state — normal character processing
            match byte {
                b'\r' | b'\n' => {
                    // Enter pressed — extract command from buffer
                    let cmd = self.extract_command();
                    if !cmd.is_empty() {
                        completed_command = Some(cmd);
                    }
                    self.line_buf.clear();
                }
                0x7f | 0x08 => {
                    // Backspace / Delete — remove last char
                    self.line_buf.pop();
                }
                0x03 | 0x04 | 0x1a => {
                    // Ctrl+C, Ctrl+D, Ctrl+Z — cancel line
                    self.line_buf.clear();
                }
                0x01..=0x1a if byte != b'\t' => {
                    // Other control characters (except Tab) — ignore
                }
                0x1b => {
                    // ESC — start of escape sequence
                    self.esc_state = EscState::Esc;
                }
                b' '..=b'~' => {
                    // Printable ASCII
                    self.line_buf.push(byte);
                }
                0x80..=0xff => {
                    // UTF-8 continuation bytes — include for multi-byte chars
                    self.line_buf.push(byte);
                }
                _ => {}
            }
        }

        completed_command
    }

    /// Extract a cleaned command string from the line buffer.
    /// Strips any remaining non-printable characters as a safety net.
    fn extract_command(&self) -> String {
        let s = String::from_utf8_lossy(&self.line_buf);
        // Remove any leftover non-printable characters
        let cleaned: String = s
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        let trimmed = cleaned.trim();
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_command() {
        let mut tracker = CommandTracker::new();
        let cmd = tracker.feed(b"ls -la\r");
        assert_eq!(cmd, Some("ls -la".to_string()));
    }

    #[test]
    fn test_backspace() {
        let mut tracker = CommandTracker::new();
        let cmd = tracker.feed(b"lss\x7f -la\r");
        assert_eq!(cmd, Some("ls -la".to_string()));
    }

    #[test]
    fn test_empty_line() {
        let mut tracker = CommandTracker::new();
        let cmd = tracker.feed(b"\r");
        assert_eq!(cmd, None);
    }

    #[test]
    fn test_ctrl_c_cancels() {
        let mut tracker = CommandTracker::new();
        tracker.feed(b"some typing");
        let cmd = tracker.feed(&[0x03]);
        assert_eq!(cmd, None);
        // After Ctrl+C, line is cleared
        let cmd2 = tracker.feed(b"ls\r");
        assert_eq!(cmd2, Some("ls".to_string()));
    }

    #[test]
    fn test_multiple_commands() {
        let mut tracker = CommandTracker::new();
        let cmd1 = tracker.feed(b"cd /tmp\r");
        assert_eq!(cmd1, Some("cd /tmp".to_string()));
        let cmd2 = tracker.feed(b"ls\r");
        assert_eq!(cmd2, Some("ls".to_string()));
    }

    #[test]
    fn test_whitespace_trimmed() {
        let mut tracker = CommandTracker::new();
        let cmd = tracker.feed(b"  ls -la  \r");
        assert_eq!(cmd, Some("ls -la".to_string()));
    }

    #[test]
    fn test_incremental_input() {
        let mut tracker = CommandTracker::new();
        assert_eq!(tracker.feed(b"l"), None);
        assert_eq!(tracker.feed(b"s"), None);
        let cmd = tracker.feed(b"\r");
        assert_eq!(cmd, Some("ls".to_string()));
    }
}
