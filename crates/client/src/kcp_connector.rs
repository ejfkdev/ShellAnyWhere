use anyhow::Result;
use saw_core::protocol::control::{AttachMode, Control, SessionInfo};
use saw_core::protocol::kcp_transport::{self, stream_id};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Result of listing sessions
pub struct SessionListResult {
    pub sessions: Vec<SessionInfo>,
}

/// Result of attaching to a session via KCP.
pub struct KcpAttachedSession {
    pub control_stream: kcp_transport::KcpVirtualStream,
    pub term_stream: kcp_transport::KcpVirtualStream,
    pub client_id: String,
}

/// Client connection to a server via KCP.
pub struct KcpClientConnector {
    control_stream: kcp_transport::KcpVirtualStream,
    term_stream: kcp_transport::KcpVirtualStream,
}

impl KcpClientConnector {
    /// Connect to a KCP server and authenticate as client.
    pub async fn connect(addr: &str, token: String) -> Result<Self> {
        let kcp_addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid server address: {}", addr))?;
        log::info!("Connecting via KCP to {}", kcp_addr);

        let mut mux = kcp_transport::connect_kcp(kcp_addr).await?;

        // Open all streams BEFORE spawning mux run loop
        let mut control_stream = mux.open_stream(stream_id::CONTROL);
        let term_stream = mux.open_stream(stream_id::TERMINAL_IO);

        // Spawn the mux run loop — it takes ownership of mux
        tokio::spawn(async move {
            if let Err(e) = mux.run().await {
                log::debug!("KCP mux run ended: {}", e);
            }
        });

        // Authenticate on the control stream
        let token_bytes = token.as_bytes();
        let mut auth_msg = vec![0x02]; // Client role
        auth_msg.extend_from_slice(&(token_bytes.len() as u16).to_be_bytes());
        auth_msg.extend_from_slice(token_bytes);
        control_stream.write_all(&auth_msg).await?;

        let mut resp = [0u8; 1];
        control_stream.read_exact(&mut resp).await?;
        if resp[0] != 0x00 {
            anyhow::bail!("KCP authentication failed");
        }

        log::info!("KCP connected and authenticated");

        Ok(Self {
            control_stream,
            term_stream,
        })
    }

    /// List available sessions
    pub async fn list_sessions(&mut self) -> Result<SessionListResult> {
        let data = encode_control(&Control::SessionList { sessions: vec![] })?;
        self.control_stream.write_all(&data).await?;

        let ctrl = recv_control(&mut self.control_stream).await?;

        let sessions = match ctrl {
            Control::SessionList { sessions: s } => s,
            _ => anyhow::bail!("Unexpected session list response"),
        };

        Ok(SessionListResult { sessions })
    }

    /// Open a session list stream that receives push updates.
    /// Returns the initial list. The connector retains the control stream
    /// so subsequent recv_control calls will yield pushed SessionList updates.
    pub async fn list_sessions_stream(&mut self) -> Result<Vec<SessionInfo>> {
        let data = encode_control(&Control::SessionList { sessions: vec![] })?;
        self.control_stream.write_all(&data).await?;

        let ctrl = recv_control(&mut self.control_stream).await?;

        let sessions = match ctrl {
            Control::SessionList { sessions: s } => s,
            _ => anyhow::bail!("Unexpected session list response"),
        };

        Ok(sessions)
    }

    /// Receive a pushed control message from the server (e.g. SessionList update).
    pub async fn recv_push(&mut self) -> Result<Control> {
        recv_control(&mut self.control_stream).await
    }

    /// Attach to a session. Consumes the connector, returns the streams.
    pub async fn attach(
        mut self,
        session_id: &str,
        mode: AttachMode,
        previous_client_id: Option<String>,
    ) -> Result<KcpAttachedSession> {
        let attach = Control::SessionAttach {
            session_id: session_id.to_string(),
            mode,
            previous_client_id,
        };
        let data = encode_control(&attach)?;
        log::info!("KCP attach: sending SessionAttach, {} bytes", data.len());
        self.control_stream.write_all(&data).await?;
        log::info!("KCP attach: SessionAttach sent, waiting for response");

        // Read response
        let attach_timeout = std::time::Duration::from_secs(5);
        let response = tokio::time::timeout(attach_timeout, recv_control(&mut self.control_stream))
            .await
            .map_err(|_| anyhow::anyhow!("Attach response timeout"))??;

        let client_id = match response {
            Control::AttachAck { client_id, .. } => client_id,
            Control::AttachReject { reason, .. } => {
                anyhow::bail!("Attach rejected: {}", reason);
            }
            _ => anyhow::bail!("Expected AttachAck or AttachReject from server"),
        };

        Ok(KcpAttachedSession {
            control_stream: self.control_stream,
            term_stream: self.term_stream,
            client_id,
        })
    }
}

/// Encode a Control message with length prefix: [4-byte BE len][bincode data]
fn encode_control(ctrl: &Control) -> anyhow::Result<Vec<u8>> {
    let data = bincode::serde::encode_to_vec(ctrl, bincode::config::standard())?;
    let len = data.len() as u32;
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&data);
    Ok(out)
}

/// Read a length-prefixed Control message from the stream.
async fn recv_control(stream: &mut kcp_transport::KcpVirtualStream) -> anyhow::Result<Control> {
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
    // Read payload
    let mut buf = vec![0u8; len];
    let mut offset = 0;
    while offset < len {
        match stream.read(&mut buf[offset..]).await {
            Ok(0) => anyhow::bail!("control stream closed mid-message"),
            Ok(n) => offset += n,
            Err(e) => return Err(e.into()),
        }
    }
    let (ctrl, _): (Control, _) =
        bincode::serde::decode_from_slice(&buf, bincode::config::standard())?;
    Ok(ctrl)
}
