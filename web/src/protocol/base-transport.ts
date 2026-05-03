/// Base transport class for WS-like protocols (WebSocket, WebRTC Data Channel).
/// Handles the shared type-prefixed binary message protocol:
///   0x00 = Control: [4B BE len][bincode Control]
///   0x01 = TerminalIO Output (server→client):
///          First message: [0x01][version=0x02][type=0x01][header_bytes]
///          Subsequent:   [0x01][0x00=raw | 0x01=lz4][payload]
///   0x02 = TerminalIO Input (client→server): raw keyboard input bytes
/// Each transport instance serves exactly one session (independent connection).

import { Frame, decodeFrame, encodeWireFrame } from "./frame";
import { readVarint, type TerminalIOHeader } from "./data-stream";
import * as lz4wasm from "lz4-wasm";

/// Callback type for TerminalIO stream delivery.
/// The consumer registers an output callback to receive terminal data.
export type TerminalIOStreamCallback = (
  onOutput: (cb: (data: Uint8Array) => void) => void,
  writable: WritableStream<Uint8Array>,
  header: TerminalIOHeader,
) => void;

/// WebSocket message type prefix bytes (must match server)
const WS_TYPE_CONTROL = 0x00;
const WS_TYPE_TERM_OUTPUT = 0x01;
const WS_TYPE_TERM_INPUT = 0x02;

/// TerminalIO output sub-flag bytes (must match server)
const TERM_FLAG_LZ4 = 0x01;

/// Abstract base transport for protocols sharing the WS binary format.
export abstract class BaseTransport {
  private onFrame_: ((frame: Frame) => void) | null = null;
  private onClose_: (() => void) | null = null;
  private onTerminalIOStream_: TerminalIOStreamCallback | null = null;

  // TerminalIO output callback (registered by consumer, one per transport)
  private outputCb_: ((data: Uint8Array) => void) | null = null;

  // Set to true by close() — prevents handleMessage during teardown
  protected _closed = false;

  // Subclass tag for log messages
  protected abstract readonly logTag: string;

  /// Whether the transport is currently connected.
  abstract get connected(): boolean;

  /// Send raw bytes over the transport.
  protected abstract sendRaw(msg: Uint8Array): void;

  /// Close the underlying connection.
  abstract close(): void;

  onFrame(cb: (frame: Frame) => void) {
    this.onFrame_ = cb;
  }

  onClose(cb: () => void) {
    this.onClose_ = cb;
  }

  onTerminalIOStream(cb: TerminalIOStreamCallback) {
    this.onTerminalIOStream_ = cb;
  }

  /// Send a control frame.
  async send(frame: Frame): Promise<void> {
    if (!this.connected) throw new Error("not connected");
    const payload = encodeWireFrame(frame);
    const msg = new Uint8Array(1 + payload.length);
    msg[0] = WS_TYPE_CONTROL;
    msg.set(payload, 1);

    if (
      frame.type === "SessionList" ||
      frame.type === "SessionAttach" ||
      frame.type === "AuthInit" ||
      frame.type === "AuthResponse" ||
      frame.type === "SessionDetach"
    ) {
      console.log(`[${this.logTag}] → ${frame.type}: ${payload.length} bytes`);
    }
    this.sendRaw(msg);
  }

  /// Handle an incoming binary message.
  protected handleMessage(data: ArrayBuffer) {
    if (this._closed) return;
    if (data.byteLength < 1) return;
    const buf = new Uint8Array(data);
    const msgType = buf[0];
    const payload = buf.slice(1);

    switch (msgType) {
      case WS_TYPE_CONTROL:
        this.handleControlMessage(payload);
        break;
      case WS_TYPE_TERM_OUTPUT:
        this.handleTermOutput(payload);
        break;
      default:
        console.warn(
          `[${this.logTag}] Unknown message type: 0x${msgType.toString(16)}`,
        );
    }
  }

  /// Reset TerminalIO state (call on close).
  protected resetTermState() {
    this.outputCb_ = null;
  }

  /// Invoke the close callback.
  protected notifyClose() {
    this.onClose_?.();
  }

