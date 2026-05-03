/// Shared transport utilities: connection, authentication, frame waiting, type guards.
/// Used by both App.tsx (control connection) and TerminalView (per-session connection).

import { WsTransport, TerminalIOStreamCallback } from "./transport";
import { Frame, SessionInfo, AttachMode } from "./frame";
import { deriveAuthKey, computeAuthResponse } from "./auth";

// ── Transport type ──

export type TransportType = "webrtc" | "ws";

/// Common transport interface (supports both WebSocket and WebRTC).
export interface Transport {
  connect(url: string): Promise<void>;
  send(frame: Frame): Promise<void>;
  close(): void;
  onFrame(cb: (frame: Frame) => void): void;
  onClose(cb: () => void): void;
  get connected(): boolean;
  onTerminalIOStream?(cb: TerminalIOStreamCallback): void;
}

// ── Connection ──

/// Try to establish a transport connection. Attempts protocols in the given
/// order, falling back on failure. Returns the connected transport and which
/// protocol succeeded, or null if all fail.
export async function tryConnect(
  url: string,
  order: TransportType[],
): Promise<{ transport: Transport; used: TransportType } | null> {
  const connectWithTimeout = (p: Promise<void>, ms: number) =>
    Promise.race([
      p,
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("Timeout")), ms),
      ),
    ]);

  for (const proto of order) {
    try {
      if (proto === "webrtc") {
        console.log("[Transport] Trying WebRTC...");
        const { WebrtcTransport: WT } = await import("./webrtc-transport");
        const x = new WT();
        await connectWithTimeout(x.connect(url), 5000);
        console.log("[Transport] WebRTC connected");
        return { transport: x, used: "webrtc" };
      } else {
        console.log("[Transport] Trying WebSocket...");
        const x = new WsTransport();
        await connectWithTimeout(x.connect(url), 10000);
        console.log("[Transport] WebSocket connected");
        return { transport: x, used: "ws" };
      }
    } catch (e: any) {
      console.warn(
        `[Transport] ${proto === "webrtc" ? "WebRTC" : "WebSocket"} failed:`,
        e.message,
      );
    }
  }
  return null;
}

// ── Authentication ──

/// Perform mutual HMAC-SHA256 authentication on a transport.
/// Returns true on success, false on failure (closes transport).
export async function authenticateTransport(
  t: Transport,
  serverToken: string | undefined,
  interceptors: React.RefObject<((frame: Frame) => boolean)[]>,
): Promise<boolean> {
  if (serverToken) {
    const clientNonce = crypto.getRandomValues(new Uint8Array(32));
    const authKey = await deriveAuthKey(serverToken);
    t.send({ type: "AuthInit", clientNonce });
    const challengeFrame = await waitForFrame(
      interceptors,
      isAuthChallenge,
      10000,
    );
    // Verify server's proof (mutual auth)
    const expectedServerProof = await computeAuthResponse(
      authKey,
      clientNonce as Uint8Array,
    );
    const serverProof = new Uint8Array(challengeFrame.proof);
    if (
      expectedServerProof.length !== serverProof.length ||
      !expectedServerProof.every((v, i) => v === serverProof[i])
    ) {
      t.close();
      return false;
    }
    const response = await computeAuthResponse(authKey, challengeFrame.nonce);
    const authResultPromise = waitForFrame(interceptors, isAuthResult, 10000);
    await t.send({ type: "AuthResponse", response });
    const authResultFrame = await authResultPromise;
    if (!authResultFrame.ok) {
      t.close();
      return false;
    }
  }
  return true;
}

// ── Frame waiting ──

/// Wait for a frame matching a predicate, using interceptors.
export function waitForFrame<T extends Frame = Frame>(
  interceptors: React.RefObject<((frame: Frame) => boolean)[]>,
  predicate:
    | ((frame: Frame) => frame is T)
    | ((frame: Frame) => boolean),
  timeoutMs: number = 15000,
): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      interceptors.current = interceptors.current.filter(
        (i) => i !== interceptor,
      );
      reject(new Error("Frame wait timeout"));
    }, timeoutMs);

    const interceptor = (frame: Frame): boolean => {
      if (predicate(frame)) {
        clearTimeout(timer);
        interceptors.current = interceptors.current.filter(
          (i) => i !== interceptor,
        );
        resolve(frame as unknown as T);
        return true;
      }
      return false;
    };

    interceptors.current.push(interceptor);
  });
}

// ── Type guards ──

export function isAuthChallenge(
  f: Frame,
): f is Frame & {
  type: "AuthChallenge";
  nonce: Uint8Array;
  proof: Uint8Array;
} {
  return f.type === "AuthChallenge";
}

export function isAuthResult(
  f: Frame,
): f is Frame & { type: "AuthResult"; ok: boolean } {
  return f.type === "AuthResult";
}

export function isSessionList(
  f: Frame,
): f is Frame & { type: "SessionList"; sessions: SessionInfo[] } {
  return f.type === "SessionList";
}

export function isSessionUpdate(
  f: Frame,
): f is Frame & { type: "SessionUpdate"; session: SessionInfo } {
  return f.type === "SessionUpdate";
}

export function isSessionRegister(
  f: Frame,
): f is Frame & { type: "SessionRegister"; session: SessionInfo } {
  return f.type === "SessionRegister";
}

export function isSessionClose(
  f: Frame,
): f is Frame & { type: "SessionClose"; sessionId: string } {
  return f.type === "SessionClose";
}

export function isDesktopNotification(
  f: Frame,
): f is Frame & {
  type: "DesktopNotification";
  title: string;
  body: string;
} {
  return f.type === "DesktopNotification";
}

export function isAttachAck(
  f: Frame,
): f is Frame & {
  type: "AttachAck";
  sessionId: string;
  clientId: string;
  mode: AttachMode;
} {
  return f.type === "AttachAck";
}

export function isAttachReject(
  f: Frame,
): f is Frame & {
  type: "AttachReject";
  sessionId: string;
  reason: string;
} {
  return f.type === "AttachReject";
}

export function isAttachResult(
  f: Frame,
): f is Frame &
  (
    | {
        type: "AttachAck";
        sessionId: string;
        clientId: string;
        mode: AttachMode;
      }
    | { type: "AttachReject"; sessionId: string; reason: string }
  ) {
  return f.type === "AttachAck" || f.type === "AttachReject";
}

/// Extract sessionId from any frame that has one.
export function getFrameSessionId(f: Frame): string | undefined {
  if ("sessionId" in f) return (f as { sessionId: string }).sessionId;
  return undefined;
}
