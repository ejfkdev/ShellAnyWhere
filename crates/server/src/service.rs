//! Platform-specific service install/uninstall.
//!
//! - Linux: systemd unit file
//! - macOS: launchd plist
//! - Windows: Windows Service (via windows-service crate)

use anyhow::{Context, Result};

#[cfg(any(target_os = "linux", target_os = "windows"))]
const SERVICE_NAME: &str = "saw-server";
const SERVICE_DISPLAY: &str = "ShellAnyWhere Server";
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.shellanywhere.server";

// ── Unsupported platforms (FreeBSD, illumos, etc.) ─────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_install(_exe: &std::path::Path, _extra_args: &[String]) -> Result<()> {
    anyhow::bail!("Service installation is not supported on this platform")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_uninstall() -> Result<()> {
    anyhow::bail!("Service uninstallation is not supported on this platform")
}

// ── Public API ─────────────────────────────────────────────────────────────

pub fn install(extra_args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("Cannot determine current executable path")?;
    let exe_str = exe.to_string_lossy();

    println!("Installing {} as a system service...", SERVICE_DISPLAY);
    println!("  Executable: {}", exe_str);
    if !extra_args.is_empty() {
        println!("  Extra args: {}", extra_args.join(" "));
    }

    platform_install(&exe, extra_args)?;

    println!("{} service installed and started.", SERVICE_DISPLAY);
    Ok(())
}

pub fn uninstall() -> Result<()> {
    println!("Uninstalling {} service...", SERVICE_DISPLAY);

    platform_uninstall()?;

    println!("{} service uninstalled.", SERVICE_DISPLAY);
    Ok(())
}

// ── Linux (systemd) ────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn platform_install(exe: &std::path::Path, extra_args: &[String]) -> Result<()> {
    let unit_dir = dirs::home_dir()
        .context("Cannot determine home directory")?
        .join(".config")
        .join("systemd")
        .join("user");
    std::fs::create_dir_all(&unit_dir)?;

    let unit_path = unit_dir.join(format!("{SERVICE_NAME}.service"));
    let exe_str = exe.to_string_lossy();

    let args_str = if extra_args.is_empty() {
        String::new()
    } else {
        format!(" {}", extra_args.join(" "))
    };

    let unit = format!(
        "[Unit]\n\
         Description={SERVICE_DISPLAY}\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe_str}{args_str}\n\
         Restart=always\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );

    std::fs::write(&unit_path, &unit)
        .with_context(|| format!("Failed to write {:?}", unit_path))?;

    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", "--now", SERVICE_NAME])?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_uninstall() -> Result<()> {
    run("systemctl", &["--user", "stop", SERVICE_NAME]).ok();
    run("systemctl", &["--user", "disable", SERVICE_NAME]).ok();

    let unit_dir = dirs::home_dir()
        .context("Cannot determine home directory")?
        .join(".config")
        .join("systemd")
        .join("user");
    let unit_path = unit_dir.join(format!("{SERVICE_NAME}.service"));

    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("Failed to remove {:?}", unit_path))?;
    }

    run("systemctl", &["--user", "daemon-reload"])?;
    Ok(())
}

