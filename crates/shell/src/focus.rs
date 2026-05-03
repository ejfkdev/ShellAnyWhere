/// On creation, writes `\x1b[?1004h` to stdout to enable focus reporting.
/// On drop, writes `\x1b[?1004l` to disable it.
pub struct FocusGuard {
    enabled: bool,
}

impl FocusGuard {
    pub fn enter(enabled: bool) -> Self {
        if enabled {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\x1b[?1004h");
            let _ = std::io::stdout().flush();
        }
        Self { enabled }
    }
}

impl Drop for FocusGuard {
    fn drop(&mut self) {
        if self.enabled {
            use std::io::Write;
            let _ = std::io::stdout().write_all(b"\x1b[?1004l");
            let _ = std::io::stdout().flush();
        }
    }
}
