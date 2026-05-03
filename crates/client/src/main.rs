mod cli;

use saw_client::kcp_connector::KcpClientConnector;
use saw_client::terminal;
use saw_core::config;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use saw_core::crypto::auth;
use saw_core::protocol::control::SessionInfo;
use saw_core::util::logging;

#[tokio::main]
async fn main() {
    let cli = cli::LegacyCli::parse_or_connect();

    logging::init_file_logging("client", true);

    let file_config = config::load_config_file().unwrap_or_default();

    if let Err(e) = run(cli, &file_config).await {
        log::error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

async fn run(cli: cli::Cli, fc: &config::AppConfig) -> anyhow::Result<()> {
    match cli.command {
        Some(cli::Commands::SshKey {
            server,
            token,
            output,
        }) => run_ssh_key(server, token, output, fc),
        Some(cli::Commands::Connect {
            server,
            token,
            session,
            observe,
            list,
            reconnect_fast_attempts,
            reconnect_fast_min_secs,
            reconnect_fast_max_secs,
            reconnect_slow_min_secs,
            reconnect_slow_max_secs,
        }) => {
            run_connect(
                server,
                token,
                session,
                observe,
                list,
                reconnect_fast_attempts,
                reconnect_fast_min_secs,
                reconnect_fast_max_secs,
                reconnect_slow_min_secs,
                reconnect_slow_max_secs,
                fc,
            )
            .await
        }
        None => {
            // No subcommand — should not happen with LegacyCli, but handle gracefully
            anyhow::bail!(
                "No subcommand specified. Use 'saw-client connect' or 'saw-client ssh-key'."
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connect(
    server: Option<String>,
    token: Option<String>,
    session: Option<String>,
    observe: bool,
    list: bool,
    reconnect_fast_attempts: Option<usize>,
    reconnect_fast_min_secs: Option<u64>,
    reconnect_fast_max_secs: Option<u64>,
    reconnect_slow_min_secs: Option<u64>,
    reconnect_slow_max_secs: Option<u64>,
    fc: &config::AppConfig,
) -> anyhow::Result<()> {
    if saw_core::util::guard::is_parent_same_exe() {
        anyhow::bail!(
            "Already running inside a remote session, refusing to start client recursively"
        );
    }

    auth::set_token_file_path(fc.paths.resolve_token_file());

    let rc = config::ResolvedClientConfig::resolve(
        server,
        token,
        reconnect_fast_attempts,
        reconnect_fast_min_secs,
        reconnect_fast_max_secs,
        reconnect_slow_min_secs,
        reconnect_slow_max_secs,
        fc,
    );

    log::info!("Client mode - connecting...");
    let token = auth::get_or_create_token(rc.token.clone())?;

    run_kcp_client(session, observe, list, &rc, &token).await
}

async fn run_kcp_client(
    session: Option<String>,
    observe: bool,
    list: bool,
    rc: &config::ResolvedClientConfig,
    token: &str,
) -> anyhow::Result<()> {
    use saw_core::protocol::control::AttachMode;

    let mut connector = KcpClientConnector::connect(&rc.server, token.to_string()).await?;

    // --list: just list sessions and exit
    if list {
        let list_result = connector.list_sessions().await?;
        let sessions = list_result.sessions;
        if sessions.is_empty() {
            println!("No sessions available.");
        } else {
            print_sessions(&sessions);
        }
        return Ok(());
    }

    // Session select → attach → relay loop.
    // On SessionClosed, go back to session list instead of exiting.
    let mut initial_session = session;
    loop {
        let session_id = select_session_kcp(&mut connector, &initial_session).await?;
        initial_session = None; // Only pre-select on first iteration

        println!("Attaching to session {}...", session_id);
        let attach_mode = if observe {
            AttachMode::Observe
        } else {
            AttachMode::Interact
        };
        log::info!("About to call connector.attach()");
        let attached = connector.attach(&session_id, attach_mode, None).await?;
        log::info!("Attach succeeded, client_id={}", attached.client_id);

        let reconnect = terminal::ReconnectParams {
            server: rc.server.clone(),
            token: token.to_string(),
            connect_timeout: rc.connect_timeout,
            fast_attempts: rc.reconnect_fast_attempts,
            fast_min: rc.reconnect_fast_min,
            fast_max: rc.reconnect_fast_max,
            slow_min: rc.reconnect_slow_min,
            slow_max: rc.reconnect_slow_max,
            keep_alive_interval: rc.keep_alive_interval,
            idle_timeout: rc.idle_timeout,
            focus_tracking: rc.focus_tracking,
        };
        let exit = terminal::run_kcp_terminal_relay_with_reconnect(
            attached.control_stream,
            attached.term_stream,
            &session_id,
            observe,
            &attached.client_id,
            reconnect,
        )
        .await?;

        match exit {
            terminal::RelayExit::UserDetach => {
                eprintln!("\r\nConnection to {} closed.", rc.server);
                return Ok(());
            }
            terminal::RelayExit::SessionClosed => {
                eprintln!("\r\nSession closed.");
                // Re-connect and go back to session list
                connector = KcpClientConnector::connect(&rc.server, token.to_string()).await?;
            }
        }
    }
}

/// Result of processing a keyboard event in session selection.
enum KeyAction {
    /// No action, continue loop
    None,
    /// User selected a session
    Selected(String),
    /// User cancelled
    Cancelled,
}

/// Process a keyboard event for session selection.
/// Returns the action to take, or None if still collecting input.
fn handle_key_event(
    key: crossterm::event::KeyEvent,
    input_buf: &mut String,
    sessions: &[SessionInfo],
) -> KeyAction {
    use crossterm::event::KeyCode;
    match key.code {
        KeyCode::Enter => {
            let choice = input_buf.trim().to_string();
            if let Ok(num) = choice.parse::<usize>()
                && num >= 1
                && num <= sessions.len()
            {
                return KeyAction::Selected(sessions[num - 1].session_id.clone());
            }
            if !choice.is_empty() {
                return KeyAction::Selected(choice);
            }
            KeyAction::None
        }
        KeyCode::Backspace => {
            input_buf.pop();
            print!("\x1B[D \x1B[D");
            KeyAction::None
        }
        KeyCode::Char('c')
            if key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL) =>
        {
            KeyAction::Cancelled
        }
        KeyCode::Char(c) => {
            input_buf.push(c);
            print!("{}", c);
            KeyAction::None
        }
        KeyCode::Esc => KeyAction::Cancelled,
        _ => KeyAction::None,
    }
}

/// Draw the session menu to the terminal.
fn draw_session_menu(sessions: &[SessionInfo], input_buf: &str) {
    use saw_core::protocol::control::format_session_menu;
    print!("\x1B[2J\x1B[H");
    print!("{}", format_session_menu(sessions, input_buf));
}

async fn select_session_kcp(
    connector: &mut KcpClientConnector,
    session_id: &Option<String>,
) -> anyhow::Result<String> {
    if let Some(sid) = session_id {
        return Ok(sid.clone());
    }

    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;
    use saw_core::protocol::control::Control;

    let mut last_sessions = connector.list_sessions_stream().await?;
    let mut input_buf = String::new();
    let mut need_redraw = true;

    crossterm::terminal::enable_raw_mode()?;
    let result = async {
        let mut reader = EventStream::new();

        loop {
            if need_redraw {
                draw_session_menu(&last_sessions, &input_buf);
                need_redraw = false;

                if last_sessions.len() == 1 && input_buf.is_empty() {
                    crossterm::terminal::disable_raw_mode()?;
                    return Ok(last_sessions[0].session_id.clone());
                }
            }

            tokio::select! {
                maybe_event = reader.next() => {
                    if let Some(Ok(Event::Key(key))) = maybe_event {
                        match handle_key_event(key, &mut input_buf, &last_sessions) {
                            KeyAction::Selected(sid) => {
                                crossterm::terminal::disable_raw_mode()?;
                                println!();
                                return Ok(sid);
                            }
                            KeyAction::Cancelled => {
                                crossterm::terminal::disable_raw_mode()?;
                                anyhow::bail!("Cancelled");
                            }
                            KeyAction::None => {}
                        }
                    }
                }
                result = connector.recv_push() => {
                    match result {
                        Ok(Control::SessionList { sessions }) => {
                            last_sessions = sessions;
                            need_redraw = true;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // Connection lost, exit session selection
                            crossterm::terminal::disable_raw_mode()?;
                            eprintln!("\r\nConnection to server lost.");
                            anyhow::bail!("Connection lost");
                        }
                    }
                }
            }
        }
    }
    .await;
    if result.is_err() {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
}

fn print_sessions(sessions: &[SessionInfo]) {
    fn format_time(ts: u64) -> String {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ago_secs = now_secs.saturating_sub(ts);

        let days_since_epoch = ts / 86400;
        let time_of_day = ts % 86400;
        let hours = time_of_day / 3600;
        let minutes = (time_of_day % 3600) / 60;

        let (month, day) = epoch_days_to_month_day(days_since_epoch);
        let time_str = format!("{:02}-{:02} {:02}:{:02}", month, day, hours, minutes);

        if ago_secs == 0 && now_secs < ts {
            time_str
        } else if ago_secs < 60 {
            format!("{} (just now)", time_str)
        } else if ago_secs < 3600 {
            format!("{} ({}m ago)", time_str, ago_secs / 60)
        } else if ago_secs < 86400 {
            format!("{} ({}h ago)", time_str, ago_secs / 3600)
        } else {
            format!("{} ({}d ago)", time_str, ago_secs / 86400)
        }
    }

    fn epoch_days_to_month_day(days: u64) -> (u64, u64) {
        let days_in_months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let year = 1970 + (days / 365);
        let mut remaining = days - (year - 1970) * 365 - count_leap_years(1970, year);
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        for (i, &mdays) in days_in_months.iter().enumerate() {
            let adjusted = if i == 1 && leap { mdays + 1 } else { mdays };
            if remaining < adjusted {
                return ((i + 1) as u64, remaining + 1);
            }
            remaining -= adjusted;
        }
        (12, 31)
    }

    fn count_leap_years(from: u64, to: u64) -> u64 {
        (from..to)
            .filter(|&y| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0)
            .count() as u64
    }

    for (i, s) in sessions.iter().enumerate() {
        let host_user = if !s.hostname.is_empty() || !s.username.is_empty() {
            format!("  {}@{}", s.username, s.hostname)
        } else {
            String::new()
        };
        println!("────────────────────────────────────────");
        println!("[{}] {}{}", i + 1, s.session_id, host_user);
        println!("  Shell: {}   Size: {}x{}", s.shell, s.cols, s.rows);
        println!("  CWD:   {}", s.cwd);
        println!(
            "  Started: {}   Last activity: {}",
            format_time(s.started_at),
            format_time(s.last_activity_at),
        );
        if let Some(ref cmd) = s.first_command {
            println!("  First cmd: {}", cmd);
        }
        if let Some(ref term) = s.terminal_program {
            println!("  Terminal:  {}", term);
        }
    }
    if !sessions.is_empty() {
        println!("────────────────────────────────────────");
    }
}

/// Derive SSH private key from token and save it.
fn run_ssh_key(
    server: Option<String>,
    token: Option<String>,
    output: Option<String>,
    fc: &config::AppConfig,
) -> anyhow::Result<()> {
    auth::set_token_file_path(fc.paths.resolve_token_file());

    // Resolve token: CLI > env > config file > saved token file
    let resolved_token = token
        .or_else(|| std::env::var("SAW_TOKEN").ok())
        .or_else(|| {
            let t = fc.client.token.clone();
            if t.is_empty() { None } else { Some(t) }
        })
        .or_else(|| auth::load_token().ok())
        .filter(|t| !t.is_empty());

    let token_str = match resolved_token {
        Some(t) => t,
        None => anyhow::bail!(
            "No token available. Set token via --token, SAW_TOKEN env, or config file."
        ),
    };

    // Derive AuthKey and SSH keypair
    let auth_key = auth::AuthKey::derive(&token_str);
    let pubkey_str = auth_key.derive_ssh_public_key();

    // Generate OpenSSH private key
    let private_key_pem = generate_openssh_private_key(&auth_key);

    // Resolve server address
    let resolved_server = server
        .or_else(|| std::env::var("SAW_SERVER").ok())
        .or_else(|| {
            let s = fc.client.server.clone();
            if s.is_empty() { None } else { Some(s) }
        });

    // Parse host and port from server address
    let (host, port) = match &resolved_server {
        Some(addr) => match addr.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.to_string()),
            _ => (addr.clone(), "18708".to_string()),
        },
        None => (String::new(), String::new()),
    };

    // Generate token identifier: last 8 chars of the public key base64
    // (the beginning is the same for all ed25519 keys, the unique part is at the end)
    let token_id = pubkey_str
        .split_whitespace()
        .nth(1)
        .map(|b64| {
            let start = b64.len().saturating_sub(8);
            b64[start..].to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Build key filename: saw_{host}-{port}_{hash8} or saw_{hash8}
    let ssh_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".ssh");

    let key_filename = if host.is_empty() {
        format!("saw_{}", token_id)
    } else {
        format!("saw_{}-{}_{}", host.replace('.', "-"), port, token_id)
    };

    let default_path = ssh_dir.join(&key_filename);
    let key_path = match output {
        Some(ref p) => std::path::PathBuf::from(p),
        None => default_path,
    };

    // Ensure .ssh directory exists
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write private key
    std::fs::write(&key_path, private_key_pem)?;

    // Set permissions: owner-only on Unix, skip on Windows (NTFS ACLs apply)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        log::debug!("Private key written (NTFS ACLs apply): {:?}", key_path);
    }

    println!("SSH private key saved to: {}", key_path.display());
    println!("Public key: {}", pubkey_str);

    // Display path: use ~/ on Unix, %USERPROFILE%\.ssh\ on Windows, or just filename
    let display_path = {
        let home = dirs::home_dir();
        if let Some(ref h) = home {
            if let Ok(rel) = key_path.strip_prefix(h) {
                if cfg!(windows) {
                    format!(
                        "%USERPROFILE%\\{}",
                        rel.to_str().unwrap_or(key_path.to_str().unwrap_or("?"))
                    )
                } else {
                    format!(
                        "~/{}",
                        rel.to_str().unwrap_or(key_path.to_str().unwrap_or("?"))
                    )
                }
            } else {
                key_path.display().to_string()
            }
        } else {
            key_path.display().to_string()
        }
    };

    // Show usage
    let display_host = if host.is_empty() { "HOST" } else { &host };
    let display_port = if port.is_empty() { "PORT" } else { &port };

    println!();
    println!("Usage:");
    if port == "22" {
        println!("  ssh -i {} {}", display_path, display_host);
    } else {
        println!(
            "  ssh -i {} -p {} {}",
            display_path, display_port, display_host
        );
    }
    println!();
    println!("To auto-select this key, add to ~/.ssh/config:");
    println!();
    println!("  Host {}", display_host);
    if port != "22" {
        println!("      Port {}", display_port);
    }
    println!("      IdentityFile {}", display_path);
    println!("      IdentitiesOnly yes");
    println!();
    if host.is_empty() {
        println!("Tip: specify --server to fill in host and port");
        println!("  saw-client ssh-key --server my.host:18708");
    } else {
        println!("Then simply: ssh {}", host);
    }

    Ok(())
}

/// Generate an OpenSSH format private key string from AuthKey.
fn generate_openssh_private_key(auth_key: &auth::AuthKey) -> String {
    auth_key.derive_openssh_private_key()
}
