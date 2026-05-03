use crate::session_mgr::SharedSessionManager;
use bytes::Bytes;
use saw_core::protocol::control::Control;

/// Helper: forward control to clients and clean up any dead clients detected.
/// Returns (encoded_bytes, ok): encoded_bytes can be reused for upstream forwarding.
fn forward_and_cleanup(
    session_mgr: &SharedSessionManager,
    session_id: &str,
    ctrl: Control,
) -> (Option<Bytes>, bool) {
    let (encoded, ok, dead_clients) = session_mgr.forward_to_client(session_id, ctrl);
    if !dead_clients.is_empty() {
        session_mgr.remove_clients_by_id(session_id, &dead_clients);
    }
    (encoded, ok)
}

/// Handle a control message received from an agent.
/// Forwards control-plane messages to the attached client.
/// Data relay (terminal I/O) happens through separate ContextStream terminal_io streams.
/// Returns encoded Bytes for controls that should also be forwarded upstream.
pub fn handle_agent_frame(
    session_mgr: &SharedSessionManager,
    session_id: &str,
    ctrl: Control,
) -> Option<Bytes> {
    match ctrl {
        Control::SessionUpdate { ref session, .. } => {
            session_mgr.update_session(session);
            ctrl.encode().ok().map(Bytes::from)
        }
        Control::SessionClose { .. } => {
            log::info!("Session {} closed by agent", session_id);
            session_mgr.force_unregister_session(session_id);
            ctrl.encode().ok().map(Bytes::from)
        }
        Control::SessionDetach { client_id, .. } => {
            log::debug!(
                "Agent acknowledged detach of client {} from session {}",
                client_id,
                session_id
            );
            None
        }
        Control::ClientResize { .. } => {
            let (encoded, _) = forward_and_cleanup(session_mgr, session_id, ctrl);
            encoded
        }
        Control::DesktopNotification { .. } => {
            let (encoded, ok) = forward_and_cleanup(session_mgr, session_id, ctrl);
            if !ok {
                log::debug!(
                    "No client attached, dropping DesktopNotification for {}",
                    session_id
                );
            }
            encoded
        }
        Control::Pong => {
            log::trace!("Received Pong from agent for session {}", session_id);
            None
        }
        _ => {
            log::debug!(
                "Unhandled control type from agent for session {}",
                session_id
            );
            None
        }
    }
}

