//! Configuration loading and merging.
//!
//! Three-tier priority: CLI args > environment variables > config file.
//!
//! Paths:
//!   Config & Data: Linux/macOS ~/.config/ShellAnyWhere/
//!                  Windows %APPDATA%\ShellAnyWhere\
//!
//! clap handles CLI + env merging natively (via `env = "SAW_XXX"` attributes).
//! This module loads the config file and provides merge helpers that treat
//! config-file values as the lowest-priority fallback.

use serde::Deserialize;
use std::path::PathBuf;

// ── Default value functions for serde ──────────────────────────────────

fn default_listen() -> String {
    "0.0.0.0:18708".to_string()
}
fn default_ssh_idle_timeout() -> u64 {
    3600
}
fn default_reconnect_fast_attempts() -> usize {
    100
}
fn default_reconnect_fast_min_secs() -> u64 {
    1
}
fn default_reconnect_fast_max_secs() -> u64 {
    2
}
fn default_reconnect_slow_min_secs() -> u64 {
    60
}
fn default_reconnect_slow_max_secs() -> u64 {
    120
}
fn default_session_update_interval() -> u64 {
    5
}
fn default_peek_timeout() -> u64 {
    5
}
fn default_connect_timeout_secs() -> u64 {
    5
}
fn default_keep_alive_interval_secs() -> u64 {
    1
}
fn default_idle_timeout_secs() -> u64 {
    5
}

// ── Config file structures ─────────────────────────────────────────────

/// Top-level config file structure.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerSection,
    pub agent: AgentSection,
    pub client: ClientSection,
    pub paths: PathsSection,
    pub protocol: ProtocolSection,
}

#[derive(Debug, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub ssh_authorized_keys: String,
    #[serde(default = "default_ssh_idle_timeout")]
    pub ssh_idle_timeout_secs: u64,
    #[serde(default = "default_true")]
    pub ssh_enabled: bool,
    #[serde(default)]
    pub ssh_password_auth: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            token: String::new(),
            ssh_authorized_keys: String::new(),
            ssh_idle_timeout_secs: default_ssh_idle_timeout(),
            ssh_enabled: true,
            ssh_password_auth: false,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentSection {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub auto_server: bool,
    #[serde(default = "default_reconnect_fast_attempts")]
    pub reconnect_fast_attempts: usize,
    #[serde(default = "default_reconnect_fast_min_secs")]
    pub reconnect_fast_min_secs: u64,
    #[serde(default = "default_reconnect_fast_max_secs")]
    pub reconnect_fast_max_secs: u64,
    #[serde(default = "default_reconnect_slow_min_secs")]
    pub reconnect_slow_min_secs: u64,
    #[serde(default = "default_reconnect_slow_max_secs")]
    pub reconnect_slow_max_secs: u64,
    /// Enable terminal focus event tracking. When the agent's terminal tab/window
    /// gains focus, the session's last activity time is updated. Default: true.
    #[serde(default = "default_true")]
    pub focus_tracking: bool,
    /// Forward local SSH public keys to server for SSH access. Default: true.
    #[serde(default = "default_true")]
    pub ssh_key_forward: bool,
    /// TerminalIO output flush interval in milliseconds. Default: 100.
    #[serde(default = "default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    /// Compress TerminalIO output data. Default: false.
    #[serde(default)]
    pub io_compress: bool,
    /// Use diff optimization for fullscreen programs. Default: false.
    #[serde(default)]
    pub io_diff: bool,
}

