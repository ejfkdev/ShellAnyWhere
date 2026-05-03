use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "saw-server",
    version,
    about = "ShellAnyWhere — relay server\n\nhttps://github.com/ejfkdev/ShellAnyWhere",
    after_help = "EXAMPLES:\n  \
        saw-server                                     Start with defaults (0.0.0.0:18708, SSH on, auto token)\n  \
        saw-server -l 0.0.0.0:9000                     Listen on port 9000\n  \
        saw-server -t my-secret-token                   Set authentication token\n  \
        saw-server --no-ssh                             Disable SSH protocol\n  \
        saw-server --cert-file /path/cert --key-file /path/key  Enable TLS\n  \
        saw-server --webrtc-public-ip 1.2.3.4           Set public IP for WebRTC (NAT traversal)\n  \
        saw-server --data-dir /opt/saw-data             Use custom data directory\n  \
        saw-server --ssh-password-auth                  Enable SSH password authentication\n  \
        saw-server install                              Install as system service\n  \
        saw-server uninstall                            Uninstall system service"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Listen address [default: 0.0.0.0:18708]
    #[arg(short, long)]
    pub listen: Option<String>,

    /// Authentication token (auto-generated if not specified)
    #[arg(short, long)]
    pub token: Option<String>,

    /// Path to authorized_keys for SSH public key auth
    #[arg(long)]
    pub ssh_authorized_keys: Option<String>,

    /// SSH idle timeout in seconds [default: 3600]
    #[arg(long)]
    pub ssh_idle_timeout: Option<u64>,

    /// Enable SSH protocol [default: true]
    #[arg(long, overrides_with("no_ssh"))]
    pub ssh: bool,

    /// Disable SSH protocol
    #[arg(long)]
    pub no_ssh: bool,

    /// Enable SSH password authentication [default: false]
    #[arg(long, overrides_with("no_ssh_password_auth"))]
    pub ssh_password_auth: bool,

    /// Disable SSH password authentication
    #[arg(long)]
    pub no_ssh_password_auth: bool,

    /// Config file path [default: config dir/config.toml]
    #[arg(short, long)]
    pub config: Option<String>,

    /// Data directory [default: config dir]
    #[arg(long)]
    pub data_dir: Option<String>,

    /// TLS certificate file
    #[arg(long)]
    pub cert_file: Option<String>,

    /// TLS private key file
    #[arg(long)]
    pub key_file: Option<String>,

    /// Public IP for WebRTC ICE host candidate (for NAT traversal)
    #[arg(long)]
    pub webrtc_public_ip: Option<String>,

    /// Serve web frontend from a local directory instead of embedded assets (for development)
    #[arg(long)]
    pub web_dir: Option<String>,

    /// [Windows only] Run as Windows Service (internal flag, do not use manually)
    #[arg(long, hide = true)]
    pub run_as_service: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Install saw-server as a system service (auto-start on boot, auto-restart on crash)
    Install,
    /// Uninstall the system service
    Uninstall,
}

impl Cli {
    /// Resolve ssh_enabled from --ssh / --no-ssh flags.
    /// Returns None if neither flag was set (use config/default).
    pub fn ssh_enabled(&self) -> Option<bool> {
        if self.no_ssh {
            Some(false)
        } else if self.ssh {
            Some(true)
        } else {
            None
        }
    }

    /// Resolve ssh_password_auth from --ssh-password-auth / --no-ssh-password-auth flags.
    /// Returns None if neither flag was set (use config/default).
    pub fn ssh_password_auth(&self) -> Option<bool> {
        if self.no_ssh_password_auth {
            Some(false)
        } else if self.ssh_password_auth {
            Some(true)
        } else {
            None
        }
    }
}
