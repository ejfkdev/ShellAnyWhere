//! Context-aware bidirectional streams with two-way context exchange.
//!
//! A stream abstraction where both sides exchange a fixed context
//! (serialized by core with bincode) upon establishment, followed by
//! subsequent binary messages with automatic compression and framing.
//!
//! # Wire format
//!
//! ```text
//! Context exchange (first message each direction):
//!   [4-byte BE total_len]
//!   [2-byte BE context_len]
//!   [context_len bytes: bincode-serialized StreamContext]
//!
//! Subsequent messages (Chunked mode):
//!   [4-byte BE total_len][1-byte flags][payload]
//!   flags bit 0 = lz4 compressed
//!
//! Subsequent messages (Raw mode, no compression):
//!   Raw byte stream — no framing
//!
//! Subsequent messages (Raw mode, with compression):
//!   [4-byte BE total_len][1-byte flags][payload]
//!   (batched every 20ms)
//! ```
//!
//! # Two-way exchange
//!
//! 1. Opener opens stream and sends its `StreamContext`.
//! 2. Acceptor reads opener's context, sends back its `StreamContext`.
//! 3. Both sides merge: local `send_*` + peer `send_*` → effective settings.
//!
//! # Auto I/O
//!
//! After exchange, `send_data` / `recv_data` automatically handle:
//! - **Chunked**: length-prefixed messages, optional per-message compression.
//! - **Raw + compressed**: buffer data, compress and send every 20ms.
//! - **Raw + not compressed**: write immediately, no framing.

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use anyhow::Result;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Deserialize, Serialize};

use crate::protocol::control::Control;

#[cfg(feature = "compress")]
use crate::util::compress;

/// Maximum total context message size (16 MB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Maximum stream_type length (256 bytes).
const MAX_TYPE_LEN: usize = 256;

/// Batch interval for Raw streams with compression (20ms).
const RAW_COMPRESS_BATCH_INTERVAL: Duration = Duration::from_millis(20);

/// Maximum raw bytes per flush chunk (8 MB).
/// Must be small enough that lz4 output fits within MAX_MESSAGE_SIZE (16 MB).
/// lz4 worst-case expansion is ~0.4%, so 8 MB raw → ~8.03 MB compressed, well under 16 MB.
const MAX_RAW_FLUSH_CHUNK: usize = 8 * 1024 * 1024;

/// Early flush threshold for send_buffer (4 MB).
/// When the buffer exceeds this size, flush immediately even if the 20ms
/// timer hasn't elapsed. This prevents unbounded buffer growth when PTY
/// output exceeds the flush rate.
const SEND_BUFFER_EARLY_FLUSH: usize = 1024 * 1024;

/// Flag: payload is lz4 compressed.
const FLAG_COMPRESSED: u8 = 0x01;

// ── StreamRole ────────────────────────────────────────────────────────────

/// Role of the stream opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamRole {
    /// Agent (shell) side — creates PTY sessions
    Agent,
    /// Client side — attaches to sessions
    Client,
    /// Server-initiated stream (e.g., push notifications)
    Server,
}

impl StreamRole {
    /// Priority for ID negotiation when both sides have non-zero IDs.
    /// Agent > Client > Server.
    fn priority(self) -> u8 {
        match self {
            Self::Agent => 2,
            Self::Client => 1,
            Self::Server => 0,
        }
    }
}

/// How subsequent data is framed on the stream, per direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamDataType {
    /// Raw byte stream — no length prefix per message.
    Raw,
    /// Chunked — each message is length-prefixed.
    Chunked,
}

// ── StreamContext ──────────────────────────────────────────────────────────

/// Fixed context exchanged when a stream is established.
///
/// Both sides send a `StreamContext` during the initial handshake.
/// After the exchange, each side merges the contexts to determine
/// effective send/receive settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamContext {
    /// Stream instance ID (unique per stream).
    /// If set to 0, the other side's ID is used instead.
    id: u64,

    /// Stream type identifier (e.g., "auth", "terminal_io", "session").
    stream_type: String,

    /// Role of the stream opener.
    role: StreamRole,

    /// Whether this side's outgoing data is compressed.
    send_compressed: bool,

    /// Whether this side can receive compressed data from the peer.
    recv_compressed: bool,

    /// Data format this side sends.
    send_data_type: StreamDataType,

    /// Data format this side expects to receive.
    recv_data_type: StreamDataType,
}

impl StreamContext {
    /// Create a new context with the minimum required fields.
    /// Defaults: no compression, both directions Chunked.
    pub fn new(id: u64, stream_type: impl Into<String>, role: StreamRole) -> Self {
        Self {
            id,
            stream_type: stream_type.into(),
            role,
            send_compressed: false,
            recv_compressed: false,
            send_data_type: StreamDataType::Chunked,
            recv_data_type: StreamDataType::Chunked,
        }
    }

    /// Set whether this side's outgoing data is compressed.
    pub fn with_send_compressed(mut self, yes: bool) -> Self {
        self.send_compressed = yes;
        self
    }

