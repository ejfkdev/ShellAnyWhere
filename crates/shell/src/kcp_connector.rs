use crate::local_io;
use crate::pty::{self, ShellKind};
use crate::session::Session;
use crate::virtual_term::VirtualTermHandle;
use anyhow::Result;
use saw_core::config::ResolvedPaths;
use saw_core::protocol::control::{Control, SessionInfo};
use saw_core::protocol::kcp_transport::{self, stream_id};

/// KCP-based agent connector: connects to server, registers session, relays I/O.
pub struct KcpAgentConnector {
    server: String,
    token: String,
    reconnect_fast_attempts: usize,
    reconnect_fast_min: std::time::Duration,
    reconnect_fast_max: std::time::Duration,
    reconnect_slow_min: std::time::Duration,
    reconnect_slow_max: std::time::Duration,
    connect_timeout: std::time::Duration,
    paths: ResolvedPaths,
    session_update_interval: std::time::Duration,
    ssh_key_forward: bool,
    flush_interval: std::time::Duration,
    io_compress: bool,
    io_diff: bool,
}

impl KcpAgentConnector {
    pub fn new(server: String, token: String) -> Self {
        Self {
            server,
            token,
            reconnect_fast_attempts: 100,
            reconnect_fast_min: std::time::Duration::from_secs(1),
            reconnect_fast_max: std::time::Duration::from_secs(2),
            reconnect_slow_min: std::time::Duration::from_secs(60),
            reconnect_slow_max: std::time::Duration::from_secs(120),
            connect_timeout: std::time::Duration::from_secs(5),
            paths: ResolvedPaths::default(),
            session_update_interval: std::time::Duration::from_secs(5),
            ssh_key_forward: true,
            flush_interval: std::time::Duration::from_millis(100),
            io_compress: false,
            io_diff: false,
        }
    }

    pub fn with_ssh_key_forward(mut self, forward: bool) -> Self {
        self.ssh_key_forward = forward;
        self
    }

    pub fn with_flush_interval(mut self, interval: std::time::Duration) -> Self {
        self.flush_interval = interval;
        self
    }

    pub fn with_io_compress(mut self, compress: bool) -> Self {
        self.io_compress = compress;
        self
    }

    pub fn with_io_diff(mut self, diff: bool) -> Self {
        self.io_diff = diff;
        self
    }

    pub fn with_reconnect_params(
        mut self,
        fast_attempts: usize,
        fast_min: std::time::Duration,
        fast_max: std::time::Duration,
        slow_min: std::time::Duration,
        slow_max: std::time::Duration,
    ) -> Self {
        self.reconnect_fast_attempts = fast_attempts;
        self.reconnect_fast_min = fast_min;
        self.reconnect_fast_max = fast_max;
        self.reconnect_slow_min = slow_min;
        self.reconnect_slow_max = slow_max;
        self
    }

    pub fn with_paths(mut self, paths: ResolvedPaths) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_session_update_interval(mut self, interval: std::time::Duration) -> Self {
        self.session_update_interval = interval;
        self
    }

