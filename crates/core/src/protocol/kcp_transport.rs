use anyhow::Result;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

pub use kcp_tokio::{KcpConfig, KcpListener, KcpStream};

/// Stream IDs for multiplexing over a single KCP connection.
pub mod stream_id {
    pub const CONTROL: u8 = 0;
    pub const TERMINAL_IO: u8 = 1;
}

/// Frame header: [0xCA][0xFE][stream_id: u8][len: u16][payload]
/// Magic bytes prevent accidental misinterpretation of random UDP data.
const FRAME_MAGIC: [u8; 2] = [0xCA, 0xFE];
const FRAME_HEADER_LEN: usize = 5; // magic(2) + stream_id(1) + len(2)
const MAX_FRAME_PAYLOAD: usize = 65535;
/// Maximum KCP payload for a single frame (must fit in KCP MTU minus overhead).
const KCP_MTU: usize = 1400;

/// Encode a frame into an existing buffer: magic + stream_id + len + payload.
/// Returns the slice of the buffer that was written.
/// Decode a frame from a buffer. Returns (stream_id, payload, bytes_consumed).
fn decode_frame(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    if buf.len() < FRAME_HEADER_LEN {
        return None;
    }
    if buf[0] != FRAME_MAGIC[0] || buf[1] != FRAME_MAGIC[1] {
        // Not a valid frame — skip one byte and try again
        return None;
    }
    let stream_id = buf[2];
    let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < FRAME_HEADER_LEN + len {
        return None; // Incomplete frame
    }
    Some((
        stream_id,
        &buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len],
        FRAME_HEADER_LEN + len,
    ))
}

/// A virtual stream identified by stream_id within a multiplexed KCP connection.
/// Implements AsyncRead + AsyncWrite for compatibility with ContextStream.
pub struct KcpVirtualStream {
    stream_id: u8,
    write_tx: mpsc::Sender<Vec<u8>>,
    read_rx: mpsc::Receiver<Vec<u8>>,
    /// Leftover data from channel recv that didn't fit in caller's ReadBuf.
    /// Uses (offset, data) instead of Vec::drain to avoid O(n) copies.
    read_buf: Vec<u8>,
    read_buf_offset: usize,
    /// Shared notify: mux signals this after draining write channel.
    write_notify: Arc<tokio::sync::Notify>,
}

impl KcpVirtualStream {
    pub fn stream_id(&self) -> u8 {
        self.stream_id
    }

    /// Compact read_buf if the consumed prefix is too large.
    fn maybe_compact_read_buf(&mut self) {
        if self.read_buf_offset > 1024 && self.read_buf_offset > self.read_buf.len() / 2 {
            self.read_buf.copy_within(self.read_buf_offset.., 0);
            self.read_buf
                .truncate(self.read_buf.len() - self.read_buf_offset);
            self.read_buf_offset = 0;
        }
    }
}