    /// Set whether this side can receive compressed data from the peer.
    pub fn with_recv_compressed(mut self, yes: bool) -> Self {
        self.recv_compressed = yes;
        self
    }

    /// Set compression for both directions.
    pub fn with_compress_both(mut self, yes: bool) -> Self {
        self.send_compressed = yes;
        self.recv_compressed = yes;
        self
    }

    /// Set the data type for this side's outgoing data.
    pub fn with_send_data_type(mut self, data_type: StreamDataType) -> Self {
        self.send_data_type = data_type;
        self
    }

    /// Set the data type this side expects to receive.
    pub fn with_recv_data_type(mut self, data_type: StreamDataType) -> Self {
        self.recv_data_type = data_type;
        self
    }

    /// Set both directions to the same data type.
    pub fn with_data_type_both(mut self, data_type: StreamDataType) -> Self {
        self.send_data_type = data_type;
        self.recv_data_type = data_type;
        self
    }

    // ── Getters ───────────────────────────────────────────────────────────

    /// Stream instance ID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Stream type identifier.
    pub fn stream_type(&self) -> &str {
        &self.stream_type
    }

    /// Role of the stream opener.
    pub fn role(&self) -> StreamRole {
        self.role
    }

    /// Whether this side's outgoing data is compressed.
    pub fn send_compressed(&self) -> bool {
        self.send_compressed
    }

    /// Whether this side can receive compressed data.
    pub fn recv_compressed(&self) -> bool {
        self.recv_compressed
    }

    /// Data format this side sends.
    pub fn send_data_type(&self) -> StreamDataType {
        self.send_data_type
    }

    /// Data format this side expects to receive.
    pub fn recv_data_type(&self) -> StreamDataType {
        self.recv_data_type
    }

    /// Validate context fields before sending.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.stream_type.is_empty(), "stream_type cannot be empty");
        anyhow::ensure!(
            self.stream_type.len() <= MAX_TYPE_LEN,
            "stream_type too long: {} bytes (max {})",
            self.stream_type.len(),
            MAX_TYPE_LEN
        );
        Ok(())
    }
}

// ── LocalConfig ────────────────────────────────────────────────────────────

/// Local configuration for the acceptor side.
///
/// After receiving the opener's context, the acceptor sends back
/// its own settings. The effective send settings are overridden
/// with these local values.
pub struct LocalConfig {
    /// Whether this side compresses outgoing data.
    pub send_compressed: bool,
    /// Whether this side can receive compressed data.
    pub recv_compressed: bool,
    /// Data format this side sends.
    pub send_data_type: StreamDataType,
    /// Data format this side expects to receive.
    pub recv_data_type: StreamDataType,
    /// Optional ID (0 = use opener's ID).
    pub id: u64,
    /// This side's actual role (for ID priority negotiation).
    pub role: StreamRole,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            send_compressed: false,
            recv_compressed: false,
            send_data_type: StreamDataType::Chunked,
            recv_data_type: StreamDataType::Chunked,
            id: 0,
            role: StreamRole::Server,
        }
    }
}

impl LocalConfig {
    /// Create a new LocalConfig with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether this side compresses outgoing data.
    pub fn with_send_compressed(mut self, yes: bool) -> Self {
        self.send_compressed = yes;
        self
    }

    /// Set whether this side can receive compressed data.
    pub fn with_recv_compressed(mut self, yes: bool) -> Self {
        self.recv_compressed = yes;
        self
    }

    /// Set compression for both directions.
    pub fn with_compress_both(mut self, yes: bool) -> Self {
        self.send_compressed = yes;
        self.recv_compressed = yes;
        self
    }

    /// Set the data type for outgoing data.
    pub fn with_send_data_type(mut self, data_type: StreamDataType) -> Self {
        self.send_data_type = data_type;
        self
    }

    /// Set the data type expected for incoming data.
    pub fn with_recv_data_type(mut self, data_type: StreamDataType) -> Self {
        self.recv_data_type = data_type;
        self
    }

    /// Set both directions to the same data type.
    pub fn with_data_type_both(mut self, data_type: StreamDataType) -> Self {
        self.send_data_type = data_type;
        self.recv_data_type = data_type;
        self
    }

    /// Set the ID (0 = use opener's ID).
    pub fn with_id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }

    /// Set this side's actual role (for ID priority negotiation).
    pub fn with_role(mut self, role: StreamRole) -> Self {
        self.role = role;
        self
    }

    /// Convert to a StreamContext for the acceptor's response.
    fn to_acceptor_context(&self, opener_context: &StreamContext) -> StreamContext {
        StreamContext {
            id: self.id,
            stream_type: opener_context.stream_type.clone(),
            role: self.role,
            send_compressed: self.send_compressed,
            recv_compressed: self.recv_compressed,
            send_data_type: self.send_data_type,
            recv_data_type: self.recv_data_type,
        }
    }
}

// ── ContextStream ─────────────────────────────────────────────────────────

