//! Logging initialization using tracing-subscriber + tracing-appender.
//!
//! Two modes:
//! - `init_file_logging`: Fixed filename + daily rotation (single-process, e.g. server)
//! - `init_file_logging_with_pid`: PID filename + old log cleanup (multi-process, e.g. client/shell)

#[cfg(feature = "config")]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "config")]
use tracing_subscriber::util::SubscriberInitExt;

/// Maximum number of old log files to keep per program.
const MAX_LOG_FILES: usize = 10;

/// Maximum age (days) for old log files before cleanup.
const MAX_LOG_AGE_DAYS: u64 = 7;

// ── Platform log directory ──────────────────────────────────────────────────

/// Resolve the platform-specific log directory for ShellAnyWhere.
#[cfg(feature = "config")]
fn log_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .map(|h| h.join("Library").join("Logs").join("ShellAnyWhere"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/shell-anywhere-logs"))
    }
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir()
            .map(|d| d.join("ShellAnyWhere").join("logs"))
            .unwrap_or_else(|| std::path::PathBuf::from("shell-anywhere-logs"))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        dirs::home_dir()
            .map(|h| h.join(".local").join("state").join("ShellAnyWhere"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/shell-anywhere-logs"))
    }
}

// ── Single-process logging (server) ─────────────────────────────────────────

/// Initialize file-based logging with daily rotation for single-process programs.
///
/// Log files use a fixed name (e.g., `server.log`) with `RollingFileAppender`
/// rotating daily. Old files are automatically cleaned up (max 7 files).
#[cfg(feature = "config")]
pub fn init_file_logging(name: &str, also_stderr: bool) {
    let dir = log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Warning: cannot create log dir {:?}: {}", dir, e);
    }

    let default_level = "info,yamux=off";
    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());

    if let Err(e) = tracing_log::LogTracer::init() {
        eprintln!("Warning: LogTracer init failed: {}", e);
    }

    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(name)
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .unwrap_or_else(|e| {
            eprintln!("Warning: cannot create rolling log appender: {}", e);
            std::process::exit(1);
        });

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    // Leak the guard so the non-blocking writer flushes on process exit, not on drop.
    std::mem::forget(guard);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&level));

    if also_stderr {
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .init();
    }
}

// ── Multi-process logging (client/shell) ────────────────────────────────────

/// Initialize file-based logging with PID in filename and old log cleanup.
///
/// Each process gets its own log file (e.g., `agent-12345.log`).
/// On startup, old log files are cleaned up (older than 7 days or more than 10 files).
#[cfg(feature = "config")]
pub fn init_file_logging_with_pid(name: &str, also_stderr: bool) {
    let dir = log_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Warning: cannot create log dir {:?}: {}", dir, e);
    }

    cleanup_old_logs(&dir, name, MAX_LOG_AGE_DAYS, MAX_LOG_FILES);

    let pid = std::process::id();
    let log_path = dir.join(format!("{}-{}.log", name, pid));

    let default_level = "info,yamux=off";
    let level = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());

    if let Err(e) = tracing_log::LogTracer::init() {
        eprintln!("Warning: LogTracer init failed: {}", e);
    }

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&level));

    match std::fs::File::create(&log_path) {
        Ok(log_file) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(log_file);
            std::mem::forget(guard);

            if also_stderr {
                let stderr_layer = tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(false);
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false);
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(stderr_layer)
                    .with(file_layer)
                    .init();
            } else {
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false);
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(file_layer)
                    .init();
            }
        }
        Err(e) => {
            if also_stderr {
                let stderr_layer = tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(false);
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(stderr_layer)
                    .init();
            } else {
                eprintln!("Warning: cannot create log file {:?}: {}", log_path, e);
            }
        }
    }
}

// ── Old log cleanup ─────────────────────────────────────────────────────────

/// Clean up old log files for a given program name.
///
/// Deletes files matching `{name}-*.log` that are older than `max_age_days`,
/// and keeps at most `max_files` recent files.
#[cfg(feature = "config")]
fn cleanup_old_logs(dir: &std::path::Path, name: &str, max_age_days: u64, max_files: usize) {
    let prefix = format!("{}-", name);
    let suffix = ".log";

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let now = std::time::SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days * 24 * 3600);

    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !file_name.starts_with(&prefix) || !file_name.ends_with(suffix) {
            continue;
        }
        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Ok(age) = now.duration_since(mtime) {
            if age > max_age {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        files.push((path, mtime));
    }

    // Sort newest first, delete excess
    files.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (path, _) in files.into_iter().skip(max_files) {
        let _ = std::fs::remove_file(&path);
    }
}

// ── No-config fallback (stderr only) ────────────────────────────────────────

#[cfg(not(feature = "config"))]
pub fn init_file_logging(_name: &str, _also_stderr: bool) {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
}

#[cfg(not(feature = "config"))]
pub fn init_file_logging_with_pid(_name: &str, _also_stderr: bool) {
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_old_logs() {
        let dir = tempfile::tempdir().unwrap();
        let name = "test-app";

        // Create some fake log files
        for i in 0..15 {
            let path = dir.path().join(format!("{}-{}.log", name, i));
            std::fs::write(&path, format!("log content {}", i)).unwrap();
        }

        // Create a non-matching file that should not be deleted
        let other_path = dir.path().join("other-app-1.log");
        std::fs::write(&other_path, "other content").unwrap();

        cleanup_old_logs(dir.path(), name, 7, 10);

        // Should keep at most 10 files of the matching name
        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(name))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(remaining.len(), 10);

        // Non-matching file should still exist
        assert!(other_path.exists());
    }
}
