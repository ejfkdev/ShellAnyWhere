/// Frame types matching Rust's protocol::control::Control enum exactly.
/// Discriminant indices must match the Rust enum variant order exactly.
/// This file mirrors the wire format of bincode::config::standard().

import { BincodeReader, BincodeWriter } from "./bincode";

// ── Supporting types ──

export interface SessionInfo {
  sessionId: string;
  shell: string;
  startedAt: number;
  cols: number;
  rows: number;
  cwd: string;
  firstCommand: string | null;
  terminalProgram: string | null;
  lastActivityAt: number;
  hostname: string;
  username: string;
  title: string;
}

export enum AttachMode {
  Observe,
  Interact,
}

// ── Frame enum (discriminant order matches Rust Control) ──

export type Frame =
  | { type: "AuthInit"; clientNonce: Uint8Array }
  | { type: "AuthChallenge"; nonce: Uint8Array; proof: Uint8Array }
  | { type: "AuthResponse"; response: Uint8Array }
  | { type: "AuthResult"; ok: boolean }
  | { type: "Ping" }
  | { type: "Pong" }
  | { type: "SessionRegister"; session: SessionInfo; sshPublicKeys: string[] }
  | { type: "SessionUpdate"; session: SessionInfo }
  | { type: "SessionList"; sessions: SessionInfo[] }
  | {
      type: "SessionAttach";
      sessionId: string;
      mode: AttachMode;
      previousClientId: string | null;
    }
  | { type: "AttachAck"; sessionId: string; clientId: string; mode: AttachMode }
  | { type: "AttachReject"; sessionId: string; reason: string }
  | { type: "SessionDetach"; sessionId: string; clientId: string }
  | { type: "SessionClose"; sessionId: string }
  | {
      type: "ClientAttached";
      sessionId: string;
      clientId: string;
      mode: AttachMode;
    }
  | {
      type: "ClientActive";
      sessionId: string;
      clientId: string;
      cols: number;
      rows: number;
    }
  | {
      type: "ClientResize";
      sessionId: string;
      clientId: string;
      cols: number;
      rows: number;
    }
  | {
      type: "DesktopNotification";
      sessionId: string;
      title: string;
      body: string;
    }
  | {
      type: "ClientSetTitle";
      sessionId: string;
      clientId: string;
      title: string;
    }
  | { type: "ClientRefresh"; sessionId: string; clientId: string };

// ── Codec ──

function readSessionInfo(r: BincodeReader): SessionInfo {
  return {
    sessionId: r.readString(),
    shell: r.readString(),
    startedAt: r.readU64(),
    cols: r.readU16(),
    rows: r.readU16(),
    cwd: r.readString(),
    firstCommand: r.readOption(() => r.readString()),
    terminalProgram: r.readOption(() => r.readString()),
    lastActivityAt: r.readU64(),
    hostname: r.readString(),
    username: r.readString(),
    title: r.readString(),
  };
}

function writeSessionInfo(w: BincodeWriter, s: SessionInfo) {
  w.writeString(s.sessionId);
  w.writeString(s.shell);
  w.writeU64(s.startedAt);
  w.writeU16(s.cols);
  w.writeU16(s.rows);
  w.writeString(s.cwd);
  w.writeOption(s.firstCommand, (v) => w.writeString(v));
  w.writeOption(s.terminalProgram, (v) => w.writeString(v));
  w.writeU64(s.lastActivityAt);
  w.writeString(s.hostname);
  w.writeString(s.username);
  w.writeString(s.title);
}

