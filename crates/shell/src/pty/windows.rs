use anyhow::Result;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct PtyProcess {
    inner: Arc<std::sync::Mutex<conpty::Process>>,
    #[allow(dead_code)]
    shell: String,
    #[allow(dead_code)]
    cols: u16,
    #[allow(dead_code)]
    rows: u16,
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_alive() {
            let _ = inner.exit(1);
        }
    }
}

impl PtyProcess {
    pub fn spawn(shell: &str, cols: u16, rows: u16) -> Result<(Self, PtyReader, PtyWriter)> {
        let shell_kind = super::ShellKind::from_path(shell);

        let mut cmd = std::process::Command::new(shell);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
        )
        .env("SAW_SKIP", "1");

        let cwd = std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("\\"));
        cmd.current_dir(&cwd);

        if saw_core::util::guard::is_parent_same_exe() {
            match shell_kind {
                super::ShellKind::PowerShell => {
                    cmd.arg("-NoProfile");
                }
                super::ShellKind::Bash => {
                    cmd.args(["--norc", "--noprofile"]);
                }
                super::ShellKind::Cmd => {}
                _ => {}
            }
        }

        match shell_kind {
            super::ShellKind::Cmd => {
                cmd.env("PROMPT", "$e]9;9;%CD%$e\\$p$g");
            }
            super::ShellKind::Bash => {
                cmd.env(
                    "PROMPT_COMMAND",
                    r#"printf '\033]7;file://%s%s\007' "$HOSTNAME" "$PWD""#,
                );
            }
            _ => {}
        }

        let mut opts = conpty::ProcessOptions::default();
        opts.set_console_size(Some((cols as i16, rows as i16)));

        let mut proc = opts.spawn(cmd)?;
        let input = proc.input()?;
        let output = proc.output()?;

        let inner = Arc::new(std::sync::Mutex::new(proc));

        let reader = PtyReader::new(Box::new(output))?;
        let writer = PtyWriter {
            writer: std::sync::Mutex::new(Box::new(input)),
        };

        let process = Self {
            inner,
            shell: shell.to_string(),
            cols,
            rows,
        };

        Ok((process, reader, writer))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.resize(cols as i16, rows as i16)?;
        Ok(())
    }

    pub fn ensure_echo_off(&self) {
        let mut inner = self.inner.lock().unwrap();
        let _ = inner.set_echo(false);
    }

    pub fn child_pid(&self) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner.pid()
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
}

impl PtyWriter {
    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    pub fn write_response(&self, data: &[u8]) -> Result<()> {
        // ConPTY handles echo differently — no explicit disable needed
        self.write_input(data)
    }

    pub fn write_responses(&self, responses: &[Vec<u8>]) -> Result<()> {
        if responses.is_empty() {
            return Ok(());
        }
        let mut writer = self.writer.lock().unwrap();
        for resp in responses {
            writer.write_all(resp)?;
        }
        writer.flush()?;
        Ok(())
    }
}
