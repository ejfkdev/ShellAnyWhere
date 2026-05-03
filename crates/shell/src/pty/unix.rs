use anyhow::Result;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use tokio::sync::mpsc;

struct MasterFd(RawFd);

impl MasterFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for MasterFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

unsafe impl Send for MasterFd {}
unsafe impl Sync for MasterFd {}

fn close_random_fds() {
    if let Ok(dir) = std::fs::read_dir("/dev/fd") {
        let mut fds = vec![];
        for entry in dir {
            if let Some(num) = entry
                .ok()
                .map(|e| e.file_name())
                .and_then(|s| s.into_string().ok())
                .and_then(|n| n.parse::<libc::c_int>().ok())
                && num > 2
            {
                fds.push(num);
            }
        }
        for fd in fds {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        anyhow::bail!("fcntl F_GETFD failed: {}", std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        anyhow::bail!(
            "fcntl F_SETFD (CLOEXEC) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

pub struct PtyProcess {
    #[allow(dead_code)]
    shell: String,
    #[allow(dead_code)]
    cols: u16,
    #[allow(dead_code)]
    rows: u16,
    master_fd: Arc<MasterFd>,
    slave_fd: Arc<MasterFd>,
    child: std::process::Child,
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl PtyProcess {
    pub fn spawn(shell: &str, cols: u16, rows: u16) -> Result<(Self, PtyReader, PtyWriter)> {
        let mut master_fd: RawFd = -1;
        let mut slave_fd: RawFd = -1;

        let winsize = libc::winsize {
            ws_row: cols,
            ws_col: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &winsize as *const _ as *mut _,
            )
        };

        if result != 0 {
            anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
        }

        // Disable ECHO on the PTY slave before spawning the shell.
        {
            let mut termios: libc::termios =
                unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
            if unsafe { libc::tcgetattr(slave_fd, &mut termios) } == 0 {
                termios.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
                if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &termios) } != 0 {
                    log::warn!(
                        "tcsetattr on PTY slave failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }

        set_cloexec(master_fd)?;

        let cwd = std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));

        let stdin_fd = unsafe { libc::dup(slave_fd) };
        let stdout_fd = unsafe { libc::dup(slave_fd) };
        let stderr_fd = unsafe { libc::dup(slave_fd) };
        if stdin_fd < 0 || stdout_fd < 0 || stderr_fd < 0 {
            unsafe {
                if stdin_fd >= 0 {
                    libc::close(stdin_fd);
                }
                if stdout_fd >= 0 {
                    libc::close(stdout_fd);
                }
                if stderr_fd >= 0 {
                    libc::close(stderr_fd);
                }
                libc::close(slave_fd);
                libc::close(master_fd);
            }
            anyhow::bail!(
                "dup for slave stdio failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let shell_kind = super::ShellKind::from_path(shell);

        let mut cmd = std::process::Command::new(shell);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
        )
        .env("SAW_SKIP", "1")
        .current_dir(&cwd);

        if super::is_default_login_shell() {
            match shell_kind {
                super::ShellKind::Bash => {
                    cmd.arg("--login");
                }
                super::ShellKind::Zsh => {
                    cmd.arg("-l");
                }
                super::ShellKind::Tcsh | super::ShellKind::Ksh => {
                    cmd.arg("-l");
                }
                super::ShellKind::Fish => {
                    cmd.arg("--login");
                }
                _ => {}
            }
        }

        if saw_core::util::guard::is_parent_same_exe() {
            match shell_kind {
                super::ShellKind::Bash => {
                    cmd.args(["--norc", "--noprofile"]);
                }
                super::ShellKind::Zsh | super::ShellKind::Tcsh | super::ShellKind::Ksh => {
                    cmd.arg("-f");
                }
                super::ShellKind::Fish => {
                    cmd.arg("--no-config");
                }
                super::ShellKind::PowerShell => {
                    cmd.arg("-NoProfile");
                }
                super::ShellKind::Nushell => {
                    cmd.arg("--no-config-file");
                }
                super::ShellKind::Elvish => {
                    cmd.arg("-norc");
                }
                super::ShellKind::Xonsh => {
                    cmd.arg("--no-rc");
                }
                super::ShellKind::Cmd | super::ShellKind::Unknown => {}
            }
        }

        match shell_kind {
            super::ShellKind::Bash => {
                cmd.env(
                    "PROMPT_COMMAND",
                    r#"printf '\033]7;file://%s%s\007' "$HOSTNAME" "$PWD""#,
                );
            }
            super::ShellKind::Cmd => {
                cmd.env("PROMPT", "$e]9;9;%CD%$e\\$p$g");
            }
            super::ShellKind::Zsh
            | super::ShellKind::Tcsh
            | super::ShellKind::Ksh
            | super::ShellKind::PowerShell
            | super::ShellKind::Nushell
            | super::ShellKind::Elvish
            | super::ShellKind::Xonsh => {}
            super::ShellKind::Fish => {}
            super::ShellKind::Unknown => {
                cmd.env(
                    "PROMPT_COMMAND",
                    r#"printf '\033]7;file://%s%s\007' "$HOSTNAME" "$PWD""#,
                );
            }
        }

        cmd.stdin(unsafe { std::process::Stdio::from_raw_fd(stdin_fd) })
            .stdout(unsafe { std::process::Stdio::from_raw_fd(stdout_fd) })
            .stderr(unsafe { std::process::Stdio::from_raw_fd(stderr_fd) });

        set_cloexec(slave_fd)?;

        unsafe {
            cmd.pre_exec(move || {
                for signo in &[
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(*signo, libc::SIG_DFL);
                }
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                close_random_fds();
                Ok(())
            });
        }

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                unsafe {
                    libc::close(master_fd);
                    libc::close(slave_fd);
                }
                return Err(e.into());
            }
        };

        let read_fd = unsafe { libc::dup(master_fd) };
        if read_fd < 0 {
            anyhow::bail!(
                "dup for PTY reader failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let write_fd = unsafe { libc::dup(master_fd) };
        if write_fd < 0 {
            unsafe {
                libc::close(read_fd);
            }
            anyhow::bail!(
                "dup for PTY writer failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let reader = PtyReader::new(Box::new(unsafe { std::fs::File::from_raw_fd(read_fd) }))?;
        let writer = PtyWriter::new(
            Box::new(unsafe { std::fs::File::from_raw_fd(write_fd) }),
            Some(slave_fd),
        );

        let process = Self {
            shell: shell.to_string(),
            cols,
            rows,
            master_fd: Arc::new(MasterFd(master_fd)),
            slave_fd: Arc::new(MasterFd(slave_fd)),
            child,
        };

        Ok((process, reader, writer))
    }

    pub fn ensure_echo_off(&self) {
        let slave_fd = self.slave_fd.as_raw_fd();
        let mut termios: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
        if unsafe { libc::tcgetattr(slave_fd, &mut termios) } != 0 {
            return;
        }
        if termios.c_lflag & libc::ECHO != 0 {
            termios.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
            if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &termios) } == 0 {
                log::debug!("Re-disabled ECHO on PTY slave (shell had re-enabled it)");
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.master_fd.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize) } != 0
        {
            anyhow::bail!(
                "ioctl(TIOCSWINSZ) failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    pub fn child_pid(&self) -> u32 {
        self.child.id()
    }
}

// ---------------------------------------------------------------------------
// PtyReader
// ---------------------------------------------------------------------------

pub struct PtyReader {
    rx: mpsc::Receiver<Vec<u8>>,
}

impl PtyReader {
    fn new(mut reader: Box<dyn Read + Send>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self { rx })
    }

    pub async fn read_output(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// PtyWriter
// ---------------------------------------------------------------------------

pub struct PtyWriter {
    writer: std::sync::Mutex<Box<dyn Write + Send>>,
    slave_fd: Option<RawFd>,
}

impl PtyWriter {
    fn new(writer: Box<dyn Write + Send>, slave_fd: Option<RawFd>) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
            slave_fd,
        }
    }

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    pub fn write_response(&self, data: &[u8]) -> Result<()> {
        self.disable_echo();
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    pub fn write_responses(&self, responses: &[Vec<u8>]) -> Result<()> {
        if responses.is_empty() {
            return Ok(());
        }
        self.disable_echo();
        let mut writer = self.writer.lock().unwrap();
        for resp in responses {
            writer.write_all(resp)?;
        }
        writer.flush()?;
        Ok(())
    }

    fn disable_echo(&self) -> Option<libc::termios> {
        let slave_fd = self.slave_fd?;
        let mut termios: libc::termios = unsafe { std::mem::MaybeUninit::zeroed().assume_init() };
        if unsafe { libc::tcgetattr(slave_fd, &mut termios) } != 0 {
            return None;
        }
        if termios.c_lflag & libc::ECHO == 0 {
            return None;
        }
        let saved = termios;
        termios.c_lflag &= !(libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
        if unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &termios) } != 0 {
            return None;
        }
        Some(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_shell() {
        let shell = super::super::detect_shell();
        assert!(!shell.is_empty());
        assert!(std::path::Path::new(&shell).exists());
    }

    #[test]
    fn test_pty_spawn() {
        let shell = super::super::detect_shell();
        let result = PtyProcess::spawn(&shell, 80, 24);
        assert!(
            result.is_ok(),
            "Failed to spawn PTY with shell {}: {:?}",
            shell,
            result.err()
        );
    }

    #[tokio::test]
    async fn test_pty_read_write() {
        let shell = super::super::detect_shell();
        let (_process, mut reader, writer) = PtyProcess::spawn(&shell, 80, 24).unwrap();
        writer.write_input(b"echo hello_world_test\n").unwrap();
        let mut output = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                reader.read_output(),
            )
            .await
            {
                Ok(Some(data)) => {
                    output.extend_from_slice(&data);
                    if String::from_utf8_lossy(&output).contains("hello_world_test") {
                        return;
                    }
                }
                _ => continue,
            }
        }
        assert!(!output.is_empty(), "PTY should produce some output");
    }
}