fn default_true() -> bool {
    true
}
fn default_flush_interval_ms() -> u64 {
    100
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            server: String::new(),
            token: String::new(),
            shell: String::new(),
            auto_server: false,
            reconnect_fast_attempts: default_reconnect_fast_attempts(),
            reconnect_fast_min_secs: default_reconnect_fast_min_secs(),
            reconnect_fast_max_secs: default_reconnect_fast_max_secs(),
            reconnect_slow_min_secs: default_reconnect_slow_min_secs(),
            reconnect_slow_max_secs: default_reconnect_slow_max_secs(),
            focus_tracking: default_true(),
            ssh_key_forward: default_true(),
            flush_interval_ms: default_flush_interval_ms(),
            io_compress: false,
            io_diff: false,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ClientSection {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub token: String,
    /// Enable terminal focus event tracking. When the client's terminal gains
    /// focus, it is marked as the active client. Default: true.
    #[serde(default = "default_true")]
    pub focus_tracking: bool,
    #[serde(default = "default_reconnect_fast_attempts")]
    pub reconnect_fast_attempts: usize,
    #[serde(default = "default_reconnect_fast_min_secs")]
    pub reconnect_fast_min_secs: u64,
    #[serde(default = "default_reconnect_fast_max_secs")]
    pub reconnect_fast_max_secs: u64,
    #[serde(default = "default_reconnect_slow_min_secs")]
    pub reconnect_slow_min_secs: u64,
    #[serde(default = "default_reconnect_slow_max_secs")]
    pub reconnect_slow_max_secs: u64,
}

impl Default for ClientSection {
    fn default() -> Self {
        Self {
            server: String::new(),
            token: String::new(),
            focus_tracking: default_true(),
            reconnect_fast_attempts: default_reconnect_fast_attempts(),
            reconnect_fast_min_secs: default_reconnect_fast_min_secs(),
            reconnect_fast_max_secs: default_reconnect_fast_max_secs(),
            reconnect_slow_min_secs: default_reconnect_slow_min_secs(),
            reconnect_slow_max_secs: default_reconnect_slow_max_secs(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct PathsSection {
    /// Data directory. Empty = config directory
    #[serde(default)]
    pub data_dir: String,
    /// Server log file. Empty = `{data_dir}/server.log`
    #[serde(default)]
    pub log_file: String,
    /// Token file. Empty = config dir/token
    #[serde(default)]
    pub token_file: String,
    /// SSH host key file. Empty = config dir/ssh_host_key
    #[serde(default)]
    pub ssh_host_key: String,
    /// TLS certificate file. Empty = config dir/server.crt
    #[serde(default)]
    pub cert_file: String,
    /// TLS private key file. Empty = config dir/server.key
    #[serde(default)]
    pub key_file: String,
}

/// Home directory based paths, consistent across macOS, Linux, and Windows.
impl PathsSection {
    /// Default config directory:
    ///   Linux/macOS: `~/.config/ShellAnyWhere/`
    ///   Windows: `%APPDATA%\ShellAnyWhere\`
    pub fn default_config_dir() -> PathBuf {
        #[cfg(not(target_os = "windows"))]
        {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("ShellAnyWhere")
        }
        #[cfg(target_os = "windows")]
        {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ShellAnyWhere")
        }
    }

    /// Default log directory:
    ///   Linux: `~/.local/state/ShellAnyWhere/`
    ///   macOS: `~/.config/ShellAnyWhere/logs/`
    fn default_log_dir() -> PathBuf {
        #[cfg(not(target_os = "windows"))]
        {
            Self::default_config_dir().join("logs")
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ShellAnyWhere")
        }
    }

    /// Compute the data directory. Defaults to the same directory as config.
    pub fn resolve_data_dir(&self) -> PathBuf {
        if self.data_dir.is_empty() {
            Self::default_config_dir()
        } else {
            PathBuf::from(&self.data_dir)
        }
    }

    pub fn resolve_log_file(&self) -> PathBuf {
        if self.log_file.is_empty() {
            Self::default_log_dir().join("server.log")
        } else {
            PathBuf::from(&self.log_file)
        }
    }

    pub fn resolve_token_file(&self) -> PathBuf {
        if self.token_file.is_empty() {
            Self::default_config_dir().join("token")
        } else {
            PathBuf::from(&self.token_file)
        }
    }

    pub fn resolve_ssh_host_key(&self) -> PathBuf {
        if self.ssh_host_key.is_empty() {
            Self::default_config_dir().join("ssh_host_key")
        } else {
            PathBuf::from(&self.ssh_host_key)
        }
    }

    pub fn resolve_cert_file(&self) -> PathBuf {
        if self.cert_file.is_empty() {
            Self::default_config_dir().join("server.crt")
        } else {
            PathBuf::from(&self.cert_file)
        }
    }

    pub fn resolve_key_file(&self) -> PathBuf {
        if self.key_file.is_empty() {
            Self::default_config_dir().join("server.key")
        } else {
            PathBuf::from(&self.key_file)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProtocolSection {
    #[serde(default = "default_session_update_interval")]
    pub session_update_interval_secs: u64,
    #[serde(default = "default_peek_timeout")]
    pub peek_timeout_secs: u64,
    /// Timeout for connect/auth/attach operations (agent, client, upstream).
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Keep-alive interval in seconds.
    /// Also used as TLS Ping interval for yamux connections.
    #[serde(default = "default_keep_alive_interval_secs")]
    pub keep_alive_interval_secs: u64,
    /// Idle timeout in seconds. Connection closed if no packets
    /// received for this duration. For TLS, max consecutive missed Pongs.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// Public IP for WebRTC ICE host candidate. Set when server is behind NAT
    /// or listening on 0.0.0.0. Empty = auto-detect from local address.
    #[serde(default)]
    pub webrtc_public_ip: String,
}

impl Default for ProtocolSection {
    fn default() -> Self {
        Self {
            session_update_interval_secs: default_session_update_interval(),
            peek_timeout_secs: default_peek_timeout(),
            connect_timeout_secs: default_connect_timeout_secs(),
            keep_alive_interval_secs: default_keep_alive_interval_secs(),
            idle_timeout_secs: default_idle_timeout_secs(),
            webrtc_public_ip: String::new(),
        }
    }
}

// ── Resolved paths ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ResolvedPaths {
    pub data_dir: PathBuf,
    pub log_file: PathBuf,
    pub token_file: PathBuf,
    pub ssh_host_key: PathBuf,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

impl Default for ResolvedPaths {
    fn default() -> Self {
        let paths = PathsSection::default();
        Self {
            data_dir: paths.resolve_data_dir(),
            log_file: paths.resolve_log_file(),
            token_file: paths.resolve_token_file(),
            ssh_host_key: paths.resolve_ssh_host_key(),
            cert_file: paths.resolve_cert_file(),
            key_file: paths.resolve_key_file(),
        }
    }
}

// ── Environment variable helpers ─────────────────────────────────────

/// Read environment variable with `SAW_` prefix.
/// `env("SERVER")` reads `SAW_SERVER`. Returns `None` if unset or empty.
pub fn env(name: &str) -> Option<String> {
    std::env::var(format!("SAW_{name}"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// Check if environment variable with `SAW_` prefix is set (flag-style).
/// `env_set("IO_COMPRESS")` checks `SAW_IO_COMPRESS`.
pub fn env_set(name: &str) -> bool {
    std::env::var(format!("SAW_{name}")).is_ok()
}

/// Read boolean environment variable with `SAW_` prefix.
/// `env_bool("SSH_ENABLED")` reads `SAW_SSH_ENABLED`.
/// "1"/"true" → Some(true), "0"/"false" → Some(false), unset/invalid → None.
pub fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(format!("SAW_{name}")) {
        Ok(v) if v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true") => Some(true),
        Ok(v) if v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

// ── Resolved configurations ─────────────────────────────────────────

/// Resolved server configuration.
/// Priority: CLI > SAW_ env vars > config file > defaults.
#[derive(Debug)]
pub struct ResolvedServerConfig {
    pub listen: String,
    pub token: Option<String>,
    pub ssh_authorized_keys: Option<String>,
    pub ssh_idle_timeout_secs: u64,
    pub ssh_enabled: bool,
    pub ssh_password_auth: bool,
    pub peek_timeout_secs: u64,
    pub data_dir: PathBuf,
    pub keep_alive_interval: std::time::Duration,
    pub idle_timeout: std::time::Duration,
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub webrtc_public_ip: String,
}

impl ResolvedServerConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        cli_listen: Option<String>,
        cli_token: Option<String>,
        cli_ssh_authorized_keys: Option<String>,
        cli_ssh_idle_timeout_secs: Option<u64>,
        cli_ssh_enabled: Option<bool>,
        cli_ssh_password_auth: Option<bool>,
        cli_data_dir: Option<String>,
        cli_cert_file: Option<String>,
        cli_key_file: Option<String>,
        cli_webrtc_public_ip: Option<String>,
        fc: &AppConfig,
    ) -> Self {
        let listen = cli_listen
            .or_else(|| env("LISTEN"))
            .or_else(|| {
                if fc.server.listen.is_empty() {
                    None
                } else {
                    Some(fc.server.listen.clone())
                }
            })
            .unwrap_or_else(default_listen);

        let token = cli_token.or_else(|| env("TOKEN")).or_else(|| {
            if fc.server.token.is_empty() {
                None
            } else {
                Some(fc.server.token.clone())
            }
        });

        let ssh_authorized_keys = cli_ssh_authorized_keys
            .or_else(|| env("SSH_AUTHORIZED_KEYS"))
            .or_else(|| {
                if fc.server.ssh_authorized_keys.is_empty() {
                    None
                } else {
                    Some(fc.server.ssh_authorized_keys.clone())
                }
            });

        let ssh_idle_timeout_secs = cli_ssh_idle_timeout_secs
            .or_else(|| env("SSH_IDLE_TIMEOUT").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.server.ssh_idle_timeout_secs);

        let ssh_enabled = cli_ssh_enabled
            .or_else(|| env_bool("SSH_ENABLED"))
            .unwrap_or(fc.server.ssh_enabled);

        let ssh_password_auth = cli_ssh_password_auth
            .or_else(|| env_bool("SSH_PASSWORD_AUTH"))
            .unwrap_or(fc.server.ssh_password_auth);

        let data_dir = cli_data_dir
            .or_else(|| env("DATA_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| fc.paths.resolve_data_dir());

        let cert_file = cli_cert_file
            .or_else(|| env("CERT_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| fc.paths.resolve_cert_file());

        let key_file = cli_key_file
            .or_else(|| env("KEY_FILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| fc.paths.resolve_key_file());

        let webrtc_public_ip = cli_webrtc_public_ip
            .or_else(|| env("WEBRTC_PUBLIC_IP"))
            .or_else(|| {
                if fc.protocol.webrtc_public_ip.is_empty() {
                    None
                } else {
                    Some(fc.protocol.webrtc_public_ip.clone())
                }
            })
            .unwrap_or_default();

        Self {
            listen,
            token,
            ssh_authorized_keys,
            ssh_idle_timeout_secs,
            ssh_enabled,
            ssh_password_auth,
            peek_timeout_secs: fc.protocol.peek_timeout_secs,
            data_dir,
            keep_alive_interval: std::time::Duration::from_secs(
                fc.protocol.keep_alive_interval_secs,
            ),
            idle_timeout: std::time::Duration::from_secs(fc.protocol.idle_timeout_secs),
            cert_file,
            key_file,
            webrtc_public_ip,
        }
    }
}

/// Resolved agent/shell configuration.
/// Priority: CLI > SAW_ env vars > config file > defaults.
#[derive(Debug)]
pub struct ResolvedAgentConfig {
    pub server: String,
    pub token: Option<String>,
    /// None = auto-detect via pty::detect_shell()
    pub shell: Option<String>,
    pub auto_server: bool,
    pub reconnect_fast_attempts: usize,
    pub reconnect_fast_min: std::time::Duration,
    pub reconnect_fast_max: std::time::Duration,
    pub reconnect_slow_min: std::time::Duration,
    pub reconnect_slow_max: std::time::Duration,
    pub connect_timeout: std::time::Duration,
    pub keep_alive_interval: std::time::Duration,
    pub idle_timeout: std::time::Duration,
    pub paths: ResolvedPaths,
    pub session_update_interval: std::time::Duration,
    pub focus_tracking: bool,
    pub ssh_key_forward: bool,
    pub flush_interval: std::time::Duration,
    pub io_compress: bool,
    pub io_diff: bool,
}

impl ResolvedAgentConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        cli_server: Option<String>,
        cli_token: Option<String>,
        cli_shell: Option<String>,
        cli_flush_interval: u64,
        cli_no_ssh_key_forward: bool,
        cli_io_compress: bool,
        cli_io_diff: bool,
        fc: &AppConfig,
    ) -> Self {
        let server = cli_server
            .or_else(|| env("SERVER"))
            .or_else(|| {
                if fc.agent.server.is_empty() {
                    None
                } else {
                    Some(fc.agent.server.clone())
                }
            })
            .unwrap_or_else(|| "127.0.0.1:18708".to_string());

        let token = cli_token.or_else(|| env("TOKEN")).or_else(|| {
            if fc.agent.token.is_empty() {
                None
            } else {
                Some(fc.agent.token.clone())
            }
        });

        let shell = cli_shell.or_else(|| env("SHELL_PATH")).or_else(|| {
            if fc.agent.shell.is_empty() {
                None
            } else {
                Some(fc.agent.shell.clone())
            }
        });

        let flush_interval_ms = if cli_flush_interval != 0 {
            cli_flush_interval
        } else {
            fc.agent.flush_interval_ms
        };

        // --no-ssh-key-forward / SAW_NO_SSH_KEY_FORWARD take precedence
        let ssh_key_forward = if cli_no_ssh_key_forward || env_set("NO_SSH_KEY_FORWARD") {
            false
        } else {
            fc.agent.ssh_key_forward
        };

        // Enable flags: any source saying true wins
        let io_compress = cli_io_compress || env_set("IO_COMPRESS") || fc.agent.io_compress;
        let io_diff = cli_io_diff || env_set("IO_DIFF") || fc.agent.io_diff;

        Self {
            server,
            token,
            shell,
            auto_server: fc.agent.auto_server,
            reconnect_fast_attempts: fc.agent.reconnect_fast_attempts,
            reconnect_fast_min: std::time::Duration::from_secs(fc.agent.reconnect_fast_min_secs),
            reconnect_fast_max: std::time::Duration::from_secs(fc.agent.reconnect_fast_max_secs),
            reconnect_slow_min: std::time::Duration::from_secs(fc.agent.reconnect_slow_min_secs),
            reconnect_slow_max: std::time::Duration::from_secs(fc.agent.reconnect_slow_max_secs),
            connect_timeout: std::time::Duration::from_secs(fc.protocol.connect_timeout_secs),
            keep_alive_interval: std::time::Duration::from_secs(
                fc.protocol.keep_alive_interval_secs,
            ),
            idle_timeout: std::time::Duration::from_secs(fc.protocol.idle_timeout_secs),
            paths: ResolvedPaths {
                data_dir: fc.paths.resolve_data_dir(),
                log_file: fc.paths.resolve_log_file(),
                token_file: fc.paths.resolve_token_file(),
                ssh_host_key: fc.paths.resolve_ssh_host_key(),
                cert_file: fc.paths.resolve_cert_file(),
                key_file: fc.paths.resolve_key_file(),
            },
            session_update_interval: std::time::Duration::from_secs(
                fc.protocol.session_update_interval_secs,
            ),
            focus_tracking: env_bool("FOCUS_TRACKING").unwrap_or(fc.agent.focus_tracking),
            ssh_key_forward,
            flush_interval: std::time::Duration::from_millis(flush_interval_ms),
            io_compress,
            io_diff,
        }
    }
}

/// Resolved client configuration.
/// Priority: CLI > SAW_ env vars > config file > defaults.
#[derive(Debug)]
pub struct ResolvedClientConfig {
    pub server: String,
    pub token: Option<String>,
    pub focus_tracking: bool,
    pub connect_timeout: std::time::Duration,
    pub keep_alive_interval: std::time::Duration,
    pub idle_timeout: std::time::Duration,
    pub reconnect_fast_attempts: usize,
    pub reconnect_fast_min: std::time::Duration,
    pub reconnect_fast_max: std::time::Duration,
    pub reconnect_slow_min: std::time::Duration,
    pub reconnect_slow_max: std::time::Duration,
}

impl ResolvedClientConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        cli_server: Option<String>,
        cli_token: Option<String>,
        cli_reconnect_fast_attempts: Option<usize>,
        cli_reconnect_fast_min_secs: Option<u64>,
        cli_reconnect_fast_max_secs: Option<u64>,
        cli_reconnect_slow_min_secs: Option<u64>,
        cli_reconnect_slow_max_secs: Option<u64>,
        fc: &AppConfig,
    ) -> Self {
        let server = cli_server
            .or_else(|| env("SERVER"))
            .or_else(|| {
                if fc.client.server.is_empty() {
                    None
                } else {
                    Some(fc.client.server.clone())
                }
            })
            .unwrap_or_else(|| "127.0.0.1:18708".to_string());

        let token = cli_token.or_else(|| env("TOKEN")).or_else(|| {
            if fc.client.token.is_empty() {
                None
            } else {
                Some(fc.client.token.clone())
            }
        });

        let reconnect_fast_attempts = cli_reconnect_fast_attempts
            .or_else(|| env("RECONNECT_FAST_ATTEMPTS").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.client.reconnect_fast_attempts);

        let reconnect_fast_min_secs = cli_reconnect_fast_min_secs
            .or_else(|| env("RECONNECT_FAST_MIN_SECS").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.client.reconnect_fast_min_secs);

        let reconnect_fast_max_secs = cli_reconnect_fast_max_secs
            .or_else(|| env("RECONNECT_FAST_MAX_SECS").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.client.reconnect_fast_max_secs);

        let reconnect_slow_min_secs = cli_reconnect_slow_min_secs
            .or_else(|| env("RECONNECT_SLOW_MIN_SECS").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.client.reconnect_slow_min_secs);

        let reconnect_slow_max_secs = cli_reconnect_slow_max_secs
            .or_else(|| env("RECONNECT_SLOW_MAX_SECS").and_then(|s| s.parse().ok()))
            .unwrap_or(fc.client.reconnect_slow_max_secs);

        Self {
            server,
            token,
            focus_tracking: env_bool("FOCUS_TRACKING").unwrap_or(fc.client.focus_tracking),
            connect_timeout: std::time::Duration::from_secs(fc.protocol.connect_timeout_secs),
            keep_alive_interval: std::time::Duration::from_secs(
                fc.protocol.keep_alive_interval_secs,
            ),
            idle_timeout: std::time::Duration::from_secs(fc.protocol.idle_timeout_secs),
            reconnect_fast_attempts,
            reconnect_fast_min: std::time::Duration::from_secs(reconnect_fast_min_secs),
            reconnect_fast_max: std::time::Duration::from_secs(reconnect_fast_max_secs),
            reconnect_slow_min: std::time::Duration::from_secs(reconnect_slow_min_secs),
            reconnect_slow_max: std::time::Duration::from_secs(reconnect_slow_max_secs),
        }
    }
}

// ── Loading ────────────────────────────────────────────────────────────

/// Returns the config file path.
pub fn config_file_path() -> PathBuf {
    PathsSection::default_config_dir().join("config.toml")
}

/// Ensure the config directory and default config file exist.
/// Creates the directory tree and writes a default config.toml if missing.
fn ensure_config_dir_and_defaults() {
    let config_dir = PathsSection::default_config_dir();

    // Create config directory if it doesn't exist
    if !config_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            log::warn!("Cannot create config dir {:?}: {}", config_dir, e);
            return;
        }
        log::info!("Created config directory {:?}", config_dir);
    }

    // Write default config.toml if it doesn't exist
    let config_path = config_dir.join("config.toml");
    if !config_path.exists() {
        let default_content = default_config_toml();
        if let Err(e) = std::fs::write(&config_path, default_content) {
            log::warn!("Cannot write default config {:?}: {}", config_path, e);
        } else {
            log::info!("Created default config file {:?}", config_path);
        }
    }
}

/// Default config.toml content, embedded from the project's config.toml at compile time.
const DEFAULT_CONFIG_TOML: &str = include_str!("../../../config.toml");

/// Generate the default config.toml content with platform-specific path comments.
fn default_config_toml() -> String {
    let config_dir = PathsSection::default_config_dir();
    let config_dir_str = config_dir.to_string_lossy();

    format!(
        "# ShellAnyWhere Configuration\n\
         # Priority: CLI args > environment variables > config file\n\
         #\n\
         # Paths:\n\
         #   Config & Data: {config_dir_str}\n\
         #\n\
         {DEFAULT_CONFIG_TOML}"
    )
}

/// Load config from file. Returns None if the file doesn't exist or is invalid.
/// Invalid files produce a warning log and are ignored.
/// On first run, creates the config directory and default config file.
pub fn load_config_file() -> Option<AppConfig> {
    ensure_config_dir_and_defaults();

    let path = config_file_path();
    load_config_file_from(&path)
}

/// Load config from a specific file path. Returns None if the file doesn't exist or is invalid.
pub fn load_config_file_from(path: &std::path::Path) -> Option<AppConfig> {
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cannot read config file {:?}: {}", path, e);
            return None;
        }
    };
    match toml::from_str(&content) {
        Ok(config) => {
            log::info!("Loaded config from {:?}", path);
            Some(config)
        }
        Err(e) => {
            log::warn!("Invalid config file {:?}: {}. Using defaults.", path, e);
            None
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.server.listen, "0.0.0.0:18708");
        assert_eq!(config.server.ssh_idle_timeout_secs, 3600);
        assert_eq!(config.agent.reconnect_fast_min_secs, 1);
        assert_eq!(config.agent.reconnect_slow_max_secs, 120);
        assert_eq!(config.protocol.session_update_interval_secs, 5);
        assert_eq!(config.protocol.peek_timeout_secs, 5);
    }

    #[test]
    fn test_paths_defaults() {
        let paths = PathsSection::default();
        let data_dir = paths.resolve_data_dir();
        assert!(data_dir.to_string_lossy().ends_with("ShellAnyWhere"));
        assert!(
            paths
                .resolve_token_file()
                .to_string_lossy()
                .ends_with("token")
        );
        assert!(
            paths
                .resolve_ssh_host_key()
                .to_string_lossy()
                .ends_with("ssh_host_key")
        );
        assert!(
            paths
                .resolve_log_file()
                .to_string_lossy()
                .ends_with("server.log")
        );
    }

    #[test]
    fn test_paths_custom() {
        let paths = PathsSection {
            data_dir: "/tmp/sr-test".into(),
            ..Default::default()
        };
        assert_eq!(paths.resolve_data_dir(), PathBuf::from("/tmp/sr-test"));
    }

    #[test]
    fn test_toml_parse() {
        let toml = r#"
[server]
listen = "0.0.0.0:9999"
ssh_idle_timeout_secs = 7200

[agent]
auto_server = true

[paths]
data_dir = "/opt/shell-remote"

[protocol]
peek_timeout_secs = 10
"#;
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.listen, "0.0.0.0:9999");
        assert_eq!(config.server.ssh_idle_timeout_secs, 7200);
        assert!(config.agent.auto_server);
        assert_eq!(config.paths.data_dir, "/opt/shell-remote");
        assert_eq!(config.protocol.peek_timeout_secs, 10);
        // Unset fields use defaults
        assert_eq!(config.agent.reconnect_fast_min_secs, 1);
    }

    #[test]
    fn test_toml_empty_sections() {
        let toml = "";
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.listen, "0.0.0.0:18708");
    }

    #[test]
    fn test_toml_partial_section() {
        let toml = "[server]\nlisten = \"1.2.3.4:8080\"";
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.listen, "1.2.3.4:8080");
        assert_eq!(config.server.ssh_idle_timeout_secs, 3600);
    }
}