/// A bidirectional stream with two-way context exchange.
///
/// Both sides exchange `StreamContext` during the initial handshake.
/// After establishment, `send_data` and `recv_data` automatically
/// handle compression and framing based on the negotiated settings.
///
/// Generic over the underlying stream type `S`, which must implement
/// both `AsyncRead` and `AsyncWrite`. This allows use with yamux streams,
/// WebSocket adapters, or any other bidirectional transport.
pub struct ContextStream<S> {
    /// The effective context after merging both sides.
    context: StreamContext,
    /// The peer's original context as received.
    #[allow(dead_code)]
    peer_context: StreamContext,
    /// Underlying bidirectional stream.
    stream: S,
    /// Reusable buffer for reading framed messages.
    read_buf: Vec<u8>,
    /// Reusable buffer for Raw mode reads (avoids per-call 64KB allocation).
    raw_read_buf: Vec<u8>,
    /// Send buffer for Raw + compressed mode.
    send_buffer: Vec<u8>,
    /// Next time to flush the send buffer.
    next_send_flush: tokio::time::Instant,
}

impl<S: AsyncRead + AsyncWrite + Unpin> ContextStream<S> {
    /// Open a context stream on an already-obtained bidirectional stream (opener side).
    ///
    /// 1. Sends opener's context.
    /// 2. Receives acceptor's context.
    /// 3. Merges contexts.
    pub async fn open_on(stream: S, context: &StreamContext) -> Result<Self> {
        context.validate()?;
        let mut stream = stream;

        // Step 1: Send our context
        send_context(&mut stream, context).await?;

        // Step 2: Receive acceptor's context
        let acceptor_ctx = recv_context(&mut stream).await?;

        // Step 3: Merge contexts (opener's role identifies the stream)
        let merged = merge_context(context, &acceptor_ctx, context.role);

        Ok(Self {
            context: merged,
            peer_context: acceptor_ctx,
            stream,
            read_buf: Vec::new(),
            raw_read_buf: vec![0u8; 65536],
            send_buffer: Vec::new(),
            next_send_flush: tokio::time::Instant::now() + RAW_COMPRESS_BATCH_INTERVAL,
        })
    }

    /// Create from an already-obtained stream (acceptor side).
    ///
    /// 1. Reads opener's context.
    /// 2. Sends acceptor's context (from `LocalConfig`).
    /// 3. Merges contexts.
    pub async fn from_raw(stream: S, local_config: &LocalConfig) -> Result<Self> {
        let mut stream = stream;

        // Step 1: Receive opener's context
        let opener_ctx = recv_context(&mut stream).await?;

        // Step 2: Send our context back
        let acceptor_ctx = local_config.to_acceptor_context(&opener_ctx);
        acceptor_ctx.validate()?;
        send_context(&mut stream, &acceptor_ctx).await?;

        // Step 3: Merge contexts (opener's role identifies the stream)
        let merged = merge_context(&acceptor_ctx, &opener_ctx, opener_ctx.role);

        Ok(Self {
            context: merged,
            peer_context: opener_ctx,
            stream,
            read_buf: Vec::new(),
            raw_read_buf: vec![0u8; 65536],
            send_buffer: Vec::new(),
            next_send_flush: tokio::time::Instant::now() + RAW_COMPRESS_BATCH_INTERVAL,
        })
    }

    /// Get a reference to the effective (merged) context.
    pub fn context(&self) -> &StreamContext {
        &self.context
    }

    /// Consume the ContextStream and return the underlying stream and context.
    /// Useful for zero-copy relay: after context exchange, the raw stream
    /// can be piped directly with `tokio::io::copy`, bypassing per-chunk
    /// framing/dispatch overhead.
    pub fn into_parts(self) -> (S, StreamContext) {
        let this = std::mem::ManuallyDrop::new(self);
        let stream = unsafe { std::ptr::read(&this.stream) };
        let context = unsafe { std::ptr::read(&this.context) };
        (stream, context)
    }

    // ── Auto I/O ──────────────────────────────────────────────────────────

