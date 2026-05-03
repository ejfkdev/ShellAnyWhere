use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "saw-client",
    version,
    about = "ShellAnyWhere — remote client\n\nhttps://github.com/ejfkdev/ShellAnyWhere",
    after_help = "EXAMPLES:\n  \
        saw-client --server my.server:18708 --token abc123   Connect to remote server\n  \
        saw-client -i session-id --token abc123              Attach to specific session\n  \
        saw-client --list --token abc123                     List available sessions\n  \
        saw-client --observe --token abc123                  Connect in read-only mode\n  \
        saw-client ssh-key --token abc123                    Derive SSH key from token\n\n\
        Running without a subcommand defaults to 'connect'.\n\
        e.g. saw-client --token 123  is equivalent to  saw-client connect --token 123"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Connect to a remote session (default when no subcommand is given)
    Connect {
        /// Server address (e.g. my.server:18708)
        #[arg(long)]
        server: Option<String>,

        /// Authentication token
        #[arg(long)]
        token: Option<String>,

        /// Session ID to attach to (skip interactive selection)
        #[arg(short = 'i', long)]
        session: Option<String>,

        /// Observe mode (read-only, no input, no resize)
        #[arg(short, long)]
        observe: bool,

        /// List available sessions instead of attaching
        #[arg(short, long)]
        list: bool,

        /// Number of fast reconnect attempts before switching to slow interval
        #[arg(long)]
        reconnect_fast_attempts: Option<usize>,

        /// Fast reconnect min interval in seconds
        #[arg(long)]
        reconnect_fast_min_secs: Option<u64>,

        /// Fast reconnect max interval in seconds
        #[arg(long)]
        reconnect_fast_max_secs: Option<u64>,

        /// Slow reconnect min interval in seconds
        #[arg(long)]
        reconnect_slow_min_secs: Option<u64>,

        /// Slow reconnect max interval in seconds
        #[arg(long)]
        reconnect_slow_max_secs: Option<u64>,
    },

    /// Derive an SSH private key from the token and save it locally
    SshKey {
        /// Server address (used to generate SSH command hint)
        #[arg(long)]
        server: Option<String>,

        /// Authentication token (must match server's token)
        #[arg(long)]
        token: Option<String>,

        /// Output file path for the private key (default: ~/.ssh/shell_anywhere)
        #[arg(long)]
        output: Option<String>,
    },
}

/// Legacy flat args parsed when no subcommand is given (backwards compat).
#[derive(Parser)]
#[command(
    name = "saw-client",
    version,
    about = "ShellAnyWhere — remote client\n\nhttps://github.com/ejfkdev/ShellAnyWhere"
)]
pub struct LegacyCli {
    /// Server address (e.g. my.server:18708)
    #[arg(long)]
    pub server: Option<String>,

    /// Authentication token
    #[arg(long)]
    pub token: Option<String>,

    /// Session ID to attach to (skip interactive selection)
    #[arg(short = 'i', long)]
    pub session: Option<String>,

    /// Observe mode (read-only, no input, no resize)
    #[arg(short, long)]
    pub observe: bool,

    /// List available sessions instead of attaching
    #[arg(short, long)]
    pub list: bool,

    /// Number of fast reconnect attempts before switching to slow interval
    #[arg(long)]
    pub reconnect_fast_attempts: Option<usize>,

    /// Fast reconnect min interval in seconds
    #[arg(long)]
    pub reconnect_fast_min_secs: Option<u64>,

    /// Fast reconnect max interval in seconds
    #[arg(long)]
    pub reconnect_fast_max_secs: Option<u64>,

    /// Slow reconnect min interval in seconds
    #[arg(long)]
    pub reconnect_slow_min_secs: Option<u64>,

    /// Slow reconnect max interval in seconds
    #[arg(long)]
    pub reconnect_slow_max_secs: Option<u64>,
}

impl LegacyCli {
    /// Parse CLI args, handling backwards compatibility:
    /// If no subcommand is given, parse as flat connect args.
    pub fn parse_or_connect() -> Cli {
        let args: Vec<String> = std::env::args().collect();
        // Check if any subcommand or help/version flag is present
        let has_subcommand = args
            .iter()
            .skip(1)
            .any(|a| a == "connect" || a == "ssh-key");
        let needs_full_help = args
            .iter()
            .skip(1)
            .any(|a| a == "-h" || a == "--help" || a == "-V" || a == "--version");
        if has_subcommand || needs_full_help {
            Cli::parse()
        } else {
            // Parse as legacy flat args, then convert to Connect subcommand
            let legacy = LegacyCli::parse();
            Cli {
                command: Some(Commands::Connect {
                    server: legacy.server,
                    token: legacy.token,
                    session: legacy.session,
                    observe: legacy.observe,
                    list: legacy.list,
                    reconnect_fast_attempts: legacy.reconnect_fast_attempts,
                    reconnect_fast_min_secs: legacy.reconnect_fast_min_secs,
                    reconnect_fast_max_secs: legacy.reconnect_fast_max_secs,
                    reconnect_slow_min_secs: legacy.reconnect_slow_min_secs,
                    reconnect_slow_max_secs: legacy.reconnect_slow_max_secs,
                }),
            }
        }
    }
}
