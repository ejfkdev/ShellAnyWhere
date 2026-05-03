//! Unified UDP multiplexer for KCP and WebRTC on a single port.
//!
//! Reads all packets from one shared `UdpSocket`, classifies them by protocol
//! fingerprint, and routes to the appropriate handler (WebRTC or KCP).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use saw_core::protocol::kcp_transport::Transport;
use tokio::sync::mpsc;

// ── Packet classification ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketClass {
    WebRtc,
    Kcp,
    Unknown,
}

/// Classify a UDP packet as WebRTC (STUN/DTLS/ChannelData) or KCP.
fn classify_packet(packet: &[u8]) -> PacketClass {
    if packet.len() < 4 {
        return PacketClass::Unknown;
    }

    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);

    // 1. STUN Binding Request/Response: first 2 bits = 00, magic cookie at bytes 4-7
    if msg_type & 0xC000 == 0 && packet.len() >= 8 {
        let cookie = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        if cookie == 0x2112A442 {
            return PacketClass::WebRtc;
        }
    }

    // 2. TURN ChannelData: first 2 bits = 01
    if msg_type & 0xC000 == 0x4000 {
        return PacketClass::WebRtc;
    }

    // 3. DTLS: content type in {20-23}, version {0xFEFD, 0xFEFF}
    if packet.len() >= 3 {
        let content_type = packet[0];
        if matches!(content_type, 20..=23) {
            let version = u16::from_be_bytes([packet[1], packet[2]]);
            if version == 0xFEFD || version == 0xFEFF {
                return PacketClass::WebRtc;
            }
        }
    }

    // 4. KCP: packet >= 24 bytes, byte[4] (cmd) in {81-84}, len field consistent
    if packet.len() >= 24 {
        let cmd = packet[4];
        if matches!(cmd, 81..=84) {
            let data_len =
                u32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]) as usize;
            if packet.len() >= 24 + data_len {
                return PacketClass::Kcp;
            }
        }
    }

    PacketClass::Unknown
}

// ── UdpMux ────────────────────────────────────────────────────────────────

/// Type alias for WebRTC peer packet channel.
type PktSender = mpsc::UnboundedSender<KcpPacket>;

/// KCP packet with source address.
type KcpPacket = (SocketAddr, Vec<u8>);

/// Unified UDP multiplexer: routes packets to WebRTC or KCP handlers.
pub struct UdpMux {
    socket: Arc<tokio::net::UdpSocket>,
    listen_addr: SocketAddr,
    // WebRTC routing: server ufrag → channel
    webrtc_routes: std::sync::Mutex<HashMap<String, PktSender>>,
    // WebRTC routing: peer source addr → server ufrag
    webrtc_peer_map: std::sync::Mutex<HashMap<SocketAddr, String>>,
    // KCP routing: single channel for all KCP packets (fed to MuxTransport)
    kcp_pkt_tx: mpsc::UnboundedSender<KcpPacket>,
    // Holder for the KCP receiver (taken once by create_kcp_transport)
    kcp_pkt_rx_holder: std::sync::Mutex<Option<mpsc::UnboundedReceiver<KcpPacket>>>,
}

impl UdpMux {
    /// Create a new UdpMux bound to the given address.
    pub fn new(socket: tokio::net::UdpSocket) -> Arc<Self> {
        let listen_addr = socket.local_addr().expect("UdpMux socket local_addr");
        let socket = Arc::new(socket);

        // Unbounded channel for KCP packets — KcpListener reads via MuxTransport
        let (kcp_pkt_tx, kcp_pkt_rx) = mpsc::unbounded_channel();

        let mux = Arc::new(Self {
            socket: socket.clone(),
            listen_addr,
            webrtc_routes: std::sync::Mutex::new(HashMap::new()),
            webrtc_peer_map: std::sync::Mutex::new(HashMap::new()),
            kcp_pkt_tx,
            kcp_pkt_rx_holder: std::sync::Mutex::new(Some(kcp_pkt_rx)),
        });

        // Spawn the unified packet receive loop
        let mux_clone = mux.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 65535];
            loop {
                match mux_clone.socket.recv_from(&mut buf).await {
                    Ok((n, source)) => {
                        mux_clone.dispatch_packet(source, &buf[..n]);
                    }
                    Err(e) => {
                        log::warn!("UdpMux: UDP recv error: {}", e);
                        break;
                    }
                }
            }
        });

        log::info!("UdpMux ready on UDP {}", listen_addr);
        mux
    }

    /// The local address the UDP socket is bound to.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Send a UDP packet through the shared socket.
    pub async fn send_to(&self, data: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        self.socket.send_to(data, dest).await?;
        Ok(())
    }

    /// Create a MuxTransport for KcpListener::with_transport().
    /// Can only be called once.
    pub fn create_kcp_transport(&self) -> MuxTransport {
        let rx = self
            .kcp_pkt_rx_holder
            .lock()
            .unwrap()
            .take()
            .expect("create_kcp_transport called more than once");
        MuxTransport {
            socket: self.socket.clone(),
            local_addr: self.listen_addr,
            pkt_rx: tokio::sync::Mutex::new(rx),
        }
    }

    // ── WebRTC methods (migrated from WebRtcMux) ────────────────────────

    /// Register a new WebRTC Rtc instance to receive packets for the given ufrag.
    pub fn register(&self, ufrag: &str) -> mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.webrtc_routes
            .lock()
            .unwrap()
            .insert(ufrag.to_string(), tx);
        log::debug!("UdpMux: registered WebRTC ufrag={}", ufrag);
        rx
    }

    /// Unregister a WebRTC peer (on disconnect).
    pub fn unregister(&self, ufrag: &str) {
        self.webrtc_routes.lock().unwrap().remove(ufrag);
        let mut pm = self.webrtc_peer_map.lock().unwrap();
        pm.retain(|_, v| v != ufrag);
        log::debug!("UdpMux: unregistered WebRTC ufrag={}", ufrag);
    }

    /// Map a peer's source address to its server ufrag (after ICE completes).
    pub fn register_peer(&self, peer_addr: SocketAddr, ufrag: &str) {
        self.webrtc_peer_map
            .lock()
            .unwrap()
            .insert(peer_addr, ufrag.to_string());
    }

    // ── Internal dispatch ───────────────────────────────────────────────

    fn dispatch_packet(&self, source: SocketAddr, packet: &[u8]) {
        match classify_packet(packet) {
            PacketClass::WebRtc => self.dispatch_webrtc(source, packet),
            PacketClass::Kcp => self.dispatch_kcp(source, packet),
            PacketClass::Unknown => {
                log::debug!(
                    "UdpMux: unknown packet {} bytes from {}",
                    packet.len(),
                    source
                );
            }
        }
    }

    fn dispatch_webrtc(&self, source: SocketAddr, packet: &[u8]) {
        let ufrag = extract_stun_ufrag(packet)
            .or_else(|| self.webrtc_peer_map.lock().unwrap().get(&source).cloned());

        match ufrag {
            Some(ufrag) => {
                if let Some(tx) = self.webrtc_routes.lock().unwrap().get(&ufrag) {
                    let _ = tx.send((source, packet.to_vec()));
                } else {
                    log::debug!("UdpMux: no WebRTC route for ufrag={}", ufrag);
                }
            }
            None => {
                log::debug!("UdpMux: cannot route WebRTC packet from {}", source);
            }
        }
    }

    fn dispatch_kcp(&self, source: SocketAddr, packet: &[u8]) {
        if self.kcp_pkt_tx.send((source, packet.to_vec())).is_err() {
            log::debug!(
                "UdpMux: KCP channel closed, dropping packet from {}",
                source
            );
        }
    }
}