impl AsyncWrite for KcpVirtualStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let chunk_len = buf.len().min(MAX_FRAME_PAYLOAD);
        // Build frame with single allocation: header + payload
        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + chunk_len);
        frame.extend_from_slice(&FRAME_MAGIC);
        frame.push(self.stream_id);
        frame.extend_from_slice(&(chunk_len as u16).to_be_bytes());
        frame.extend_from_slice(&buf[..chunk_len]);
        match self.write_tx.try_send(frame) {
            Ok(()) => Poll::Ready(Ok(chunk_len)),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Register for notification when mux drains the channel
                let notify = self.write_notify.clone();
                let waker = cx.waker().clone();
                tokio::spawn(async move {
                    notify.notified().await;
                    waker.wake();
                });
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for KcpVirtualStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = self.read_buf.len().saturating_sub(self.read_buf_offset);
        if remaining > 0 {
            let n = remaining.min(buf.remaining());
            buf.put_slice(&self.read_buf[self.read_buf_offset..self.read_buf_offset + n]);
            self.read_buf_offset += n;
            self.maybe_compact_read_buf();
            return Poll::Ready(Ok(()));
        }

        match self.read_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.read_buf.clear();
                    self.read_buf_offset = 0;
                    self.read_buf.extend_from_slice(&data[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Multiplexer for a single KCP connection.
/// Reads frames from the underlying KCP stream and routes them to virtual streams.
/// Writes from virtual streams are sent as frames on the KCP stream.
pub struct KcpMultiplex {
    /// Shared write channel: all virtual streams send frames here.
    write_rx: mpsc::Receiver<Vec<u8>>,
    /// Write channel producer (cloned for each virtual stream).
    write_tx: mpsc::Sender<Vec<u8>>,
    /// Map of stream_id -> sender for delivering received data.
    streams: HashMap<u8, mpsc::Sender<Vec<u8>>>,
    /// The underlying KCP stream.
    stream: KcpStream,
    /// Read buffer for partial frame parsing. Uses cursor to avoid drain.
    read_buf: Vec<u8>,
    read_buf_offset: usize,
    /// Notify virtual streams when write channel has been drained.
    write_notify: Arc<tokio::sync::Notify>,
}

/// Maximum number of frames to batch in a single KCP write.
const MAX_BATCH_FRAMES: usize = 64;

impl KcpMultiplex {
    pub fn new(stream: KcpStream) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(512);
        Self {
            write_rx,
            write_tx,
            streams: HashMap::new(),
            stream,
            read_buf: Vec::with_capacity(16384),
            read_buf_offset: 0,
            write_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Open a virtual stream with the given stream_id.
    /// Returns the virtual stream (AsyncRead + AsyncWrite).
    pub fn open_stream(&mut self, stream_id: u8) -> KcpVirtualStream {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(2048);
        self.streams.insert(stream_id, tx);
        KcpVirtualStream {
            stream_id,
            write_tx: self.write_tx.clone(),
            read_rx: rx,
            read_buf: Vec::new(),
            read_buf_offset: 0,
            write_notify: self.write_notify.clone(),
        }
    }

    /// Run the multiplexer loop. Reads from KCP, demultiplexes to virtual streams,
    /// and writes frames from virtual streams to KCP.
    /// Returns when the connection is closed or an error occurs.
    pub async fn run(&mut self) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut read_buf = [0u8; 16384];
        let mut batch = Vec::with_capacity(MAX_BATCH_FRAMES);
        // Reusable buffer for merging batch frames into a single write
        let mut merge_buf: Vec<u8> = Vec::with_capacity(65536);

        loop {
            tokio::select! {
                // Read from KCP, demultiplex frames
                result = self.stream.read(&mut read_buf) => {
                    match result {
                        Ok(0) => {
                            log::debug!("KCP multiplex: connection closed");
                            return Ok(());
                        }
                        Ok(n) => {
                            // Compact read_buf if there's too much consumed prefix
                            if self.read_buf_offset > 4096 {
                                let remaining = self.read_buf.len() - self.read_buf_offset;
                                self.read_buf.copy_within(self.read_buf_offset.., 0);
                                self.read_buf.truncate(remaining);
                                self.read_buf_offset = 0;
                            }
                            self.read_buf.extend_from_slice(&read_buf[..n]);
                            self.process_read_buffer();
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
                // Write frames from virtual streams to KCP — merge into single write
                frame = self.write_rx.recv() => {
                    match frame {
                        Some(frame) => {
                            batch.push(frame);
                            // Drain all pending frames to batch them
                            while batch.len() < MAX_BATCH_FRAMES {
                                match self.write_rx.try_recv() {
                                    Ok(f) => batch.push(f),
                                    Err(_) => break,
                                }
                            }
                            // Merge all frames into a single buffer, one KCP write
                            merge_buf.clear();
                            for f in &batch {
                                merge_buf.extend_from_slice(f);
                            }
                            if !merge_buf.is_empty() && let Err(e) = self.stream.write_all(&merge_buf).await {
                                return Err(e.into());
                            }
                            batch.clear();
                            // Notify virtual streams that write channel has space
                            self.write_notify.notify_waiters();
                            // Retry deferred frames: outgoing activity means app is processing data,
                            // so stream channels may have freed up space.
                            self.process_read_buffer();
                        }
                        None => {
                            log::debug!("KCP multiplex: all virtual streams closed");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Process the read buffer, extracting and routing complete frames.
    /// When a stream's channel is full, stops processing to apply backpressure.
    /// The remaining data stays in read_buf and will be retried on next call.
    fn process_read_buffer(&mut self) {
        while self.read_buf_offset < self.read_buf.len() {
            match decode_frame(&self.read_buf[self.read_buf_offset..]) {
                Some((stream_id, payload, consumed)) => {
                    log::debug!(
                        "KCP mux: routing {} bytes to stream_id={}",
                        payload.len(),
                        stream_id
                    );
                    if let Some(tx) = self.streams.get(&stream_id) {
                        match tx.try_send(payload.to_vec()) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Backpressure: stop processing, retry on next read
                                log::debug!(
                                    "KCP mux: stream_id={} channel full, deferring {} bytes",
                                    stream_id,
                                    payload.len()
                                );
                                break;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                log::trace!("KCP mux: stream_id={} channel closed", stream_id);
                            }
                        }
                    } else {
                        log::trace!(
                            "KCP multiplex: no handler for stream_id={}, dropping {} bytes",
                            stream_id,
                            payload.len()
                        );
                    }
                    self.read_buf_offset += consumed;
                }
                None => break,
            }
        }
        // Compact if needed
        if self.read_buf_offset > 4096 && self.read_buf_offset >= self.read_buf.len() / 2 {
            let remaining = self.read_buf.len() - self.read_buf_offset;
            if remaining == 0 {
                self.read_buf.clear();
            } else {
                self.read_buf.copy_within(self.read_buf_offset.., 0);
                self.read_buf.truncate(remaining);
            }
            self.read_buf_offset = 0;
        }
    }
}

/// Create a KCP config optimized for ultra-low-latency localhost/LAN use.
/// 1ms update interval, no congestion control, fast resend, large windows.
pub fn low_latency_kcp_config() -> KcpConfig {
    use kcp_core::config::NodeDelayConfig;
    KcpConfig::new()
        .nodelay_config(NodeDelayConfig::custom(true, 1, 1, true)) // nodelay, 1ms interval, fast resend, no congestion
        .mtu(KCP_MTU as u32)
        .window_size(256, 256)
        .stream_mode(true)
        .socket_buffer_size(2 * 1024 * 1024) // 2MB socket buffer
        .connect_timeout(std::time::Duration::from_secs(3))
        .keep_alive(Some(std::time::Duration::from_secs(10)))
}

/// Connect to a KCP server.
/// Returns a KcpMultiplex that can be used to open virtual streams.
pub async fn connect_kcp(addr: SocketAddr) -> Result<KcpMultiplex> {
    let config = low_latency_kcp_config();
    let stream = KcpStream::connect(addr, config).await?;
    log::info!("KCP connected to {}", addr);
    Ok(KcpMultiplex::new(stream))
}

/// Create a KCP listener bound to the given address.
pub async fn listen_kcp(addr: SocketAddr) -> Result<KcpListener> {
    let config = low_latency_kcp_config();
    let listener = KcpListener::bind(addr, config).await?;
    log::info!("KCP listener bound on {}", addr);
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode() {
        let payload = b"hello world";
        let mut buf = Vec::new();
        buf.extend_from_slice(&FRAME_MAGIC);
        buf.push(stream_id::CONTROL);
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(payload);
        assert_eq!(&buf[0..2], &FRAME_MAGIC);
        assert_eq!(buf[2], stream_id::CONTROL);
        let (sid, decoded, consumed) = decode_frame(&buf).unwrap();
        assert_eq!(sid, stream_id::CONTROL);
        assert_eq!(decoded, payload);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn test_frame_decode_incomplete() {
        let buf = [0xCA, 0xFE, 0x00]; // Missing length bytes
        assert!(decode_frame(&buf).is_none());
    }

    fn encode_test_frame(buf: &mut Vec<u8>, stream_id: u8, payload: &[u8]) {
        buf.extend_from_slice(&FRAME_MAGIC);
        buf.push(stream_id);
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(payload);
    }

    #[test]
    fn test_multiple_frames() {
        let mut buf = Vec::new();
        encode_test_frame(&mut buf, 0, b"first");
        encode_test_frame(&mut buf, 1, b"second");
        let (sid1, p1, c1) = decode_frame(&buf).unwrap();
        assert_eq!(sid1, 0);
        assert_eq!(p1, b"first");
        let (sid2, p2, _) = decode_frame(&buf[c1..]).unwrap();
        assert_eq!(sid2, 1);
        assert_eq!(p2, b"second");
    }
}