    pub fn with_connect_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Main run loop: local I/O always active, KCP connection in background.
    pub async fn run(self, mut session: Session) -> Result<()> {
        let _raw_guard = local_io::RawModeGuard::enter();
        let mut stdin_reader = local_io::StdinReader::new();
        let mut stdin_filter = local_io::StdinResponseFilter::new();
        let mut resize_watcher = local_io::ResizeWatcher::new();

        let _focus_guard = FocusGuard::enter(true);

        let (cols, rows) = pty::get_terminal_size();
        if let Err(e) = session.resize(cols, rows) {
            log::debug!("Initial resize failed: {}", e);
        }

        let session_info = session.session_info();
        let mut attempt_count: usize = 0;

        let ssh_public_keys = if self.ssh_key_forward {
            let config_dir = self
                .paths
                .token_file
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            super::connector::read_ssh_public_keys(config_dir)
        } else {
            Vec::new()
        };

        let (init_cols, init_rows) = pty::get_terminal_size();
        let virtual_term = VirtualTermHandle::new(init_rows, init_cols);

        loop {
            let mut query_interceptor = local_io::QueryInterceptor::new();
            {
                let (cols, rows) = pty::get_terminal_size();
                query_interceptor.set_terminal_size(rows, cols);
            }

            let delay = if attempt_count == 0 {
                std::time::Duration::ZERO
            } else if attempt_count <= self.reconnect_fast_attempts {
                random_duration(self.reconnect_fast_min, self.reconnect_fast_max)
            } else {
                random_duration(self.reconnect_slow_min, self.reconnect_slow_max)
            };

            let connect_fut = Self::establish_connection(
                self.server.clone(),
                self.token.clone(),
                &session_info,
                delay,
                self.connect_timeout,
                ssh_public_keys.clone(),
            );
            tokio::pin!(connect_fut);

            let connected = loop {
                tokio::select! {
                    output = session.reader.read_output() => {
                        match output {
                            Some(data) => {
                                let intercept = query_interceptor.process(&data);
                                if let Some(ref new_cwd) = intercept.extracted_cwd
                                    && new_cwd != &session.cwd {
                                        session.cwd = new_cwd.clone();
                                    }
                                if let Some(ref new_title) = intercept.extracted_title
                                    && new_title != &session.title {
                                        session.title = new_title.clone();
                                    }
                                if !intercept.responses.is_empty() {
                                    session.process.ensure_echo_off();
                                    if let Err(e) = session.writer.write_responses(&intercept.responses) {
                                        log::debug!("Failed to write synthetic responses: {}", e);
                                    }
                                }
                                if let Err(e) = local_io::write_local_stdout(&intercept.display_data) {
                                    log::debug!("Local stdout write error: {}", e);
                                }
                                virtual_term.process(intercept.display_data.clone());
                                if intercept.has_screen_boundary {
                                    use std::io::Write;
                                    let _ = std::io::stdout().write_all(b"\x1b[?1004h");
                                    let _ = std::io::stdout().flush();
                                    stdin_filter.set_shell_focus_enabled(false);
                                }
                                if intercept.has_clear_scrollback {
                                    virtual_term.clear_scrollback();
                                }
                                if intercept.has_focus_enable {
                                    stdin_filter.set_shell_focus_enabled(true);
                                }
                            }
                            None => {
                                log::info!("Session {} PTY closed", session.id);
                                return Ok(());
                            }
                        }
                    }
                    stdin_data = stdin_reader.read() => {
                        if let Some(data) = stdin_data {
                            let filtered = stdin_filter.filter(&data);
                            if !filtered.is_empty()
                                && let Err(e) = session.writer.write_input(&filtered) {
                                    log::debug!("PTY write error: {}", e);
                                }
                        }
                    }
                    resize = resize_watcher.next_resize() => {
                        if let Some((cols, rows)) = resize {
                            query_interceptor.set_terminal_size(rows, cols);
                            if let Err(e) = session.resize(cols, rows) {
                                log::debug!("Resize failed: {}", e);
                            }
                        }
                    }
                    result = &mut connect_fut => {
                        match result {
                            Ok(connected) => break Some(connected),
                            Err(e) => {
                                log::info!("KCP connect attempt {} failed: {}", attempt_count + 1, e);
                                break None;
                            }
                        }
                    }
                }
            };

            #[allow(clippy::let_underscore_future)]
            let _ = connect_fut;

            match connected {
                Some(connected) => {
                    attempt_count = 0;
                    match self
                        .relay_loop(
                            connected,
                            &mut session,
                            &mut stdin_reader,
                            &mut stdin_filter,
                            &mut resize_watcher,
                            &virtual_term,
                            self.flush_interval,
                            self.io_diff,
                            self.io_compress,
                        )
                        .await
                    {
                        Ok(()) => {
                            log::info!("Session {} PTY closed", session.id);
                            return Ok(());
                        }
                        Err(e) => {
                            log::debug!(
                                "KCP connection lost, will reconnect (attempt {}): {}",
                                attempt_count,
                                e
                            );
                            attempt_count += 1;
                            let (cols, rows) = pty::get_terminal_size();
                            if let Err(e) = session.resize(cols, rows) {
                                log::debug!("Failed to restore local terminal size: {}", e);
                            }
                        }
                    }
                }
                None => {
                    attempt_count += 1;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn establish_connection(
        server: String,
        token: String,
        session_info: &SessionInfo,
        delay: std::time::Duration,
        connect_timeout: std::time::Duration,
        ssh_public_keys: Vec<String>,
    ) -> Result<KcpRelayState> {
        if !delay.is_zero() {
            log::debug!("Reconnect delay: {:?}", delay);
            tokio::time::sleep(delay).await;
        }

        // Parse server address: use KCP port (server_port + 1)
        let kcp_addr = resolve_kcp_addr(&server)?;

        log::info!("Connecting via KCP to {}", kcp_addr);
        let mut mux = tokio::time::timeout(connect_timeout, kcp_transport::connect_kcp(kcp_addr))
            .await
            .map_err(|_| {
                anyhow::anyhow!("KCP connect timeout ({}s)", connect_timeout.as_secs())
            })??;

        // Open both virtual streams BEFORE spawning mux.run()
        let mut control_stream = mux.open_stream(stream_id::CONTROL);
        let term_stream = mux.open_stream(stream_id::TERMINAL_IO);

        // Spawn the mux run loop BEFORE any I/O — it must be running to actually
        // send/receive frames on the KCP connection.
        tokio::spawn(async move {
            if let Err(e) = mux.run().await {
                log::debug!("KCP multiplex run ended: {}", e);
            }
        });

        // Authenticate: [0x01=agent][token_len:u16][token_bytes]
        {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let token_bytes = token.as_bytes();
            let mut auth_msg = vec![0x01]; // Agent role
            auth_msg.extend_from_slice(&(token_bytes.len() as u16).to_be_bytes());
            auth_msg.extend_from_slice(token_bytes);
            control_stream.write_all(&auth_msg).await?;

            let mut resp = [0u8; 1];
            control_stream.read_exact(&mut resp).await?;
            if resp[0] != 0x00 {
                anyhow::bail!("KCP authentication failed");
            }
        }

        log::info!(
            "KCP authenticated, registering session {}",
            session_info.session_id
        );

        // Register session
        let register = Control::SessionRegister {
            session: session_info.clone(),
            ssh_public_keys,
        };
        let data = encode_control(&register)?;
        {
            use tokio::io::AsyncWriteExt;
            control_stream.write_all(&data).await?;
        }

        Ok(KcpRelayState {
            session_id: session_info.session_id.clone(),
            control_stream,
            term_stream,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn relay_loop(
        &self,
        relay_state: KcpRelayState,
        session: &mut Session,
        stdin_reader: &mut local_io::StdinReader,
        stdin_filter: &mut local_io::StdinResponseFilter,
        resize_watcher: &mut local_io::ResizeWatcher,
        virtual_term: &VirtualTermHandle,
        _flush_interval: std::time::Duration,
        _io_diff: bool,
        _io_compress: bool,
    ) -> Result<()> {
        let KcpRelayState {
            session_id,
            mut control_stream,
            mut term_stream,
        } = relay_state;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut active_source = ActiveSource::Local;
        let mut cached_active_source = ActiveSource::Local;
        let mut local_size: (u16, u16) = pty::get_terminal_size();
        let mut remote_size: (u16, u16) = local_size;
        let mut suppress_resize_watcher = false;

        let writer = session.writer.clone();

        let shell_kind = ShellKind::from_path(&session.shell);
        let needs_inject = shell_kind.needs_pty_inject();
        let mut shell_injected = false;

        let needs_cwd_poll =
            !shell_kind.native_osc7() && !matches!(shell_kind, ShellKind::Bash | ShellKind::Cmd);
        let cwd_poll_interval = if needs_inject {
            std::time::Duration::from_secs(3)
        } else {
            std::time::Duration::from_secs(1)
        };
        let child_pid = session.process.child_pid();
        let mut cwd_poll_rx = if needs_cwd_poll {
            let (tx, rx) = tokio::sync::mpsc::channel::<String>(4);
            tokio::spawn(async move {
                let mut poll_count: u32 = 0;
                loop {
                    tokio::time::sleep(cwd_poll_interval).await;
                    poll_count += 1;
                    match pty::read_process_cwd(child_pid) {
                        Some(cwd) => {
                            if poll_count <= 3 || poll_count % 10 == 0 {
                                log::info!(
                                    "CWD poll #{}: pid={} cwd={}",
                                    poll_count,
                                    child_pid,
                                    cwd
                                );
                            }
                            if tx.send(cwd).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            log::info!(
                                "CWD poll: read_process_cwd({}) failed, stopping",
                                child_pid
                            );
                            break;
                        }
                    }
                }
            });
            Some(rx)
        } else {
            None
        };

        let mut cmd_tracker = crate::command::CommandTracker::new();
        let mut last_update_time = std::time::Instant::now();
        let mut last_activity_epoch: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let update_interval = self.session_update_interval;

        let mut query_interceptor = local_io::QueryInterceptor::new();
        {
            let (cols, rows) = pty::get_terminal_size();
            query_interceptor.set_terminal_size(rows, cols);
        }

        let mut control_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut term_read_buf = vec![0u8; 65536];
        let mut needs_replay = false;

        // Send latest session info (title, cwd, etc.) to server immediately
        // so the server has up-to-date state after a reconnect.
        {
            let _ = send_session_update(
                session,
                &mut last_update_time,
                update_interval,
                true,
                &mut control_stream,
            )
            .await;
        }

        loop {
            // Send replay buffer after ClientAttached (outside select! to avoid term_stream borrow)
            if needs_replay {
                needs_replay = false;
                let replay = virtual_term.serialize_screen().await;
                if !replay.is_empty() {
                    if let Err(e) = term_stream.write_all(&replay).await {
                        log::debug!("KCP replay write error: {}", e);
                    } else {
                        log::info!("KCP replay: sent {} bytes", replay.len());
                    }
                }
            }
            tokio::select! {
                // 1. PTY output → local stdout + remote
                output = session.reader.read_output() => {
                    match output {
                        Some(data) => {
                            if needs_inject && !shell_injected {
                                shell_injected = true;
                                if let Some(init_cmd) = shell_kind.pty_inject_cmd()
                                    && let Err(e) = writer.write_response(init_cmd)
                                {
                                    log::debug!("Failed to inject OSC 7 hook: {}", e);
                                }
                            }
                            {
                                let now = std::time::Instant::now();
                                if now.duration_since(last_update_time) > std::time::Duration::from_secs(1) {
                                    last_activity_epoch = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                }
                            }
                            session.last_activity_at = last_activity_epoch;
                            if data.is_empty() { continue; }

                            let intercept = query_interceptor.process(&data);
                            if let Some(ref new_cwd) = intercept.extracted_cwd
                                && new_cwd != &session.cwd {
                                    session.cwd = new_cwd.clone();
                                    let _ = send_session_update(session, &mut last_update_time, update_interval, true, &mut control_stream).await;
                            }
                            if let Some(ref new_title) = intercept.extracted_title
                                && new_title != &session.title {
                                    session.title = new_title.clone();
                                    let _ = send_session_update(session, &mut last_update_time, update_interval, true, &mut control_stream).await;
                            }
                            if let Some((ref title, ref body)) = intercept.extracted_notification
                                && let Ok(data) = encode_control(&Control::DesktopNotification {
                                    session_id: session_id.clone(),
                                    title: title.clone(),
                                    body: body.clone(),
                                })
                            {
                                log::info!("DesktopNotification: title={:?} body={:?}", title, body);
                                let _ = control_stream.write_all(&data).await;
                            }
                            if !intercept.responses.is_empty() {
                                session.process.ensure_echo_off();
                                if let Err(e) = writer.write_responses(&intercept.responses) {
                                    log::debug!("Failed to write synthetic responses: {}", e);
                                }
                            }
                            if let Err(e) = local_io::write_local_stdout(&intercept.display_data) {
                                log::debug!("Local stdout write error: {}", e);
                            }
                            if intercept.has_screen_boundary {
                                use std::io::Write;
                                let _ = std::io::stdout().write_all(b"\x1b[?1004h");
                                let _ = std::io::stdout().flush();
                                stdin_filter.set_shell_focus_enabled(false);
                            }
                            if intercept.has_clear_scrollback {
                                virtual_term.clear_scrollback();
                            }
                            if intercept.has_focus_enable {
                                stdin_filter.set_shell_focus_enabled(true);
                            }

                            let remote_data = if intercept.was_filtered {
                                local_io::final_defense_filter_owned(intercept.display_data)
                            } else {
                                intercept.display_data
                            };

                            // Send directly on TerminalIO stream (realtime mode)
                            if !remote_data.is_empty() {
                                if log::log_enabled!(log::Level::Trace) {
                                    let t = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs_f64() * 1000.0;
                                    log::trace!("[LATENCY] shell send_data {} bytes at {:.3}ms", remote_data.len(), t);
                                }
                                if let Err(e) = term_stream.write_all(&remote_data).await {
                                    log::debug!("KCP TerminalIO write error: {}", e);
                                    return Err(anyhow::anyhow!("KCP connection lost"));
                                }
                                virtual_term.process(remote_data);
                            }
                        }
                        None => {
                            log::info!("Shell exited, sending SessionClose for {}", session_id);
                            if let Ok(data) = encode_control(&Control::SessionClose {
                                session_id: session_id.clone(),
                            }) {
                                let _ = control_stream.write_all(&data).await;
                            }
                            return Ok(());
                        }
                    }
                }
                // 2. Local stdin → PTY input
                stdin_data = stdin_reader.read() => {
                    if let Some(data) = stdin_data {
                        let filtered = stdin_filter.filter(&data);
                        if let Some(focused) = stdin_filter.take_focus_event()
                            && focused {
                                last_activity_epoch = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs();
                                session.last_activity_at = last_activity_epoch;
                                if active_source == ActiveSource::Local {
                                    local_size = pty::get_terminal_size();
                                    suppress_resize_watcher = true;
                                    if let Err(e) = session.resize(local_size.0, local_size.1) {
                                        log::debug!("Focus resize failed: {}", e);
                                    }
                                }
                                let _ = send_session_update(session, &mut last_update_time, update_interval, true, &mut control_stream).await;
                        }
                        if !filtered.is_empty() {
                            active_source = ActiveSource::Local;
                            local_size = pty::get_terminal_size();
                            last_activity_epoch = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            session.last_activity_at = last_activity_epoch;
                            if let Err(e) = writer.write_input(&filtered) {
                                log::debug!("PTY write error: {}", e);
                            }
                        }
                        if let Some(cmd) = cmd_tracker.feed(&filtered) {
                            if session.first_command.is_none() {
                                session.first_command = Some(cmd.clone());
                            }
                            let _ = send_session_update(session, &mut last_update_time, update_interval, true, &mut control_stream).await;
                        } else {
                            let _ = send_session_update(session, &mut last_update_time, update_interval, false, &mut control_stream).await;
                        }
                    }
                }
                // 3. Remote control messages
                ctrl_result = recv_control(&mut control_stream, &mut control_buf) => {
                    match ctrl_result {
                        Ok(ctrl) => match ctrl {
                                    Control::ClientAttached { client_id, .. } => {
                                        log::debug!("Client {} attached", client_id);
                                        virtual_term.on_client_attached(client_id.clone());
                                        active_source = ActiveSource::Remote;
                                        needs_replay = true;
                                    }
                                    Control::ClientRefresh { client_id, .. } => {
                                        log::debug!("Client {} requested screen refresh", client_id);
                                        needs_replay = true;
                                    }
                                    Control::ClientResize { cols, rows, .. } => {
                                        log::info!("KCP shell: received ClientResize {}x{}", cols, rows);
                                        if cols == 0 || rows == 0 { continue; }
                                        active_source = ActiveSource::Remote;
                                        remote_size = (cols, rows);
                                        last_activity_epoch = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs();
                                        session.last_activity_at = last_activity_epoch;
                                        virtual_term.resize(rows, cols);
                                        cached_active_source = ActiveSource::Remote;
                                        query_interceptor.set_terminal_size(rows, cols);
                                        suppress_resize_watcher = true;
                                        if let Err(e) = session.resize(cols, rows) {
                                            log::warn!("Resize failed: {}", e);
                                        }
                                    }
                                    Control::SessionDetach { client_id, .. } => {
                                        log::info!("Client {} detached", client_id);
                                        let was_last = virtual_term.on_client_detached(client_id.clone()).await;
                                        if was_last {
                                            active_source = ActiveSource::Local;
                                            let (cols, rows) = pty::get_terminal_size();
                                            local_size = (cols, rows);
                                            virtual_term.resize(rows, cols);
                                        }
                                    }
                                    Control::ClientActive { client_id: active_client_id, cols, rows, .. } => {
                                        active_source = ActiveSource::Remote;
                                        if cols > 0 && rows > 0 {
                                            remote_size = (cols, rows);
                                            virtual_term.resize(rows, cols);
                                            query_interceptor.set_terminal_size(rows, cols);
                                            suppress_resize_watcher = true;
                                            if let Err(e) = session.resize(cols, rows) {
                                                log::warn!("Resize failed: {}", e);
                                            }
                                        }
                                        cached_active_source = ActiveSource::Remote;
                                        log::debug!("Active client {} ({}x{})", active_client_id, cols, rows);
                                    }
                                    Control::ClientSetTitle { ref title, .. } => {
                                        use std::io::Write;
                                        let osc = format!("\x1b]0;{}\x07", title);
                                        let _ = std::io::stdout().write_all(osc.as_bytes());
                                        let _ = std::io::stdout().flush();
                                    }
                                    other => log::debug!("Unhandled control: {:?}", other),
                                },
                        Err(e) => return Err(anyhow::anyhow!("KCP control read error: {}", e)),
                    }
                }
                // 4. Terminal resize
                resize = resize_watcher.next_resize() => {
                    if let Some((cols, rows)) = resize {
                        if suppress_resize_watcher {
                            suppress_resize_watcher = false;
                        } else if active_source == ActiveSource::Local {
                            local_size = (cols, rows);
                            virtual_term.resize(rows, cols);
                            query_interceptor.set_terminal_size(rows, cols);
                            cached_active_source = ActiveSource::Local;
                            if let Err(e) = session.resize(cols, rows) {
                                log::debug!("Resize failed: {}", e);
                            }
                        } else {
                            local_size = (cols, rows);
                            virtual_term.resize(rows, cols);
                        }
                    }
                }
                // 5. CWD poll
                Some(new_cwd) = async {
                    match cwd_poll_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if cwd_poll_rx.is_some() => {
                    if new_cwd != session.cwd {
                        log::info!("CWD poll: cwd changed to {}", new_cwd);
                        session.cwd = new_cwd;
                        let _ = send_session_update(session, &mut last_update_time, update_interval, true, &mut control_stream).await;
                    }
                }
                // 6. TerminalIO input from remote (client keystrokes → PTY)
                n = term_stream.read(&mut term_read_buf) => {
                    match n {
                        Ok(0) => {
                            log::debug!("KCP TerminalIO stream closed");
                        }
                        Ok(n) => {
                            if !term_read_buf[..n].is_empty()
                                && let Err(e) = writer.write_input(&term_read_buf[..n]) {
                                    log::debug!("PTY write error for TerminalIO input: {}", e);
                            }
                        }
                        Err(e) => {
                            log::debug!("KCP TerminalIO read error: {}", e);
                        }
                    }
                }
            }

            // Detect active source switch and resize PTY
            if active_source != cached_active_source {
                match active_source {
                    ActiveSource::Local => {
                        let (cols, rows) = local_size;
                        suppress_resize_watcher = true;
                        if let Err(e) = session.resize(cols, rows) {
                            log::debug!("Resize to local size failed: {}", e);
                        }
                    }
                    ActiveSource::Remote => {
                        let (cols, rows) = remote_size;
                        suppress_resize_watcher = true;
                        if let Err(e) = session.resize(cols, rows) {
                            log::debug!("Resize to remote size failed: {}", e);
                        }
                    }
                }
                cached_active_source = active_source;
            }
        }
    }
}

struct KcpRelayState {
    session_id: String,
    control_stream: kcp_transport::KcpVirtualStream,
    term_stream: kcp_transport::KcpVirtualStream,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum ActiveSource {
    Local,
    Remote,
}

use crate::reconnect::random_duration;

/// Parse server address and compute KCP port (TCP port + 1).
fn resolve_kcp_addr(server: &str) -> anyhow::Result<std::net::SocketAddr> {
    // server format: "host:port" or "[ipv6]:port"
    let addr: std::net::SocketAddr = server
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid server address: {}", server))?;
    let kcp_port = addr.port() + 1;
    Ok(std::net::SocketAddr::new(addr.ip(), kcp_port))
}

fn encode_control(ctrl: &Control) -> anyhow::Result<Vec<u8>> {
    let data = bincode::serde::encode_to_vec(ctrl, bincode::config::standard())?;
    // Length-prefixed: [4-byte BE len][bincode data]
    let len = data.len() as u32;
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&data);
    Ok(out)
}

fn decode_control(data: &[u8]) -> anyhow::Result<Control> {
    let (ctrl, _): (Control, _) =
        bincode::serde::decode_from_slice(data, bincode::config::standard())?;
    Ok(ctrl)
}

/// Read a length-prefixed Control message from the control stream.
async fn recv_control(
    stream: &mut kcp_transport::KcpVirtualStream,
    buf: &mut Vec<u8>,
) -> anyhow::Result<Control> {
    use tokio::io::AsyncReadExt;
    // Read 4-byte length prefix
    let mut len_buf = [0u8; 4];
    let mut offset = 0;
    while offset < 4 {
        match stream.read(&mut len_buf[offset..]).await {
            Ok(0) => anyhow::bail!("control stream closed"),
            Ok(n) => offset += n,
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        anyhow::bail!("control message too large: {} bytes", len);
    }
    if buf.len() < len {
        buf.resize(len, 0);
    }
    // Read payload
    let mut offset = 0;
    while offset < len {
        match stream.read(&mut buf[offset..len]).await {
            Ok(0) => anyhow::bail!("control stream closed mid-message"),
            Ok(n) => offset += n,
            Err(e) => return Err(e.into()),
        }
    }
    decode_control(&buf[..len])
}

async fn send_session_update(
    session: &Session,
    last_update_time: &mut std::time::Instant,
    update_interval: std::time::Duration,
    force: bool,
    control_stream: &mut kcp_transport::KcpVirtualStream,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let now = std::time::Instant::now();
    if force || now.duration_since(*last_update_time) >= update_interval {
        *last_update_time = now;
        let info = session.session_info();
        // encode_control already adds length prefix
        let data = encode_control(&Control::SessionUpdate { session: info })?;
        control_stream.write_all(&data).await?;
    }
    Ok(())
}

use crate::focus::FocusGuard;