/// Handle a control message received from a client.
/// Forwards ClientResize to the agent.
/// Observe-mode clients cannot send resize.
pub fn handle_client_frame(session_mgr: &SharedSessionManager, client_id: &str, ctrl: Control) {
    match ctrl {
        Control::ClientResize {
            session_id,
            client_id: cid,
            cols,
            rows,
        } => {
            let sid = session_id.clone();
            if session_mgr.is_observe_client(&sid, client_id) {
                log::debug!("Ignoring ClientResize from observe client {}", client_id);
                return;
            }
            log::info!(
                "ClientResize from {} for session {}: {}x{}",
                cid,
                sid,
                cols,
                rows
            );
            session_mgr.update_client_size(&sid, &cid, cols, rows);
            let (_broadcast, size_changed) = session_mgr.set_active_client(&sid, &cid, cols, rows);
            if size_changed {
                if !session_mgr.forward_to_agent(
                    &sid,
                    Control::ClientResize {
                        session_id,
                        client_id: cid,
                        cols,
                        rows,
                    },
                ) {
                    log::warn!(
                        "Failed to forward ClientResize from client {} to session {}",
                        client_id,
                        sid
                    );
                }
            } else {
                log::info!(
                    "ClientResize from {} for session {}: size unchanged (skipping forward)",
                    cid,
                    sid
                );
            }
        }
        Control::Ping => {
            log::trace!("Received Ping from client {}", client_id);
        }
        Control::ClientSetTitle {
            session_id,
            title,
            client_id: cid,
        } => {
            log::debug!(
                "ClientSetTitle from {} for session {}: {:?}",
                client_id,
                session_id,
                title
            );
            session_mgr.set_session_title(&session_id, &title);
            session_mgr.forward_to_agent(
                &session_id.clone(),
                Control::ClientSetTitle {
                    session_id,
                    client_id: cid,
                    title,
                },
            );
        }
        Control::ClientRefresh {
            session_id,
            client_id: cid,
        } => {
            log::debug!(
                "ClientRefresh from {} for session {}",
                client_id,
                session_id
            );
            session_mgr.forward_to_agent(
                &session_id.clone(),
                Control::ClientRefresh {
                    session_id,
                    client_id: cid,
                },
            );
        }
        _ => {
            log::debug!("Unhandled control type from client {}", client_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_mgr::SessionManager;
    use saw_core::protocol::control::{AttachMode, SessionInfo};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn make_session_info(id: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            shell: "/bin/zsh".to_string(),
            started_at: 1713400000,
            cols: 80,
            rows: 24,
            cwd: "/home/user".to_string(),
            first_command: None,
            terminal_program: None,
            last_activity_at: 1713400000,
            hostname: "myhost".to_string(),
            username: "user".to_string(),
            title: String::new(),
        }
    }

    #[tokio::test]
    async fn test_handle_agent_frame_session_update() {
        let mgr = Arc::new(SessionManager::new());
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);
        let (client_tx, _client_rx) = mpsc::channel::<Bytes>(32);

        {
            let _ = mgr.register_session(make_session_info("s1"), agent_tx);
            mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);
        }

        let ctrl = Control::SessionUpdate {
            session: make_session_info("s1"),
        };
        let result = handle_agent_frame(&mgr, "s1", ctrl);
        assert!(
            result.is_some(),
            "handle_agent_frame should return encoded bytes for SessionUpdate"
        );
        assert!(!result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_handle_agent_frame_no_client() {
        let mgr = Arc::new(SessionManager::new());
        let (agent_tx, _agent_rx) = mpsc::channel::<Control>(32);

        {
            let _ = mgr.register_session(make_session_info("s1"), agent_tx);
        }

        let ctrl = Control::SessionClose {
            session_id: "s1".into(),
        };
        handle_agent_frame(&mgr, "s1", ctrl);
    }

    #[tokio::test]
    async fn test_handle_client_frame_client_resize() {
        let mgr = Arc::new(SessionManager::new());
        let (agent_tx, mut agent_rx) = mpsc::channel::<Control>(32);
        let (client_tx, _client_rx) = mpsc::channel::<Bytes>(32);

        {
            let _ = mgr.register_session(make_session_info("s1"), agent_tx);
            mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);
        }

        while let Ok(Control::ClientActive { .. }) = agent_rx.try_recv() {}

        let ctrl = Control::ClientResize {
            session_id: "s1".into(),
            client_id: "c1".into(),
            cols: 120,
            rows: 40,
        };
        handle_client_frame(&mgr, "c1", ctrl);

        let mut found_resize = false;
        while let Ok(c) = agent_rx.try_recv() {
            if let Control::ClientResize { cols, rows, .. } = c {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
                found_resize = true;
            }
        }
        assert!(found_resize, "Expected ClientResize control");
    }

    #[tokio::test]
    async fn test_client_resize_updates_session_size_and_active_client() {
        let mgr = Arc::new(SessionManager::new());
        let (agent_tx, mut agent_rx) = mpsc::channel::<Control>(32);
        let (client_tx, mut client_rx) = mpsc::channel::<Bytes>(32);

        {
            let _ = mgr.register_session(make_session_info("s1"), agent_tx);
            mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client_tx);
        }

        while let Ok(Control::ClientActive { .. }) = agent_rx.try_recv() {}
        while client_rx.try_recv().is_ok() {}

        let ctrl = Control::ClientResize {
            session_id: "s1".into(),
            client_id: "c1".into(),
            cols: 200,
            rows: 50,
        };
        handle_client_frame(&mgr, "c1", ctrl);

        let sessions = mgr.list_sessions();
        assert_eq!(sessions[0].cols, 200);
        assert_eq!(sessions[0].rows, 50);

        let mut found_resize = false;
        while let Ok(c) = agent_rx.try_recv() {
            if let Control::ClientResize { cols, rows, .. } = c {
                assert_eq!(cols, 200);
                assert_eq!(rows, 50);
                found_resize = true;
            }
        }
        assert!(found_resize, "Expected ClientResize forwarded to agent");
    }

    #[tokio::test]
    async fn test_client_resize_no_rebroadcast_same_size() {
        let mgr = Arc::new(SessionManager::new());
        let (agent_tx, mut agent_rx) = mpsc::channel::<Control>(32);
        let (client1_tx, _client1_rx) = mpsc::channel::<Bytes>(32);
        let (client2_tx, _client2_rx) = mpsc::channel::<Bytes>(32);

        {
            let _ = mgr.register_session(make_session_info("s1"), agent_tx);
            mgr.attach_client("s1", "c1".into(), AttachMode::Interact, client1_tx);
            mgr.attach_client("s1", "c2".into(), AttachMode::Interact, client2_tx);
        }

        while agent_rx.try_recv().is_ok() {}

        mgr.set_active_client("s1", "c1", 80, 24);
        while agent_rx.try_recv().is_ok() {}

        let ctrl = Control::ClientResize {
            session_id: "s1".into(),
            client_id: "c2".into(),
            cols: 120,
            rows: 40,
        };
        handle_client_frame(&mgr, "c2", ctrl);

        let mut found_active = false;
        while let Ok(c) = agent_rx.try_recv() {
            if let Control::ClientActive {
                client_id,
                cols,
                rows,
                ..
            } = c
            {
                assert_eq!(client_id, "c2");
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
                found_active = true;
            }
        }
        assert!(
            found_active,
            "Expected ClientActive when active client changes"
        );

        let ctrl2 = Control::ClientResize {
            session_id: "s1".into(),
            client_id: "c2".into(),
            cols: 120,
            rows: 40,
        };
        handle_client_frame(&mgr, "c2", ctrl2);

        let mut second_active = false;
        while let Ok(c) = agent_rx.try_recv() {
            if let Control::ClientActive { .. } = c {
                second_active = true;
            }
        }
        assert!(
            !second_active,
            "Should NOT broadcast ClientActive for same size from same active client"
        );
    }
}
