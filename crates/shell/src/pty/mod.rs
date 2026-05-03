#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

// ---------------------------------------------------------------------------
// ShellKind — classify a shell path for CWD injection strategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Tcsh,
    Ksh,
    PowerShell,
    Cmd,
    Nushell,
    Elvish,
    Xonsh,
    Unknown,
}

impl ShellKind {
    pub fn from_path(shell: &str) -> Self {
        let name = shell
            .rsplit('/')
            .next()
            .unwrap_or(shell)
            .rsplit('\\')
            .next()
            .unwrap_or(shell)
            .to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        match name {
            "bash" => ShellKind::Bash,
            "zsh" => ShellKind::Zsh,
            "fish" => ShellKind::Fish,
            "tcsh" | "csh" => ShellKind::Tcsh,
            "ksh" | "ksh93" | "mksh" => ShellKind::Ksh,
            "pwsh" | "powershell" | "powershell_ise" => ShellKind::PowerShell,
            "cmd" => ShellKind::Cmd,
            "nu" => ShellKind::Nushell,
            "elvish" | "elv" => ShellKind::Elvish,
            "xonsh" => ShellKind::Xonsh,
            _ => ShellKind::Unknown,
        }
    }

    pub fn native_osc7(self) -> bool {
        matches!(self, ShellKind::Fish)
    }

    pub fn needs_pty_inject(self) -> bool {
        matches!(
            self,
            ShellKind::Zsh
                | ShellKind::Tcsh
                | ShellKind::Ksh
                | ShellKind::PowerShell
                | ShellKind::Nushell
                | ShellKind::Elvish
                | ShellKind::Xonsh
        )
    }

    pub fn pty_inject_cmd(self) -> Option<&'static [u8]> {
        match self {
            ShellKind::Zsh => Some(b"\x03__osc7_cwd() { printf '\\033]7;file://%s%s\\007' \"$HOSTNAME\" \"$PWD\"; }; if [[ -z ${precmd_functions[(r)__osc7_cwd]} ]]; then precmd_functions+=(__osc7_cwd); fi\n\x03"),
            ShellKind::Tcsh => Some(b"alias precmd 'printf \"\\033]7;file://%s%s\\007\" \"$HOST\" \"$cwd\"'\n"),
            ShellKind::PowerShell => Some(b"function global:prompt { $esc=[char]27; $h=[System.Net.Dns]::GetHostName(); $p=$pwd.Path.Replace('\\','/'); Write-Host \"${esc}]7;file://${h}${p}${esc}\\\" -NoNewline; \"PS $($pwd.Path)> \" }\n"),
            ShellKind::Nushell => Some(b"$env.config = ($env.config | upsert hooks.pre_prompt [{ code: 'print -n $\"(ansi esc)]7;file://(hostname)($env.PWD)(char bel)\"' }])\n"),
            ShellKind::Xonsh => Some(b"@events.on_pre_prompt\ndef _osc7_cwd():\n import os; h=os.uname().nodename if hasattr(os,'uname') else os.environ.get('HOSTNAME','localhost'); print(f'\\033]7;file://{h}{os.getcwd()}\\007',end='',flush=True)\n\n"),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// detect_shell
// ---------------------------------------------------------------------------

pub fn detect_shell() -> String {
    #[cfg(unix)]
    {
        if let Ok(shell) = std::env::var("SHELL")
            && !shell.is_empty()
            && std::path::Path::new(&shell).exists()
            && !saw_core::util::guard::is_saw_executable(&shell)
        {
            return shell;
        }
        let candidates = [
            "/bin/zsh",
            "/bin/bash",
            "/usr/bin/zsh",
            "/usr/bin/bash",
            "/usr/local/bin/fish",
        ];
        for path in &candidates {
            if std::path::Path::new(path).exists() {
                return path.to_string();
            }
        }
        "/bin/sh".to_string()
    }
    #[cfg(not(unix))]
    {
        if let Ok(ps) = which("powershell") {
            return ps;
        }
        if let Ok(pwsh) = which("pwsh") {
            return pwsh;
        }
        "cmd.exe".to_string()
    }
}

#[cfg(unix)]
fn is_default_login_shell() -> bool {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some_and(|s| saw_core::util::guard::is_saw_executable(&s))
}

#[cfg(not(unix))]
fn which(name: &str) -> Result<String, ()> {
    if let Ok(path_var) = std::env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for dir in path_var.split(separator) {
            let candidate = std::path::Path::new(dir).join(format!("{}.exe", name));
            if candidate.exists() {
                return Ok(candidate.to_string_lossy().to_string());
            }
            let candidate_no_ext = std::path::Path::new(dir).join(name);
            if candidate_no_ext.exists() {
                return Ok(candidate_no_ext.to_string_lossy().to_string());
            }
        }
    }
    Err(())
}

// ---------------------------------------------------------------------------
// get_terminal_size
// ---------------------------------------------------------------------------

pub fn get_terminal_size() -> (u16, u16) {
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        return (cols, rows);
    }
    (80, 24)
}

// ---------------------------------------------------------------------------
// read_process_cwd
// ---------------------------------------------------------------------------

pub fn read_process_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{}/cwd", pid);
        std::fs::read_link(&path)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        read_process_cwd_macos(pid)
    }
    #[cfg(target_os = "windows")]
    {
        read_process_cwd_windows(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
fn read_process_cwd_macos(pid: u32) -> Option<String> {
    use std::ffi::CStr;
    const PROC_PIDVNODEPATHINFO: i32 = 9;
    const VIP_PATH_OFFSET: usize = 152;
    const BUF_SIZE: usize = 2352;
    let mut buf = [0u8; BUF_SIZE];
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as i32,
        )
    };
    if ret <= 0 {
        return None;
    }
    if VIP_PATH_OFFSET >= buf.len() {
        return None;
    }
    let path_buf = &buf[VIP_PATH_OFFSET..];
    let cstr = CStr::from_bytes_until_nul(path_buf).ok()?;
    let path = cstr.to_str().ok()?;
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

#[cfg(target_os = "windows")]
fn read_process_cwd_windows(_pid: u32) -> Option<String> {
    None
}