// ── MuxTransport ──────────────────────────────────────────────────────────

/// Custom `Transport` implementation that feeds KCP packets from UdpMux
/// into `KcpListener::with_transport()`.
pub struct MuxTransport {
    socket: Arc<tokio::net::UdpSocket>,
    local_addr: SocketAddr,
    pkt_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<KcpPacket>>,
}

impl Transport for MuxTransport {
    type Addr = SocketAddr;

    async fn send_to(&self, buf: &[u8], target: &SocketAddr) -> std::io::Result<usize> {
        self.socket.send_to(buf, target).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        // Read from the channel fed by UdpMux's dispatch loop
        let (source, data) = self
            .pkt_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| std::io::Error::other("KCP channel closed"))?;

        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok((len, source))
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

// ── STUN ufrag extraction (migrated from webrtc.rs) ───────────────────────

/// Try to extract the server (remote) ufrag from a STUN Binding Request.
///
/// STUN Binding Request USERNAME attribute format: `remote_ufrag:local_ufrag`
/// where "remote" is the server's ufrag (the one we need for routing).
pub fn extract_stun_ufrag(packet: &[u8]) -> Option<String> {
    if packet.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
    if msg_type & 0xC000 != 0 {
        return None;
    }
    if u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) != 0x2112A442 {
        return None;
    }

    let msg_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < 20 + msg_len {
        return None;
    }

    let mut offset = 20;
    while offset + 4 <= 20 + msg_len {
        let attr_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let attr_len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let attr_start = offset + 4;
        let attr_end = attr_start + attr_len;
        if attr_end > packet.len() {
            break;
        }

        if attr_type == 0x0006 {
            if let Ok(username) = std::str::from_utf8(&packet[attr_start..attr_end]) {
                if let Some(remote) = username.split(':').next() {
                    return Some(remote.to_string());
                }
            }
        }

        let padded = (attr_len + 3) & !3;
        offset = attr_start + padded;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_stun_binding_request() {
        // Minimal STUN Binding Request with magic cookie
        let mut packet = vec![0u8; 24];
        // msg_type = 0x0001 (Binding Request, first 2 bits = 00)
        packet[0] = 0x00;
        packet[1] = 0x01;
        // magic cookie
        packet[4] = 0x21;
        packet[5] = 0x12;
        packet[6] = 0xA4;
        packet[7] = 0x42;
        assert_eq!(classify_packet(&packet), PacketClass::WebRtc);
    }

    #[test]
    fn test_classify_dtls() {
        // DTLS record: content type 22 (Handshake), version 0xFEFD
        let packet = [22, 0xFE, 0xFD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(classify_packet(&packet), PacketClass::WebRtc);
    }

    #[test]
    fn test_classify_kcp() {
        // KCP packet: 24-byte header with cmd=81 (IKCP_CMD_PUSH)
        let mut packet = vec![0u8; 48];
        // conv (bytes 0-3) = non-zero
        packet[0] = 0x01;
        // cmd (byte 4) = 81
        packet[4] = 81;
        // frg (byte 5) = 0
        // wnd (bytes 6-7) = 256
        packet[7] = 1;
        // ts, sn, una (bytes 8-19) = 0
        // len (bytes 20-23, LE) = 24 (payload length)
        packet[20] = 24;
        assert_eq!(classify_packet(&packet), PacketClass::Kcp);
    }

    #[test]
    fn test_classify_unknown() {
        let packet = [0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(classify_packet(&packet), PacketClass::Unknown);
    }

    #[test]
    fn test_classify_turn_channel_data() {
        // TURN ChannelData: first 2 bits = 01
        let mut packet = vec![0u8; 20];
        packet[0] = 0x40; // 01 in first 2 bits
        packet[1] = 0x00;
        assert_eq!(classify_packet(&packet), PacketClass::WebRtc);
    }
}