  /// Handle type 0x00 Control messages: [4B BE len][bincode Control]
  private handleControlMessage(payload: Uint8Array) {
    if (payload.length < 4) {
      console.warn(
        `[${this.logTag}] Control message too short:`,
        payload.length,
      );
      return;
    }
    const len = new DataView(payload.buffer, payload.byteOffset, 4).getUint32(
      0,
      false,
    );
    if (len > 65536) {
      console.error(`[${this.logTag}] Control message invalid length:`, len);
      return;
    }
    if (payload.length < 4 + len) {
      console.warn(
        `[${this.logTag}] Control message truncated:`,
        payload.length,
        "<",
        4 + len,
      );
      return;
    }
    const bincodePayload = payload.slice(4, 4 + len);
    try {
      const frame = decodeFrame(bincodePayload);
      if (frame.type === "SessionUpdate") {
        const s = (frame as any).session;
        console.log(
          `[${this.logTag}] ← SessionUpdate: sid=${s?.sessionId} cwd=${s?.cwd} title=${s?.title}`,
        );
      } else if (frame.type === "SessionRegister") {
        const s = (frame as any).session;
        console.log(
          `[${this.logTag}] ← SessionRegister: sid=${s?.sessionId} cwd=${s?.cwd}`,
        );
      } else if (frame.type === "SessionClose") {
        console.log(
          `[${this.logTag}] ← SessionClose: sid=${(frame as any).sessionId}`,
        );
      } else if (frame.type === "SessionList") {
        const sessions: any[] = (frame as any).sessions ?? [];
        console.log(
          `[${this.logTag}] ← SessionList: ${sessions.length} sessions`,
          sessions.map((s: any) => `${s.sessionId} cwd=${s.cwd}`),
        );
      } else if (frame.type === "AuthChallenge") {
        console.log(`[${this.logTag}] ← AuthChallenge`);
      } else if (frame.type === "AuthResult") {
        console.log(`[${this.logTag}] ← AuthResult: ok=${(frame as any).ok}`);
      } else if (frame.type !== "Ping" && frame.type !== "Pong") {
        console.log(`[${this.logTag}] ← ${frame.type}:`, JSON.stringify(frame));
      }
      this.onFrame_?.(frame);
    } catch (e) {
      console.error(
        `[${this.logTag}] decode error: payload=${bincodePayload.length}B`,
        e,
      );
    }
  }

  /// Handle type 0x01 TerminalIO Output messages.
  /// Header message: [version=0x02][type=0x01][header_bytes]
  /// Data message:   [flag=0x00|0x01][payload]
  private handleTermOutput(payload: Uint8Array) {
    // Distinguish header (first message) from data (subsequent messages):
    // Header starts with version=0x02, streamType=0x01.
    // Data starts with flag=0x00 (raw) or flag=0x01 (lz4) — neither is 0x02.
    if (payload.length >= 2 && payload[0] === 0x02 && payload[1] === 0x01) {
      // Header message — parse and deliver stream
      const header = this.parseTermIOHeader(payload);
      if (header && this.onTerminalIOStream_) {
        const self = this;

        const writable = new WritableStream<Uint8Array>({
          write(chunk) {
            self.sendTermInput(chunk);
          },
        });

        const registerOutput = (cb: (data: Uint8Array) => void) => {
          self.outputCb_ = cb;
        };

        this.onTerminalIOStream_(registerOutput, writable, header);
      } else if (header && !this.onTerminalIOStream_) {
        console.warn(
          `[${this.logTag}] TerminalIO header received but no stream callback registered — dropping session ${header.sessionId}`,
        );
      }
    } else {
      // Data message — decompress and deliver
      if (!this.outputCb_) return;
      if (payload.length < 1) return;
      const flag = payload[0];
      const data = payload.slice(1);

      if (data.length === 0) return;

      if (flag === TERM_FLAG_LZ4) {
        if (this._closed) return;
        try {
          const decompressed = lz4wasm.decompress(data);
          if (!this._closed) {
            this.outputCb_(decompressed);
          }
        } catch (e) {
          console.warn(
            `[${this.logTag}] LZ4 decompression failed, skipping chunk:`,
            e,
          );
        }
      } else {
        this.outputCb_(data);
      }
    }
  }

  /// Parse TerminalIO header from first type 0x01 message.
  private parseTermIOHeader(data: Uint8Array): TerminalIOHeader | null {
    if (data.length < 4) {
      console.warn(
        `[${this.logTag}] TerminalIO header too short:`,
        data.length,
      );
      return null;
    }
    const version = data[0];
    const streamType = data[1];
    if (version !== 0x02 || streamType !== 0x01) {
      console.warn(
        `[${this.logTag}] Invalid TerminalIO header: version=${version} type=${streamType}`,
      );
      return null;
    }

    let offset = 2;
    const [sidLen, sidBytes] = readVarint(data, offset);
    offset += sidBytes;
    const sessionId = new TextDecoder().decode(
      data.slice(offset, offset + sidLen),
    );
    offset += sidLen;

    const [cidLen, cidBytes] = readVarint(data, offset);
    offset += cidBytes;
    const clientId = new TextDecoder().decode(
      data.slice(offset, offset + cidLen),
    );
    offset += cidLen;

    const flags = data[offset] ?? 0;

    return {
      sessionId,
      clientId,
      outputCompress: !!(flags & 0x02),
      inputCompress: !!(flags & 0x08),
    };
  }

  /// Send terminal input as type 0x02 message.
  private sendTermInput(data: Uint8Array) {
    if (!this.connected) return;
    const msg = new Uint8Array(1 + data.length);
    msg[0] = WS_TYPE_TERM_INPUT;
    msg.set(data, 1);
    this.sendRaw(msg);
  }
}
