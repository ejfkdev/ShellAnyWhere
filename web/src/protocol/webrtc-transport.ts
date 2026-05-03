/// WebRTC Data Channel client transport.
/// Connects via SDP signaling (POST /api/webrtc/offer) and uses a single
/// ordered Data Channel with the same type-prefixed binary format (shared
/// protocol handled by BaseTransport).

import { BaseTransport } from "./base-transport";

/// WebRTC connection timeout (ms)
const WEBRTC_CONNECT_TIMEOUT = 4000;

/// How long to wait for ICE to recover from 'disconnected' before giving up (ms)
const ICE_DISCONNECT_TIMEOUT = 5000;

export class WebrtcTransport extends BaseTransport {
  private pc: RTCPeerConnection | null = null;
  private dc: RTCDataChannel | null = null;
  protected readonly logTag = "WebrtcTransport";

  // ICE disconnect recovery timer
  private iceDisconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private destroyed = false;
  private _abortCtrl: AbortController | null = null;
  private wasConnected = false;

  // Stored reject for dcOpen promise — allows close() to unblock connect()
  private dcOpenReject_: ((reason: any) => void) | null = null;

  get connected(): boolean {
    return this.dc !== null && this.dc.readyState === "open";
  }

  protected sendRaw(msg: Uint8Array): void {
    this.dc?.send(msg as any);
  }

  async connect(url: string): Promise<void> {
    this.destroyed = false;
    this.pc = new RTCPeerConnection({
      iceServers: [],
      bundlePolicy: "max-bundle",
    });

    this.dc = this.pc.createDataChannel("saw-data", {
      ordered: true,
      maxRetransmits: 2,
    });

    this.dc.binaryType = "arraybuffer";
    this.dc.bufferedAmountLowThreshold = 65536;

    const dcOpen = new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(
        () => reject(new Error("DataChannel open timeout")),
        WEBRTC_CONNECT_TIMEOUT,
      );

      this.dcOpenReject_ = reject;

      this.dc!.onopen = () => {
        clearTimeout(timeout);
        this.dcOpenReject_ = null;
        this.dc!.onerror = null; // Clear connect-phase error handler
        console.log("[WebrtcTransport] DataChannel opened");
        resolve();
      };

      this.dc!.onerror = (e) => {
        clearTimeout(timeout);
        this.dcOpenReject_ = null;
        console.error("[WebrtcTransport] DataChannel error", e);
        reject(new Error("DataChannel error"));
      };
    });

    this.dc.onmessage = (event) => {
      this.handleMessage(event.data as ArrayBuffer);
    };

    this.dc.onclose = () => {
      console.log("[WebrtcTransport] DataChannel closed");
      this.cancelIceDisconnectTimer();
      this.resetTermState();
      if (this.wasConnected) this.notifyClose();
    };

    this.pc.oniceconnectionstatechange = () => {
      const state = this.pc?.iceConnectionState;
      console.log("[WebrtcTransport] ICE state:", state);

      if (state === "connected" || state === "completed") {
        this.cancelIceDisconnectTimer();
      } else if (state === "disconnected") {
        this.startIceDisconnectTimer();
      } else if (state === "failed" || state === "closed") {
        this.cancelIceDisconnectTimer();
        this.cleanup();
        if (this.wasConnected) this.notifyClose();
      }
    };

    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    await this.waitForIceGathering();

    const localSdp = this.pc.localDescription!.sdp;

    const offerUrl = new URL(url);
    offerUrl.pathname = "/api/webrtc/offer";

    const abortCtrl = new AbortController();
    const fetchTimeout = setTimeout(() => abortCtrl.abort(), 8000);
    this._abortCtrl = abortCtrl;

    let resp: Response;
    try {
      resp = await fetch(offerUrl.toString(), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ sdp: localSdp }),
        signal: abortCtrl.signal,
      });
    } finally {
      clearTimeout(fetchTimeout);
      this._abortCtrl = null;
    }

    if (!resp.ok) {
      const text = await resp.text();
      throw new Error(`WebRTC offer failed: ${resp.status} ${text}`);
    }

    const answerJson = await resp.json();
    const answerSdp: string = answerJson.sdp;

    await this.pc.setRemoteDescription(
      new RTCSessionDescription({
        type: "answer",
        sdp: answerSdp,
      }),
    );

    await dcOpen;
    this.wasConnected = true;
    console.log("[WebrtcTransport] Connected");
  }

  close() {
    this._closed = true;
    this.destroyed = true;
    // Unblock connect() if it's waiting on dcOpen
    this.dcOpenReject_?.(new Error("Transport closed during connect"));
    this.dcOpenReject_ = null;
    this._abortCtrl?.abort();
    this.cancelIceDisconnectTimer();
    this.cleanup();
  }

  private startIceDisconnectTimer() {
    if (this.iceDisconnectTimer) return;
    console.log(
      `[WebrtcTransport] ICE disconnected — waiting up to ${ICE_DISCONNECT_TIMEOUT}ms for recovery`,
    );
    this.iceDisconnectTimer = setTimeout(() => {
      this.iceDisconnectTimer = null;
      if (this.destroyed) return;
      console.warn(
        "[WebrtcTransport] ICE did not recover — closing connection",
      );
      this.cleanup();
      if (this.wasConnected) this.notifyClose();
    }, ICE_DISCONNECT_TIMEOUT);
  }

  private cancelIceDisconnectTimer() {
    if (this.iceDisconnectTimer) {
      clearTimeout(this.iceDisconnectTimer);
      this.iceDisconnectTimer = null;
    }
  }

  private cleanup() {
    this.resetTermState();
    if (this.dc) {
      this.dc.onmessage = null;
      this.dc.onerror = null;
      this.dc.onclose = null;
      this.dc.close();
      this.dc = null;
    }
    if (this.pc) {
      this.pc.oniceconnectionstatechange = null;
      this.pc.close();
      this.pc = null;
    }
  }

  private waitForIceGathering(): Promise<void> {
    return new Promise((resolve) => {
      if (!this.pc) return resolve();
      if (this.pc.iceGatheringState === "complete") return resolve();
      const timeout = setTimeout(() => resolve(), 3000);
      this.pc.onicegatheringstatechange = () => {
        if (this.pc?.iceGatheringState === "complete") {
          clearTimeout(timeout);
          resolve();
        }
      };
    });
  }
}