    /// Send data through the stream, auto-handling compression and framing.
    pub async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        match self.context.send_data_type {
            StreamDataType::Chunked => {
                send_framed(&mut self.stream, data, self.context.send_compressed).await?;
            }
            StreamDataType::Raw => {
                if self.context.send_compressed {
                    self.send_buffer.extend_from_slice(data);
                    self.maybe_flush_send_buffer().await?;
                } else {
                    self.stream.write_all(data).await?;
                    self.stream.flush().await?;
                }
            }
        }
        Ok(())
    }

    /// Flush any buffered send data immediately.
    /// Splits large buffers into chunks to ensure each framed message
    /// fits within MAX_MESSAGE_SIZE after compression.
    pub async fn flush_send(&mut self) -> Result<()> {
        if self.send_buffer.is_empty() {
            return Ok(());
        }
        // Flush in chunks to keep each framed message within MAX_MESSAGE_SIZE.
        // Each chunk is independently compressed and sent as a separate frame.
        // The receiver handles each frame as a separate recv_data() result,
        // which is correct for Raw mode — the caller already handles
        // receiving data in variable-size chunks.
        while !self.send_buffer.is_empty() {
            let end = std::cmp::min(self.send_buffer.len(), MAX_RAW_FLUSH_CHUNK);
            let chunk: Vec<u8> = self.send_buffer.drain(..end).collect();
            send_framed(&mut self.stream, &chunk, true).await?;
        }
        self.next_send_flush = tokio::time::Instant::now() + RAW_COMPRESS_BATCH_INTERVAL;
        Ok(())
    }

    /// Flush the send buffer if the 20ms timer has elapsed, or if the
    /// buffer exceeds the early flush threshold (prevents unbounded growth).
    async fn maybe_flush_send_buffer(&mut self) -> Result<()> {
        if self.send_buffer.is_empty() {
            return Ok(());
        }
        // Flush early when buffer is large to prevent unbounded growth.
        // PTY output can arrive faster than the 20ms flush interval,
        // causing the buffer to grow. Flushing early keeps each frame
        // small enough to fit within MAX_MESSAGE_SIZE after compression.
        if self.send_buffer.len() >= SEND_BUFFER_EARLY_FLUSH {
            self.flush_send().await?;
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= self.next_send_flush {
            self.flush_send().await?;
        }
        Ok(())
    }

    /// Receive data from the stream, auto-handling decompression and framing.
    ///
    /// Returns `None` if the stream is closed.
    pub async fn recv_data(&mut self) -> Result<Option<Vec<u8>>> {
        match self.context.recv_data_type {
            StreamDataType::Chunked => recv_framed(&mut self.stream, &mut self.read_buf).await,
            StreamDataType::Raw => {
                if self.context.recv_compressed {
                    recv_framed(&mut self.stream, &mut self.read_buf).await
                } else {
                    // Read raw bytes directly using reusable buffer
                    let buf = &mut self.raw_read_buf;
                    match self.stream.read(buf).await {
                        Ok(0) => Ok(None),
                        Ok(n) => Ok(Some(buf[..n].to_vec())),
                        Err(e) => Err(e.into()),
                    }
                }
            }
        }
    }

    // ── Raw I/O (bypass auto-handling) ────────────────────────────────────

    /// Send a control message over this stream.
    pub async fn send_control(&mut self, ctrl: &Control) -> Result<()> {
        let bytes = bincode::serde::encode_to_vec(ctrl, bincode::config::standard())?;
        self.send_data(&bytes).await
    }

    /// Receive a control message from this stream.
    /// Returns `None` if the stream is closed.
    pub async fn recv_control(&mut self) -> Result<Option<Control>> {
        match self.recv_data().await? {
            Some(bytes) => {
                let (ctrl, _): (Control, _) =
                    bincode::serde::decode_from_slice(&bytes, bincode::config::standard())?;
                Ok(Some(ctrl))
            }
            None => Ok(None),
        }
    }

    /// Receive raw data from the stream without decompressing.
    ///
    /// Returns `(payload, is_compressed)` where `payload` is the raw bytes
    /// from the frame (after the flags byte), and `is_compressed` indicates
    /// whether the payload is lz4 compressed (compress_prepend_size format).
    ///
    /// This is useful for pass-through relay where compressed data is
    /// forwarded directly to another consumer without decompressing.
    ///
    /// Only supported for Chunked mode. Returns an error for Raw mode.
    pub async fn recv_data_raw(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        match self.context.recv_data_type {
            StreamDataType::Chunked => recv_framed_raw(&mut self.stream, &mut self.read_buf).await,
            StreamDataType::Raw => {
                anyhow::bail!("recv_data_raw not applicable for Raw mode — use recv_data() instead")
            }
        }
    }

    /// Send pre-compressed data through the stream without additional compression.
    ///
    /// The `is_compressed` flag is set in the frame header, and the data is sent
    /// as-is (no recompression). This is the send-side counterpart of `recv_data_raw()`
    /// and is useful for pass-through relay where compressed data is forwarded
    /// directly without decompressing and recompressing.
    ///
    /// Only supported for Chunked mode. Returns an error for Raw mode.
    pub async fn send_data_raw(&mut self, data: &[u8], is_compressed: bool) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        match self.context.send_data_type {
            StreamDataType::Chunked => send_framed_raw(&mut self.stream, data, is_compressed).await,
            StreamDataType::Raw => {
                anyhow::bail!("send_data_raw not applicable for Raw mode — use send_data() instead")
            }
        }
    }

    /// Flush the underlying stream (bypasses ContextStream buffer logic).
    /// Useful after `send_data_raw()` to ensure the QUIC stream is flushed
    /// for low-latency forwarding.
    pub async fn flush_stream(&mut self) -> Result<()> {
        use futures::AsyncWriteExt;
        self.stream.flush().await?;
        Ok(())
    }
}

impl<S> Drop for ContextStream<S> {
    fn drop(&mut self) {
        if !self.send_buffer.is_empty() {
            log::warn!(
                "ContextStream dropped with {} bytes in send buffer — data lost. \
                 Call flush_send() before dropping.",
                self.send_buffer.len()
            );
        }
    }
}

// ── Context merge ──────────────────────────────────────────────────────────

