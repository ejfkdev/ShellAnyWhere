mod cli;

use saw_core::config;
use saw_core::crypto::auth;
use saw_core::util::logging;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use saw_shell::kcp_connector::KcpAgentConnector;
use saw_shell::{pty, session::Session, setup};

#[tokio::main]
async fn main() {
    let cli = match cli::parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}\nTry 'saw-shell --help' for usage.");
            std::process::exit(1);
        }
    };

    match &cli {
        cli::Cli::Help => {
            cli::print_help();
            return;
        }
        cli::Cli::Version => {
            cli::print_version();
            return;
        }
        _ => {}
    }

    logging::init_file_logging("agent", false);

    let file_config = config::load_config_file().unwrap_or_default();

    if let Err(e) = run(cli, &file_config).await {
        log::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

async fn run(cli: cli::Cli, fc: &config::AppConfig) -> anyhow::Result<()> {
    auth::set_token_file_path(fc.paths.resolve_token_file());

    match cli {
        cli::Cli::Agent(a) => {
            if std::env::var("SAW_SKIP").is_ok() {
                anyhow::bail!("SAW_SKIP is set, refusing to start agent recursively");
            }
            if saw_core::util::guard::is_parent_same_exe() {
                anyhow::bail!(
                    "Already running inside a remote session, refusing to start agent recursively"
                );
            }

            let rc = config::ResolvedAgentConfig::resolve(
                a.server,
                a.token,
                a.shell,
                a.flush_interval,
                a.no_ssh_key_forward,
                a.io_compress,
                a.io_diff,
                fc,
            );

            let shell = rc.shell.unwrap_or_else(pty::detect_shell);
            let (cols, rows) = pty::get_terminal_size();

            log::info!("Starting agent with shell={} size={}x{}", shell, cols, rows);

            let session = Session::new(&shell, cols, rows)?;
            let server_token = auth::get_or_create_token(rc.token)?;

            let connector = KcpAgentConnector::new(rc.server, server_token)
                .with_reconnect_params(
                    rc.reconnect_fast_attempts,
                    rc.reconnect_fast_min,
                    rc.reconnect_fast_max,
                    rc.reconnect_slow_min,
                    rc.reconnect_slow_max,
                )
                .with_connect_timeout(rc.connect_timeout)
                .with_paths(rc.paths)
                .with_session_update_interval(rc.session_update_interval)
                .with_ssh_key_forward(rc.ssh_key_forward)
                .with_flush_interval(rc.flush_interval)
                .with_io_compress(rc.io_compress)
                .with_io_diff(rc.io_diff);
            connector.run(session).await?;
        }

        cli::Cli::Install(s) => {
            setup::inject_config(s.shell, s.server, s.token)?;
        }
        cli::Cli::Uninstall(u) => {
            setup::remove_config(u.shell)?;
        }
        cli::Cli::Help | cli::Cli::Version => unreachable!(),
    }

    Ok(())
}
