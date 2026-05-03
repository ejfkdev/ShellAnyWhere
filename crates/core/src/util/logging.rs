#[cfg(feature = "config")]
use std::io::Write;

/// Initialize file-based logging with optional stderr output.
///
/// Log files are written to platform-specific directories:
/// - macOS: `~/Library/Logs/ShellAnyWhere/{name}-{pid}.log`
/// - Linux: `~/.local/state/ShellAnyWhere/{name}-{pid}.log`
/// - Windows: `%LOCALAPPDATA%/ShellAnyWhere/{name}-{pid}.log`
///
/// Falls back to `/tmp/shell-anywhere-{name}-{pid}.log` on Unix if home dir is unavailable.
#[cfg(feature = "config")]
pub fn init_file_logging(name: &str, also_stderr: bool) {
    let pid = std::process::id();
    let log_path = {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir()
                .map(|h| {
                    h.join("Library")
                        .join("Logs")
                        .join("ShellAnyWhere")
                        .join(format!("{}-{}.log", name, pid))
                })
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("/tmp/shell-anywhere-{}-{}.log", name, pid))
                })
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_local_dir()
                .map(|d| {
                    d.join("ShellAnyWhere")
                        .join(format!("{}-{}.log", name, pid))
                })
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("shell-anywhere-{}-{}.log", name, pid))
                })
        }
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            dirs::home_dir()
                .map(|h| {
                    h.join(".local")
                        .join("state")
                        .join("ShellAnyWhere")
                        .join(format!("{}-{}.log", name, pid))
                })
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(format!("/tmp/shell-anywhere-{}-{}.log", name, pid))
                })
        }
    };

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let default_level = "info,yamux=off";
    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());

    let mut builder = env_logger::Builder::from_default_env();
    builder.parse_filters(&level).format_timestamp_secs();

    match std::fs::File::create(&log_path) {
        Ok(log_file) => {
            if also_stderr {
                builder.target(env_logger::Target::Pipe(Box::new(MultiWriter {
                    file: log_file,
                    fallback: Box::new(std::io::stderr()),
                })));
            } else {
                builder.target(env_logger::Target::Pipe(Box::new(log_file)));
            }
            builder.init();
        }
        Err(e) => {
            if also_stderr {
                builder.target(env_logger::Target::Stderr);
                builder.init();
            } else {
                eprintln!("Warning: cannot create log file {:?}: {}", log_path, e);
            }
        }
    }
}

/// Writes to both a log file and a fallback (stderr or sink).
#[cfg(feature = "config")]
struct MultiWriter {
    file: std::fs::File,
    fallback: Box<dyn Write + Send>,
}

#[cfg(feature = "config")]
impl Write for MultiWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.fallback.write(buf);
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.fallback.flush();
        self.file.flush()
    }
}