/// Merge local and peer contexts into the effective context.
fn merge_context(
    local: &StreamContext,
    peer: &StreamContext,
    opener_role: StreamRole,
) -> StreamContext {
    let effective_id = match (local.id, peer.id) {
        (0, 0) => 0,
        (0, id) | (id, 0) => id,
        (_, _) => {
            if local.role.priority() >= peer.role.priority() {
                local.id
            } else {
                peer.id
            }
        }
    };

    StreamContext {
        id: effective_id,
        stream_type: local.stream_type.clone(),
        role: opener_role,
        send_compressed: local.send_compressed,
        recv_compressed: peer.send_compressed,
        send_data_type: local.send_data_type,
        recv_data_type: peer.send_data_type,
    }
}

// ── Context exchange (generic over AsyncRead/AsyncWrite) ───────────────────

/// Send context on a stream.
/// Wire: [4-byte BE total_len][2-byte BE context_len][context bytes]
async fn send_context<W: AsyncWrite + Unpin>(send: &mut W, context: &StreamContext) -> Result<()> {
    let context_bytes = bincode::serde::encode_to_vec(context, bincode::config::standard())?;
    let context_len = context_bytes.len() as u16;
    let total_len = 2 + context_bytes.len();
    anyhow::ensure!(
        total_len <= MAX_MESSAGE_SIZE,
        "context too large: {} bytes (max {})",
        total_len,
        MAX_MESSAGE_SIZE
    );

    let mut buf = Vec::with_capacity(4 + 2 + context_bytes.len());
    buf.extend_from_slice(&(total_len as u32).to_be_bytes());
    buf.extend_from_slice(&context_len.to_be_bytes());
    buf.extend_from_slice(&context_bytes);
    send.write_all(&buf).await?;
    use futures::AsyncWriteExt;
    send.flush().await?;
    Ok(())
}

/// Receive context from a stream.
async fn recv_context<R: AsyncRead + Unpin>(recv: &mut R) -> Result<StreamContext> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            anyhow::anyhow!("stream closed before context header")
        } else {
            anyhow::anyhow!("read error: {}", e)
        }
    })?;
    let total_len = u32::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(
        total_len <= MAX_MESSAGE_SIZE,
        "context too large: {} bytes (max {})",
        total_len,
        MAX_MESSAGE_SIZE
    );

    let mut buf = vec![0u8; total_len];
    recv.read_exact(&mut buf).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            anyhow::anyhow!("stream closed before context body")
        } else {
            anyhow::anyhow!("read error: {}", e)
        }
    })?;

    anyhow::ensure!(buf.len() >= 2, "context too short: missing context_len");
    let context_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    anyhow::ensure!(
        buf.len() >= 2 + context_len,
        "context too short: missing context body"
    );

    let (context, _) =
        bincode::serde::decode_from_slice(&buf[2..2 + context_len], bincode::config::standard())?;

    Ok(context)
}

// ── Framed message helpers (with compression flag) ─────────────────────────

/// Send a framed message with optional compression.
/// Wire: [4-byte BE total_len][1-byte flags][payload]
async fn send_framed<W: AsyncWrite + Unpin>(
    send: &mut W,
    data: &[u8],
    #[allow(unused_variables)] compress: bool,
) -> Result<()> {
    #[cfg(feature = "compress")]
    if compress {
        let (payload, was_compressed) = compress::compress(data);
        let flags = if was_compressed { FLAG_COMPRESSED } else { 0 };
        let total_len = 1 + payload.len();
        if total_len > MAX_MESSAGE_SIZE {
            log::error!(
                "send_framed: compressed frame too large: {} bytes raw → {} bytes compressed (max {})",
                data.len(),
                total_len,
                MAX_MESSAGE_SIZE
            );
            anyhow::bail!(
                "message too large: {} bytes (max {}) — raw input was {} bytes",
                total_len,
                MAX_MESSAGE_SIZE,
                data.len()
            );
        }
        let header = [
            (total_len as u32).to_be_bytes()[0],
            (total_len as u32).to_be_bytes()[1],
            (total_len as u32).to_be_bytes()[2],
            (total_len as u32).to_be_bytes()[3],
            flags,
        ];
        send.write_all(&header).await?;
        send.write_all(&payload).await?;
        return Ok(());
    }

    // No compression: write header + data directly, zero copy
    let total_len = 1u32 + data.len() as u32;
    if total_len as usize > MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "message too large: {} bytes (max {})",
            total_len,
            MAX_MESSAGE_SIZE
        );
    }
    let len_bytes = total_len.to_be_bytes();
    let header = [
        len_bytes[0],
        len_bytes[1],
        len_bytes[2],
        len_bytes[3],
        0u8, // flags: not compressed
    ];
    send.write_all(&header).await?;
    send.write_all(data).await?;
    Ok(())
}

