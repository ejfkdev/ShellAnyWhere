/// Native WebSocket client transport.
/// Connects to the server's /ws endpoint and sends/receives type-prefixed
/// binary messages (shared protocol handled by BaseTransport).

import { BaseTransport } from "./base-transport";

export type { TerminalIOStreamCallback } from "./base-transport";

export class WsTransport extends BaseTransport {
  private ws: WebSocket | null = null;
  protected readonly logTag = "WsTransport";
  private wasConnected = false;

  get connected(): boolean {
    return this.ws !== null && this.ws.readyState === WebSocket.OPEN;
  }

  protected sendRaw(msg: Uint8Array): void {
    this.ws?.send(msg as any);
  }

  async connect(url: string): Promise<void> {
    const wsUrl = url.replace(/^http/, "ws") + "/ws";

    this.ws = new WebSocket(wsUrl);
    this.ws.binaryType = "arraybuffer";

    return new Promise<void>((resolve, reject) => {
      if (!this.ws) return reject(new Error("ws not created"));

      const safetyTimer = setTimeout(() => {
        if (this.ws && this.ws.readyState !== WebSocket.OPEN) {
          this.ws.onclose = null;
          this.ws.close();
          this.ws = null;
          reject(new Error("WebSocket connect timeout"));
        }
      }, 12000);

      this.ws.onopen = () => {
        clearTimeout(safetyTimer);
        this.wasConnected = true;
        console.log("[WsTransport] Connected to", wsUrl);
        resolve();
      };

      this.ws.onerror = (e) => {
        clearTimeout(safetyTimer);
        console.error("[WsTransport] Connection error", e);
        reject(new Error("WebSocket connection failed"));
      };

      this.ws.onmessage = (event) => {
        this.handleMessage(event.data as ArrayBuffer);
      };

      this.ws.onclose = () => {
        clearTimeout(safetyTimer);
        console.log("[WsTransport] Connection closed");
        this.resetTermState();
        // Only notify close if we were previously connected — prevents
        // spurious reconnect on connect failure (onerror already rejected)
        if (this.wasConnected) this.notifyClose();
      };
    });
  }

  close() {
    this._closed = true;
    this.resetTermState();
    if (this.ws) {
      this.ws.onmessage = null;
      this.ws.onclose = null;
      this.ws.close();
      this.ws = null;
    }
  }
}