export function decodeFrame(data: Uint8Array): Frame {
  const r = new BincodeReader(data);
  const tag = r.readVarint();
  switch (tag) {
    case 0:
      return { type: "AuthInit", clientNonce: r.readVecU8() };
    case 1:
      return {
        type: "AuthChallenge",
        nonce: r.readVecU8(),
        proof: r.readVecU8(),
      };
    case 2:
      return { type: "AuthResponse", response: r.readVecU8() };
    case 3:
      return { type: "AuthResult", ok: r.readBool() };
    case 4:
      return { type: "Ping" };
    case 5:
      return { type: "Pong" };
    case 6: {
      const session = readSessionInfo(r);
      const sshPublicKeys: string[] = [];
      const n = r.readVarint();
      for (let i = 0; i < n; i++) sshPublicKeys.push(r.readString());
      return { type: "SessionRegister", session, sshPublicKeys };
    }
    case 7:
      return { type: "SessionUpdate", session: readSessionInfo(r) };
    case 8: {
      const sessions: SessionInfo[] = [];
      const n = r.readVarint();
      for (let i = 0; i < n; i++) sessions.push(readSessionInfo(r));
      return { type: "SessionList", sessions };
    }
    case 9:
      return {
        type: "SessionAttach",
        sessionId: r.readString(),
        mode: r.readVarint(),
        previousClientId: r.readOption(() => r.readString()),
      };
    case 10:
      return {
        type: "AttachAck",
        sessionId: r.readString(),
        clientId: r.readString(),
        mode: r.readVarint(),
      };
    case 11:
      return {
        type: "AttachReject",
        sessionId: r.readString(),
        reason: r.readString(),
      };
    case 12:
      return {
        type: "SessionDetach",
        sessionId: r.readString(),
        clientId: r.readString(),
      };
    case 13:
      return { type: "SessionClose", sessionId: r.readString() };
    case 14:
      return {
        type: "ClientAttached",
        sessionId: r.readString(),
        clientId: r.readString(),
        mode: r.readVarint(),
      };
    case 15:
      return {
        type: "ClientActive",
        sessionId: r.readString(),
        clientId: r.readString(),
        cols: r.readU16(),
        rows: r.readU16(),
      };
    case 16:
      return {
        type: "ClientResize",
        sessionId: r.readString(),
        clientId: r.readString(),
        cols: r.readU16(),
        rows: r.readU16(),
      };
    case 17:
      return {
        type: "DesktopNotification",
        sessionId: r.readString(),
        title: r.readString(),
        body: r.readString(),
      };
    case 18:
      return {
        type: "ClientSetTitle",
        sessionId: r.readString(),
        clientId: r.readString(),
        title: r.readString(),
      };
    case 19:
      return {
        type: "ClientRefresh",
        sessionId: r.readString(),
        clientId: r.readString(),
      };
    default:
      throw new Error(`bincode: unknown frame tag ${tag}`);
  }
}

export function encodeFrame(frame: Frame): Uint8Array {
  const w = new BincodeWriter();
  switch (frame.type) {
    case "AuthInit":
      w.writeVarint(0);
      w.writeVecU8(frame.clientNonce);
      break;
    case "AuthChallenge":
      w.writeVarint(1);
      w.writeVecU8(frame.nonce);
      w.writeVecU8(frame.proof);
      break;
    case "AuthResponse":
      w.writeVarint(2);
      w.writeVecU8(frame.response);
      break;
    case "AuthResult":
      w.writeVarint(3);
      w.writeBool(frame.ok);
      break;
    case "Ping":
      w.writeVarint(4);
      break;
    case "Pong":
      w.writeVarint(5);
      break;
    case "SessionRegister":
      w.writeVarint(6);
      writeSessionInfo(w, frame.session);
      w.writeVarint(frame.sshPublicKeys.length);
      for (const k of frame.sshPublicKeys) w.writeString(k);
      break;
    case "SessionUpdate":
      w.writeVarint(7);
      writeSessionInfo(w, frame.session);
      break;
    case "SessionList":
      w.writeVarint(8);
      w.writeVarint(frame.sessions.length);
      for (const s of frame.sessions) writeSessionInfo(w, s);
      break;
    case "SessionAttach":
      w.writeVarint(9);
      w.writeString(frame.sessionId);
      w.writeVarint(frame.mode);
      w.writeOption(frame.previousClientId, (v) => w.writeString(v));
      break;
    case "AttachAck":
      w.writeVarint(10);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      w.writeVarint(frame.mode);
      break;
    case "AttachReject":
      w.writeVarint(11);
      w.writeString(frame.sessionId);
      w.writeString(frame.reason);
      break;
    case "SessionDetach":
      w.writeVarint(12);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      break;
    case "SessionClose":
      w.writeVarint(13);
      w.writeString(frame.sessionId);
      break;
    case "ClientAttached":
      w.writeVarint(14);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      w.writeVarint(frame.mode);
      break;
    case "ClientActive":
      w.writeVarint(15);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      w.writeU16(frame.cols);
      w.writeU16(frame.rows);
      break;
    case "ClientResize":
      w.writeVarint(16);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      w.writeU16(frame.cols);
      w.writeU16(frame.rows);
      break;
    case "DesktopNotification":
      w.writeVarint(17);
      w.writeString(frame.sessionId);
      w.writeString(frame.title);
      w.writeString(frame.body);
      break;
    case "ClientSetTitle":
      w.writeVarint(18);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      w.writeString(frame.title);
      break;
    case "ClientRefresh":
      w.writeVarint(19);
      w.writeString(frame.sessionId);
      w.writeString(frame.clientId);
      break;
    default:
      throw new Error(
        `encodeFrame: unsupported frame type ${(frame as Frame).type}`,
      );
  }
  return w.toBytes();
}

/// Wrap a bincode-encoded frame with the 4-byte BE length prefix (wire format).
export function encodeWireFrame(frame: Frame): Uint8Array {
  const payload = encodeFrame(frame);
  const header = new Uint8Array(4);
  new DataView(header.buffer).setUint32(0, payload.length, false); // big-endian
  const result = new Uint8Array(4 + payload.length);
  result.set(header, 0);
  result.set(payload, 4);
  return result;
}
