/// Multi-stream data protocol: stream header parsing and varint helpers.

/// Parsed TerminalIO stream header
export interface TerminalIOHeader {
  sessionId: string;
  clientId: string;
  outputCompress: boolean;
  inputCompress: boolean;
}

/// TerminalIO stream handle delivered to TerminalView
export interface TerminalIOStream {
  /// Register a callback to receive output data from the server.
  /// Data is pushed synchronously from the transport's onmessage handler.
  /// The consumer should buffer and flush on a timer.
  onOutput: (cb: (data: Uint8Array) => void) => void;
  writable: WritableStream<Uint8Array>;
  header: TerminalIOHeader;
}

/// Read a varint from a Uint8Array at the given offset.
/// Returns [value, bytesRead].
export function readVarint(buf: Uint8Array, offset: number): [number, number] {
  let v = 0;
  let shift = 0;
  let i = offset;
  while (i < buf.length) {
    const byte = buf[i++];
    v |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) break;
    shift += 7;
    if (shift >= 64) throw new Error("varint overflow");
  }
  if (i > offset && (buf[i - 1] & 0x80) !== 0) {
    throw new Error("varint: unexpected end of data");
  }
  return [v, i - offset];
}
