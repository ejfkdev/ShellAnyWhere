mod cli;
mod service;

use clap::Parser;
use saw_core::config;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use saw_core::crypto::auth;
use saw_core::util::logging;
use saw_server::listener;

#[tokio::main]
async fn main() {
    let cli = cli::Cli::parse();

    // Windows Service mode: dispatched by Windows Service Control Manager
    #[cfg(target_os = "windows")]
    if cli.run_as_service {
        logging::init_file_logging("server", true);
        if let Err(e) = service::run_as_service() {
            log::error!("Windows Service error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // In debug builds, resolve the default web-dir from CARGO_MANIFEST_DIR.
    // Canonicalize to an absolute path without ../.. so it works when
    // installed as a system service (working directory differs).
    let debug_web_dir: Option<std::path::PathBuf> = if cfg!(debug_assertions) {
        let web_dist = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
        match web_dist.canonicalize() {
            Ok(p) => {
                log::info!("[debug] Auto web-dir: {}", p.display());
                Some(p)
            }
            Err(_) => None,
        }
    } else {
        None
    };

    // Subcommand dispatch
    match &cli.command {
        Some(cli::Commands::Install) => {
            let extra_args: Vec<String> = if let Some(ref wd) = debug_web_dir {
                println!("  [debug] Using web-dir: {}", wd.display());
                vec!["--web-dir".to_string(), wd.to_string_lossy().into_owned()]
            } else {
                Vec::new()
            };
            if let Err(e) = service::install(&extra_args) {
                eprintln!("Install failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(cli::Commands::Uninstall) => {
            if let Err(e) = service::uninstall() {
                eprintln!("Uninstall failed: {}", e);
                std::process::exit(1);
            }
        }
        None => {
            // Default: run server
            logging::init_file_logging("server", true);

            let file_config = if let Some(ref path) = cli.config {
                config::load_config_file_from(std::path::Path::new(path)).unwrap_or_default()
            } else {
                config::load_config_file().unwrap_or_default()
            };

            if let Err(e) = run(cli, &file_config, debug_web_dir).await {
                log::error!("Fatal error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

async fn run(
    cli: cli::Cli,
    fc: &config::AppConfig,
    debug_web_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    auth::set_token_file_path(fc.paths.resolve_token_file());

    let ssh_enabled = cli.ssh_enabled();
    let ssh_password_auth = cli.ssh_password_auth();
    let rc = config::ResolvedServerConfig::resolve(
        cli.listen,
        cli.token,
        cli.ssh_authorized_keys,
        cli.ssh_idle_timeout,
        ssh_enabled,
        ssh_password_auth,
        cli.data_dir,
        cli.cert_file,
        cli.key_file,
        cli.webrtc_public_ip,
        fc,
    );

    log::info!("Starting server on {}", rc.listen);
    let token = auth::get_or_create_token(rc.token)?;
    let server_config = listener::ServerConfig {
        listen_addr: rc.listen,
        token: Some(token),
        ssh_authorized_keys: rc.ssh_authorized_keys,
        ssh_idle_timeout_secs: rc.ssh_idle_timeout_secs,
        ssh_enabled: rc.ssh_enabled,
        ssh_password_auth: rc.ssh_password_auth,
        peek_timeout_secs: rc.peek_timeout_secs,
        data_dir: rc.data_dir,
        keep_alive_interval: rc.keep_alive_interval,
        idle_timeout: rc.idle_timeout,
        cert_file: rc.cert_file,
        key_file: rc.key_file,
        webrtc_public_ip: rc.webrtc_public_ip,
        web_dir: cli.web_dir.map(std::path::PathBuf::from).or(debug_web_dir),
    };
    listener::run_server(server_config).await
}