/// Send a framed message with a pre-set compression flag, without compressing.
/// This is the send-side counterpart of `recv_framed_raw()` and is used for
/// pass-through relay where compressed data is forwarded as-is.
/// Wire: [4-byte BE total_len][1-byte flags][payload]
async fn send_framed_raw<W: AsyncWrite + Unpin>(
    send: &mut W,
    data: &[u8],
    is_compressed: bool,
) -> Result<()> {
    let flags = if is_compressed { FLAG_COMPRESSED } else { 0 };
    let total_len = 1u32 + data.len() as u32;
    if total_len as usize > MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "message too large: {} bytes (max {})",
            total_len,
            MAX_MESSAGE_SIZE
        );
    }
    let len_bytes = total_len.to_be_bytes();
    let header = [
        len_bytes[0],
        len_bytes[1],
        len_bytes[2],
        len_bytes[3],
        flags,
    ];
    send.write_all(&header).await?;
    send.write_all(data).await?;
    Ok(())
}

/// Receive a framed message, auto-decompressing if flagged.
/// Returns `None` if the stream is closed.
async fn recv_framed<R: AsyncRead + Unpin>(
    recv: &mut R,
    reuse_buf: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let total_len = u32::from_be_bytes(len_buf) as usize;
    if !(1..=MAX_MESSAGE_SIZE).contains(&total_len) {
        // Stream is likely out of sync — try to read a few more bytes
        // to help diagnose what went wrong.
        let mut diagnostic = [0u8; 64];
        match recv.read(&mut diagnostic).await {
            Ok(n) if n > 0 => {
                log::error!(
                    "recv_framed: stream out of sync! bogus total_len={} (0x{:08x}), \
                     next {} bytes: {:02x?}  ascii: {}",
                    total_len,
                    total_len,
                    n,
                    &diagnostic[..n],
                    String::from_utf8_lossy(&diagnostic[..n])
                );
            }
            _ => {
                log::error!(
                    "recv_framed: stream out of sync! bogus total_len={} (0x{:08x}), \
                     no more data readable",
                    total_len,
                    total_len
                );
            }
        }
        anyhow::bail!(
            "message too large: {} bytes (max {}) — raw len bytes: {:02x?} (stream out of sync)",
            total_len,
            MAX_MESSAGE_SIZE,
            len_buf
        );
    }
    reuse_buf.resize(total_len, 0);
    match recv.read_exact(reuse_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            log::warn!(
                "recv_framed: UnexpectedEof reading {} byte payload (len header was {:02x?})",
                total_len,
                len_buf
            );
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    let flags = reuse_buf[0];
    let payload = &reuse_buf[1..];
    let data = if flags & FLAG_COMPRESSED != 0 {
        #[cfg(feature = "compress")]
        {
            match compress::decompress(payload) {
                Ok(d) => d,
                Err(e) => {
                    log::error!(
                        "recv_framed: decompress failed for {} byte payload (flags=0x{:02x}, total_len={}): {}",
                        payload.len(),
                        flags,
                        total_len,
                        e
                    );
                    return Err(e.into());
                }
            }
        }
        #[cfg(not(feature = "compress"))]
        {
            anyhow::bail!("received compressed data but 'compress' feature is disabled");
        }
    } else {
        payload.to_vec()
    };
    Ok(Some(data))
}

/// Receive a framed message, returning the raw payload and compression flag
/// WITHOUT decompressing. This is useful for pass-through relay where the
/// compressed data is forwarded directly to another consumer.
/// Returns `None` if the stream is closed.
async fn recv_framed_raw<R: AsyncRead + Unpin>(
    recv: &mut R,
    reuse_buf: &mut Vec<u8>,
) -> Result<Option<(Vec<u8>, bool)>> {
    let mut len_buf = [0u8; 4];
    match recv.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let total_len = u32::from_be_bytes(len_buf) as usize;
    if !(1..=MAX_MESSAGE_SIZE).contains(&total_len) {
        let mut diagnostic = [0u8; 64];
        match recv.read(&mut diagnostic).await {
            Ok(n) if n > 0 => {
                log::error!(
                    "recv_framed_raw: stream out of sync! bogus total_len={} (0x{:08x}), \
                     next {} bytes: {:02x?}  ascii: {}",
                    total_len,
                    total_len,
                    n,
                    &diagnostic[..n],
                    String::from_utf8_lossy(&diagnostic[..n])
                );
            }
            _ => {
                log::error!(
                    "recv_framed_raw: stream out of sync! bogus total_len={} (0x{:08x}), \
                     no more data readable",
                    total_len,
                    total_len
                );
            }
        }
        anyhow::bail!(
            "message too large: {} bytes (max {}) — raw len bytes: {:02x?} (stream out of sync)",
            total_len,
            MAX_MESSAGE_SIZE,
            len_buf
        );
    }
    reuse_buf.resize(total_len, 0);
    match recv.read_exact(reuse_buf).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            log::warn!(
                "recv_framed_raw: UnexpectedEof reading {} byte payload (len header was {:02x?})",
                total_len,
                len_buf
            );
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    let flags = reuse_buf[0];
    let payload = &reuse_buf[1..];
    let is_compressed = flags & FLAG_COMPRESSED != 0;
    Ok(Some((payload.to_vec(), is_compressed)))
}

// ── StreamRegistry ─────────────────────────────────────────────────────────

/// Registry of active streams, with connection-level tracking.
///
/// Streams are keyed by `{role:?}-{id:x}` for individual lookup,
/// and also indexed by `connection_id` so all streams belonging to
/// a connection can be cleaned up at once when the connection drops.
pub struct StreamRegistry<S> {
    streams: HashMap<String, ContextStream<S>>,
    /// connection_id → set of stream keys belonging to this connection
    conn_index: HashMap<u64, HashSet<String>>,
}

impl<S> StreamRegistry<S> {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            conn_index: HashMap::new(),
        }
    }

    /// Build the registry key from role and id.
    fn make_key(role: StreamRole, id: u64) -> String {
        format!("{:?}-{:x}", role, id)
    }

    /// Insert a stream into the registry, associating it with a connection.
    pub fn insert(
        &mut self,
        connection_id: u64,
        role: StreamRole,
        id: u64,
        stream: ContextStream<S>,
    ) {
        let key = Self::make_key(role, id);
        self.conn_index
            .entry(connection_id)
            .or_default()
            .insert(key.clone());
        self.streams.insert(key, stream);
    }

    /// Get a stream by role and ID.
    pub fn get(&self, role: StreamRole, id: u64) -> Option<&ContextStream<S>> {
        self.streams.get(&Self::make_key(role, id))
    }

    /// Get a mutable reference to a stream by role and ID.
    pub fn get_mut(&mut self, role: StreamRole, id: u64) -> Option<&mut ContextStream<S>> {
        self.streams.get_mut(&Self::make_key(role, id))
    }

    /// Remove a single stream from the registry.
    pub fn remove(&mut self, role: StreamRole, id: u64) -> Option<ContextStream<S>> {
        let stream = self.streams.remove(&Self::make_key(role, id));
        if stream.is_some() {
            for keys in self.conn_index.values_mut() {
                keys.retain(|k| self.streams.contains_key(k));
            }
        }
        stream
    }

    /// Remove all streams belonging to a connection.
    pub fn remove_connection(&mut self, connection_id: u64) -> usize {
        if let Some(keys) = self.conn_index.remove(&connection_id) {
            let count = keys.len();
            for key in keys {
                self.streams.remove(&key);
            }
            count
        } else {
            0
        }
    }

    /// Check if a stream exists.
    pub fn contains(&self, role: StreamRole, id: u64) -> bool {
        self.streams.contains_key(&Self::make_key(role, id))
    }

    /// Number of streams in the registry.
    pub fn len(&self) -> usize {
        self.streams.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }
}