// ── macOS (launchd) ────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn platform_install(exe: &std::path::Path, extra_args: &[String]) -> Result<()> {
    let plist_dir = dirs::home_dir()
        .context("Cannot determine home directory")?
        .join("Library")
        .join("LaunchAgents");
    std::fs::create_dir_all(&plist_dir)?;

    let plist_path = plist_dir.join(format!("{LAUNCHD_LABEL}.plist"));

    // Unload existing plist first if present
    if plist_path.exists() {
        run("launchctl", &["unload", &plist_path.to_string_lossy()]).ok();
    }

    let exe_str = exe.to_string_lossy();

    // Build ProgramArguments array: exe + extra_args
    let mut args_xml = format!("                 <string>{exe_str}</string>\n");
    for arg in extra_args {
        args_xml.push_str(&format!("                 <string>{arg}</string>\n"));
    }

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
             <key>Label</key>\n\
             <string>{LAUNCHD_LABEL}</string>\n\
             <key>ProgramArguments</key>\n\
             <array>\n\
{args_xml}\
             </array>\n\
             <key>RunAtLoad</key>\n\
             <true/>\n\
             <key>KeepAlive</key>\n\
             <true/>\n\
             <key>StandardOutPath</key>\n\
             <string>/tmp/saw-server.stdout.log</string>\n\
             <key>StandardErrorPath</key>\n\
             <string>/tmp/saw-server.stderr.log</string>\n\
         </dict>\n\
         </plist>\n"
    );

    std::fs::write(&plist_path, &plist)
        .with_context(|| format!("Failed to write {:?}", plist_path))?;

    run("launchctl", &["load", &plist_path.to_string_lossy()])?;

    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_uninstall() -> Result<()> {
    let plist_dir = dirs::home_dir()
        .context("Cannot determine home directory")?
        .join("Library")
        .join("LaunchAgents");
    let plist_path = plist_dir.join(format!("{LAUNCHD_LABEL}.plist"));

    if plist_path.exists() {
        run("launchctl", &["unload", &plist_path.to_string_lossy()]).ok();
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("Failed to remove {:?}", plist_path))?;
    }

    Ok(())
}

// ── Windows (Windows Service) ──────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn platform_install(exe: &std::path::Path, _extra_args: &[String]) -> Result<()> {
    use windows_service::{
        service::{
            ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
            ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
            ServiceType,
        },
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    // If service already exists, stop and delete it first so we can recreate cleanly
    if let Ok(existing) =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)
    {
        let _ = existing.stop();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let _ = existing.delete();
        // Wait for deletion to take effect
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    let service_info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.as_os_str().into(),
        launch_arguments: vec!["--run-as-service".into()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };

    let service = manager.create_service(
        &service_info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
    )?;

    // Configure failure actions: restart after 3 seconds, reset fail counter after 60 seconds
    let failure_actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(std::time::Duration::from_secs(60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: std::time::Duration::from_secs(3),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: std::time::Duration::from_secs(3),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: std::time::Duration::from_secs(3),
            },
        ]),
    };
    service.update_failure_actions(failure_actions)?;
    service.start(&[] as &[&str])?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_uninstall() -> Result<()> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)?;

    service.stop()?;
    // Give the service a moment to stop
    std::thread::sleep(std::time::Duration::from_secs(2));
    service.delete()?;

    Ok(())
}

/// Windows Service entry point. Called from main() when `--run-as-service` is detected.
#[cfg(target_os = "windows")]
pub fn run_as_service() -> Result<()> {
    use std::sync::mpsc;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    define_windows_service!(ffi_service_main, windows_service_main);

    fn windows_service_main(_args: Vec<std::ffi::OsString>) {
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .expect("Failed to register service control handler");

        status_handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: std::time::Duration::default(),
                process_id: None,
            })
            .expect("Failed to set service status to Running");

        // Run the server in a tokio runtime
        let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let server_handle = runtime.spawn(async {
            if let Err(e) = run_server_default().await {
                log::error!("Server error: {}", e);
            }
        });

        // Wait for shutdown signal
        let _ = shutdown_rx.recv();

        // Shutdown tokio runtime
        runtime.block_on(async {
            server_handle.abort();
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        });

        status_handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: std::time::Duration::default(),
                process_id: None,
            })
            .expect("Failed to set service status to Stopped");
    }

    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("Failed to start Windows Service dispatcher: {}", e))
}

/// Default server startup for Windows Service mode (no CLI args, use config file defaults).
#[cfg(target_os = "windows")]
async fn run_server_default() -> anyhow::Result<()> {
    use saw_core::config;
    use saw_core::crypto::auth;
    use saw_server::listener;

    let fc = config::load_config_file().unwrap_or_default();
    auth::set_token_file_path(fc.paths.resolve_token_file());

    let rc = config::ResolvedServerConfig::resolve(
        None, None, None, None, None, None, None, None, None, None, &fc,
    );

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
        web_dir: None,
    };
    listener::run_server(server_config).await
}

// ── Helpers ────────────────────────────────────────────────────────────────

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run {} {}", cmd, args.join(" ")))?;

    if !status.success() {
        anyhow::bail!("{} {} exited with {}", cmd, args.join(" "), status);
    }
    Ok(())
}