impl<S> Default for StreamRegistry<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_context_builder() {
        let ctx =
            StreamContext::new(1, "terminal_io", StreamRole::Agent).with_send_compressed(true);

        assert_eq!(ctx.id(), 1);
        assert_eq!(ctx.stream_type(), "terminal_io");
        assert_eq!(ctx.role(), StreamRole::Agent);
        assert!(ctx.send_compressed());
        assert!(!ctx.recv_compressed());
    }

    #[test]
    fn test_stream_context_compress_both() {
        let ctx =
            StreamContext::new(2, "file_transfer", StreamRole::Client).with_compress_both(true);
        assert!(ctx.send_compressed());
        assert!(ctx.recv_compressed());
    }

    #[test]
    fn test_stream_context_validate() {
        let ctx = StreamContext::new(0, "", StreamRole::Agent);
        assert!(ctx.validate().is_err());

        let ctx = StreamContext::new(0, "auth", StreamRole::Agent);
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn test_context_bincode_roundtrip() {
        let ctx =
            StreamContext::new(42, "terminal_io", StreamRole::Agent).with_send_compressed(true);

        let bytes = bincode::serde::encode_to_vec(&ctx, bincode::config::standard()).unwrap();
        let (decoded, _): (StreamContext, _) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

        assert_eq!(decoded.id(), 42);
        assert_eq!(decoded.stream_type(), "terminal_io");
        assert_eq!(decoded.role(), StreamRole::Agent);
        assert!(decoded.send_compressed());
        assert!(!decoded.recv_compressed());
    }

    #[test]
    fn test_merge_context() {
        let local = StreamContext::new(1, "terminal_io", StreamRole::Agent)
            .with_send_compressed(true)
            .with_send_data_type(StreamDataType::Raw)
            .with_recv_compressed(false)
            .with_recv_data_type(StreamDataType::Chunked);

        let peer = StreamContext::new(2, "terminal_io", StreamRole::Client)
            .with_send_compressed(false)
            .with_send_data_type(StreamDataType::Chunked)
            .with_recv_compressed(true)
            .with_recv_data_type(StreamDataType::Raw);

        let merged = merge_context(&local, &peer, StreamRole::Agent);

        assert_eq!(merged.id(), 1);
        assert_eq!(merged.role(), StreamRole::Agent);
        assert!(merged.send_compressed());
        assert_eq!(merged.send_data_type(), StreamDataType::Raw);
        assert!(!merged.recv_compressed());
        assert_eq!(merged.recv_data_type(), StreamDataType::Chunked);
    }

    #[test]
    fn test_merge_context_id_priority() {
        let local = StreamContext::new(1, "s", StreamRole::Agent);
        let peer = StreamContext::new(2, "s", StreamRole::Client);
        let merged = merge_context(&local, &peer, StreamRole::Agent);
        assert_eq!(merged.id(), 1);

        let local = StreamContext::new(10, "s", StreamRole::Client);
        let peer = StreamContext::new(20, "s", StreamRole::Server);
        let merged = merge_context(&local, &peer, StreamRole::Client);
        assert_eq!(merged.id(), 10);

        let local = StreamContext::new(10, "s", StreamRole::Client);
        let peer = StreamContext::new(20, "s", StreamRole::Agent);
        let merged = merge_context(&local, &peer, StreamRole::Client);
        assert_eq!(merged.id(), 20);
    }

    #[test]
    fn test_merge_context_id_fallback() {
        let local = StreamContext::new(0, "auth", StreamRole::Client);
        let peer = StreamContext::new(99, "auth", StreamRole::Agent);
        let merged = merge_context(&local, &peer, StreamRole::Client);
        assert_eq!(merged.id(), 99);
    }

    #[test]
    fn test_local_config() {
        let config = LocalConfig::new()
            .with_send_compressed(true)
            .with_send_data_type(StreamDataType::Raw)
            .with_id(42)
            .with_role(StreamRole::Agent);

        assert!(config.send_compressed);
        assert_eq!(config.send_data_type, StreamDataType::Raw);
        assert_eq!(config.id, 42);
        assert_eq!(config.role, StreamRole::Agent);
    }

    #[test]
    fn test_local_config_to_acceptor_context() {
        let opener_ctx = StreamContext::new(1, "terminal_io", StreamRole::Agent);
        let config = LocalConfig::new()
            .with_send_compressed(true)
            .with_recv_data_type(StreamDataType::Raw)
            .with_id(42)
            .with_role(StreamRole::Client);

        let acceptor_ctx = config.to_acceptor_context(&opener_ctx);
        assert_eq!(acceptor_ctx.id(), 42);
        assert_eq!(acceptor_ctx.stream_type(), "terminal_io");
        assert_eq!(acceptor_ctx.role(), StreamRole::Client);
        assert!(acceptor_ctx.send_compressed());
        assert_eq!(acceptor_ctx.recv_data_type(), StreamDataType::Raw);
    }

    #[test]
    fn test_stream_registry_key() {
        let key = StreamRegistry::<()>::make_key(StreamRole::Agent, 0xff);
        assert_eq!(key, "Agent-ff");

        let key = StreamRegistry::<()>::make_key(StreamRole::Client, 256);
        assert_eq!(key, "Client-100");
    }

    #[test]
    fn test_stream_registry_connection_cleanup() {
        let mut registry = StreamRegistry::<()>::new();

        registry
            .conn_index
            .entry(1)
            .or_default()
            .insert("Agent-1".into());
        registry
            .conn_index
            .entry(1)
            .or_default()
            .insert("Client-2".into());
        registry
            .conn_index
            .entry(2)
            .or_default()
            .insert("Agent-3".into());

        let removed = registry.remove_connection(1);
        assert_eq!(removed, 2);
        assert!(!registry.conn_index.contains_key(&1));
        assert!(registry.conn_index.contains_key(&2));

        let removed = registry.remove_connection(2);
        assert_eq!(removed, 1);
        assert!(registry.conn_index.is_empty());

        let removed = registry.remove_connection(99);
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_context_wire_format() {
        let ctx = StreamContext::new(99, "auth", StreamRole::Client);
        let context_bytes =
            bincode::serde::encode_to_vec(&ctx, bincode::config::standard()).unwrap();
        let context_len = context_bytes.len() as u16;
        let total_len = 2 + context_bytes.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(&(total_len as u32).to_be_bytes());
        buf.extend_from_slice(&context_len.to_be_bytes());
        buf.extend_from_slice(&context_bytes);

        assert_eq!(
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            total_len as u32
        );
        assert_eq!(u16::from_be_bytes([buf[4], buf[5]]), context_len);

        let (decoded, _): (StreamContext, _) = bincode::serde::decode_from_slice(
            &buf[6..6 + context_bytes.len()],
            bincode::config::standard(),
        )
        .unwrap();
        assert_eq!(decoded.stream_type(), "auth");
        assert_eq!(decoded.role(), StreamRole::Client);
    }

    #[cfg(feature = "compress")]
    #[test]
    fn test_framed_message_format() {
        let data = b"test payload";
        let (payload, was_compressed) = compress::compress(data);
        let flags = if was_compressed { FLAG_COMPRESSED } else { 0 };
        let total_len = 1 + payload.len();

        let mut buf = Vec::new();
        buf.extend_from_slice(&(total_len as u32).to_be_bytes());
        buf.push(flags);
        buf.extend_from_slice(&payload);

        assert_eq!(
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            total_len as u32
        );
        assert_eq!(buf[4], flags);

        if flags & FLAG_COMPRESSED != 0 {
            let decoded = compress::decompress(&buf[5..]).unwrap();
            assert_eq!(decoded, data);
        } else {
            assert_eq!(&buf[5..], data);
        }
    }
}
