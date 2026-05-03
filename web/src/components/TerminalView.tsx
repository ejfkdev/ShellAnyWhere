/// Terminal view: wterm integrated with per-session transport.
/// Each TerminalView owns its own transport connection (WebSocket or WebRTC),
/// handles connect/auth/attach/reconnect independently.

import { useEffect, useRef, useCallback } from 'react';
import { Terminal } from '../wterm/react/index.js';
import type { WTerm } from '../wterm/react/index.js';
import '../wterm/dom/terminal.css';
import { Frame, SessionInfo, AttachMode } from '../protocol/frame';
import {
  type Transport,
  type TransportType,
  authenticateTransport,
  waitForFrame,
  isAttachReject,
  isAttachResult,
  getFrameSessionId,
} from '../protocol/transport-utils';
import { ctrlKeySequence } from '../utils/ctrl-keys';

interface Props {
  sessionId: string;
  active: boolean;
  fontSize: number;
  sessionInfo: SessionInfo | undefined;
  serverToken: string | undefined;
  transportFactory: () => Promise<{ transport: Transport; used: TransportType } | null>;
  onAttached: (sessionId: string, clientId: string) => void;
  onDetached: (sessionId: string) => void;
  onError: (sessionId: string, error: string) => void;
  onTitle?: (sessionId: string, title: string) => void;
  vkCtrl: boolean;
  vkAlt: boolean;
  onSetActiveInput: (handler: ((data: string) => void) | null) => void;
}

/// Measure the actual row height for the current font size.
function measureTrueRowHeight(wtermEl: HTMLElement): number | null {
  const grid = wtermEl.querySelector('.term-grid');
  if (!grid) return null;
  const probe = document.createElement('div');
  probe.style.cssText = `
    visibility: hidden;
    position: absolute;
    font-family: var(--term-font-family);
    font-size: var(--term-font-size);
    line-height: var(--term-line-height);
    white-space: pre;
  `;
  probe.textContent = 'W';
  grid.appendChild(probe);
  const h = probe.getBoundingClientRect().height;
  probe.remove();
  return h > 0 ? Math.ceil(h) : null;
}

/// Calculate and apply the correct terminal size after font/row-height changes.
function applyTerminalSize(wt: any) {
  if (!wt?.element) return;
  const wtermEl = wt.element as HTMLElement;
  const style = getComputedStyle(wtermEl);
  const pl = parseFloat(style.paddingLeft) || 0;
  const pr = parseFloat(style.paddingRight) || 0;
  const pt = parseFloat(style.paddingTop) || 0;
  const pb = parseFloat(style.paddingBottom) || 0;
  const rect = wtermEl.getBoundingClientRect();

  const charWidth = wt._measureCharSize?.()?.charWidth
    || parseFloat(style.fontSize) * 0.6;
  const rowHeight = wt._rowHeight || parseFloat(style.fontSize) * 1.333;

  const contentWidth = rect.width - pl - pr;
  const newCols = Math.max(1, Math.floor(contentWidth / charWidth));
  const newRows = Math.max(1, Math.floor((rect.height - pt - pb) / rowHeight));

  if (newCols !== wt.cols || newRows !== wt.rows) {
    wt.resize(newCols, newRows);
  }
}

function isScrolledToBottom(wt: any): boolean {
  const el = wt?.element as HTMLElement | null;
  if (!el) return true;
  const rh = wt?._rowHeight || 19;
  return el.scrollHeight - el.scrollTop - el.clientHeight < rh;
}

export function TerminalView({
  sessionId, active, fontSize, sessionInfo,
  serverToken, transportFactory,
  onAttached, onDetached, onError, onTitle,
  vkCtrl, vkAlt, onSetActiveInput,
}: Props) {
  const termRef = useRef<WTerm | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  // Transport state
  const transportRef = useRef<Transport | null>(null);
  const clientIdRef = useRef<string>('');
  const connectedRef = useRef(false);
  const reconnectingRef = useRef(false);
  const lastTransportTypeRef = useRef<TransportType>('webrtc');
  const interceptors = useRef<((frame: Frame) => boolean)[]>([]);
  const destroyedRef = useRef(false);

  // TerminalIO stream state
  const streamWriterRef = useRef<WritableStreamDefaultWriter<Uint8Array> | null>(null);
  const streamCleanupRef = useRef<(() => void) | null>(null);

  // Track whether user is at bottom
  const atBottomRef = useRef(true);

  // Whether this client is the server-recognized active client
  const isActiveClientRef = useRef(true);
  const needsReclaimRef = useRef(true);

  // Virtual keyboard modifier state
  const vkCtrlRef = useRef(vkCtrl);
  const vkAltRef = useRef(vkAlt);
  vkCtrlRef.current = vkCtrl;
  vkAltRef.current = vkAlt;

  // Track remote shell size and last sent size
  const remoteSizeRef = useRef({ cols: 0, rows: 0 });
  const lastSentSizeRef = useRef({ cols: 0, rows: 0 });

  // Reconnect wake function
  const reconnectWakeRef = useRef<(() => void) | null>(null);
  const hiddenAtRef = useRef(0);

  // Sync remoteSizeRef from sessionInfo
  useEffect(() => {
    if (sessionInfo && sessionInfo.cols > 0 && sessionInfo.rows > 0) {
      remoteSizeRef.current = { cols: sessionInfo.cols, rows: sessionInfo.rows };
    }
  }, [sessionInfo]);

  // ── Connection lifecycle ──

  const doConnectAndAttach = useCallback(async (previousClientId: string | null): Promise<boolean> => {
    const result = await transportFactory();
    if (!result) return false;

    const t = result.transport;
    lastTransportTypeRef.current = result.used;
    interceptors.current = [];

    // Set up frame dispatch
    t.onFrame((frame: Frame) => {
      // Intercept for waitForFrame promises first
      for (const interceptor of [...interceptors.current]) {
        if (interceptor(frame)) return;
      }

      // Handle session-level frames
      if (frame.type === 'SessionClose' && getFrameSessionId(frame) === sessionId) {
        termRef.current?.write('\r\n\x1b[33mSession closed\x1b[0m\r\n');
      } else if (frame.type === 'ClientActive') {
        const { clientId: activeCid, cols, rows } = frame as any;
        if (cols > 0 && rows > 0) remoteSizeRef.current = { cols, rows };
        if (activeCid === clientIdRef.current) {
          isActiveClientRef.current = true;
          needsReclaimRef.current = false;
        } else {
          isActiveClientRef.current = false;
          needsReclaimRef.current = true;
        }
      } else if (frame.type === 'DesktopNotification') {
        const { title, body } = frame as any;
        if (Notification.permission === 'granted') {
          new Notification(title, { body });
        } else if (Notification.permission !== 'denied') {
          Notification.requestPermission().then(perm => {
            if (perm === 'granted') new Notification(title, { body });
          });
        }
      }
    });

    t.onClose(() => {
      connectedRef.current = false;
      streamCleanupRef.current?.();
      streamCleanupRef.current = null;
      streamWriterRef.current = null;
      if (!destroyedRef.current) {
        startReconnect();
      }
    });

    // Authenticate
    const authOk = await authenticateTransport(t, serverToken, interceptors);
    if (!authOk) { t.close(); return false; }

    // Attach to session
    t.send({ type: 'SessionAttach', sessionId, mode: AttachMode.Interact, previousClientId });

    try {
      const attachResult = await waitForFrame(
        interceptors,
        (f): f is Frame & ({ type: 'AttachAck' } | { type: 'AttachReject' }) =>
          isAttachResult(f) && getFrameSessionId(f) === sessionId,
        15000,
      );

      if (isAttachReject(attachResult)) {
        t.close();
        onError(sessionId, `Attach rejected: ${attachResult.reason}`);
        return false;
      }

      clientIdRef.current = attachResult.clientId;
    } catch {
      t.close();
      return false;
    }

    // Set up TerminalIO stream
    t.onTerminalIOStream!((onOutput, writable, header) => {
      console.log(
        `[TV ${sessionId.slice(0,8)}] TerminalIO stream: client=${header.clientId} compress=${header.outputCompress}/${header.inputCompress}`,
      );

      const writer = writable.getWriter();
      streamWriterRef.current = writer;

      let pending: Uint8Array[] = [];
      let timerId: ReturnType<typeof setTimeout> | null = null;
      const FLUSH_INTERVAL = 100;

      const flushToTerminal = () => {
        timerId = null;
        if (pending.length === 0) return;
        const wt = termRef.current;
        if (!wt) { pending = []; return; }

        if (pending.length === 1) {
          wt.write(pending[0]);
        } else {
          const totalLen = pending.reduce((acc, c) => acc + c.length, 0);
          const merged = new Uint8Array(totalLen);
          let offset = 0;
          for (const chunk of pending) {
            merged.set(chunk, offset);
            offset += chunk.length;
          }
          wt.write(merged);
        }
        pending = [];
      };

      onOutput((data: Uint8Array) => {
        pending.push(data);
        if (timerId == null) {
          timerId = setTimeout(flushToTerminal, FLUSH_INTERVAL);
        }
      });

      streamCleanupRef.current = () => {
        try { writer.releaseLock(); } catch {}
        if (timerId != null) { clearTimeout(timerId); timerId = null; }
        streamWriterRef.current = null;
      };
    });

    // Store transport
    transportRef.current = t;
    connectedRef.current = true;
    onAttached(sessionId, clientIdRef.current);
    console.log(`[TV ${sessionId.slice(0,8)}] Connected and attached (cid=${clientIdRef.current.slice(0,8)})`);
    return true;
  }, [sessionId, serverToken, transportFactory, onAttached, onError]);

  // Reconnect with two-phase backoff
  const startReconnect = useCallback(async () => {
    if (reconnectingRef.current || destroyedRef.current) return;
    reconnectingRef.current = true;

    const FAST_ATTEMPTS = 100;
    const FAST_MIN = 1000;
    const FAST_MAX = 2000;
    const SLOW_MIN = 60000;
    const SLOW_MAX = 120000;

    const interruptibleDelay = (ms: number): Promise<void> =>
      new Promise<void>(resolve => {
        const timer = setTimeout(resolve, ms);
        reconnectWakeRef.current = () => { clearTimeout(timer); resolve(); };
      });

    let attempt = 0;
    const prevClientId = clientIdRef.current;

    while (reconnectingRef.current && !destroyedRef.current) {
      if (attempt > 0) {
        const delayMs = attempt < FAST_ATTEMPTS
          ? FAST_MIN + Math.random() * (FAST_MAX - FAST_MIN)
          : SLOW_MIN + Math.random() * (SLOW_MAX - SLOW_MIN);
        console.log(`[TV ${sessionId.slice(0,8)}] Reconnect attempt ${attempt + 1}, waiting ${Math.round(delayMs)}ms`);
        await interruptibleDelay(delayMs);
        reconnectWakeRef.current = null;
      }

      try {
        if (await doConnectAndAttach(prevClientId || null)) {
          reconnectingRef.current = false;
          console.log(`[TV ${sessionId.slice(0,8)}] Reconnected`);
          return;
        }
      } catch {}
      attempt++;
    }
    reconnectingRef.current = false;
  }, [sessionId, doConnectAndAttach]);

  // Initial connect on mount
  useEffect(() => {
    destroyedRef.current = false;
    doConnectAndAttach(null).then(ok => {
      if (!ok && !destroyedRef.current) {
        onError(sessionId, 'Failed to connect and attach');
      }
    });

    return () => {
      destroyedRef.current = true;
      reconnectingRef.current = false;
      reconnectWakeRef.current?.();
      reconnectWakeRef.current = null;

      // Send detach and close transport
      const t = transportRef.current;
      if (t) {
        const cid = clientIdRef.current;
        if (cid) {
          try { t.send({ type: 'SessionDetach', sessionId, clientId: cid }); } catch {}
        }
        t.close();
        transportRef.current = null;
      }
      connectedRef.current = false;
      streamCleanupRef.current?.();
      streamCleanupRef.current = null;
      streamWriterRef.current = null;
      onDetached(sessionId);
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Visibility change: probe connection and refresh
  useEffect(() => {
    const REFRESH_THRESHOLD = 3000;

    const handleVisibility = () => {
      if (document.visibilityState === 'hidden') {
        hiddenAtRef.current = Date.now();
        return;
      }

      const hiddenDuration = hiddenAtRef.current ? Date.now() - hiddenAtRef.current : 0;
      hiddenAtRef.current = 0;

      if (reconnectingRef.current) {
        reconnectWakeRef.current?.();
        return;
      }

      if (!connectedRef.current) {
        startReconnect();
        return;
      }

      if (hiddenDuration > REFRESH_THRESHOLD) {
        const t = transportRef.current;
        if (!t) return;

        // Probe connection liveness
        const probeTimeout = setTimeout(() => {
          console.warn(`[TV ${sessionId.slice(0,8)}] Visibility probe failed — reconnecting`);
          if (transportRef.current === t) {
            t.close();
            transportRef.current = null;
          }
          connectedRef.current = false;
          startReconnect();
        }, 5000);

        t.send({ type: 'SessionList', sessions: [] });
        waitForFrame(interceptors, (f: Frame) => f.type === 'SessionList', 10000)
          .then(() => clearTimeout(probeTimeout))
          .catch(() => clearTimeout(probeTimeout));

        // Request terminal screen refresh
        const cid = clientIdRef.current;
        if (cid) {
          t.send({ type: 'ClientRefresh', sessionId, clientId: cid });
        }
      }
    };

    document.addEventListener('visibilitychange', handleVisibility);
    return () => document.removeEventListener('visibilitychange', handleVisibility);
  }, [sessionId, startReconnect]);

  // bfcache restoration
  useEffect(() => {
    const handlePageshow = (e: PageTransitionEvent) => {
      if (!e.persisted) return;
      const t = transportRef.current;
      if (t) t.close();
      transportRef.current = null;
      connectedRef.current = false;
      reconnectingRef.current = false;
      startReconnect();
    };
    window.addEventListener('pageshow', handlePageshow);
    return () => window.removeEventListener('pageshow', handlePageshow);
  }, [startReconnect]);

  // beforeunload: close transport to prevent refresh hang
  useEffect(() => {
    const handleBeforeUnload = () => {
      const t = transportRef.current;
      if (t) { t.close(); transportRef.current = null; }
    };
    window.addEventListener('beforeunload', handleBeforeUnload);
    return () => window.removeEventListener('beforeunload', handleBeforeUnload);
  }, []);

  // ── Resize / title ──

  const sendResizeIfDifferent = useCallback((source: string): boolean => {
    const wt = termRef.current;
    if (!wt?.element) return false;
    const wtermEl = wt.element as HTMLElement;
    const style = getComputedStyle(wtermEl);
    const pl = parseFloat(style.paddingLeft) || 0;
    const pr = parseFloat(style.paddingRight) || 0;
    const pt = parseFloat(style.paddingTop) || 0;
    const pb = parseFloat(style.paddingBottom) || 0;
    const rect = wtermEl.getBoundingClientRect();
    const charWidth = wt._measureCharSize?.()?.charWidth
      || parseFloat(style.fontSize) * 0.6;
    const rowHeight = wt._rowHeight || parseFloat(style.fontSize) * 1.333;
    const contentWidth = rect.width - pl - pr;
    const cols = Math.floor(contentWidth / charWidth);
    const rows = Math.floor((rect.height - pt - pb) / rowHeight);
    const remote = remoteSizeRef.current;
    const last = lastSentSizeRef.current;
    if (cols > 0 && rows > 0
        && (cols !== remote.cols || rows !== remote.rows)
        && (cols !== last.cols || rows !== last.rows)) {
      lastSentSizeRef.current = { cols, rows };
      isActiveClientRef.current = true;
      const t = transportRef.current;
      const cid = clientIdRef.current;
      if (t?.connected && cid) {
        console.log(`[TV] sendResizeIfDifferent(${source}): ${cols}x${rows} → sending`);
        t.send({ type: 'ClientResize', sessionId, clientId: cid, cols, rows });
      }
      return true;
    }
    return false;
  }, [sessionId]);

  const handleResize = useCallback((cols: number, rows: number) => {
    if (active && isActiveClientRef.current && (cols !== lastSentSizeRef.current.cols || rows !== lastSentSizeRef.current.rows)) {
      lastSentSizeRef.current = { cols, rows };
      const t = transportRef.current;
      const cid = clientIdRef.current;
      if (t?.connected && cid) {
        t.send({ type: 'ClientResize', sessionId, clientId: cid, cols, rows });
      }
    }
  }, [active, sessionId]);

  // When tab becomes active, re-declare as active client if size differs
  useEffect(() => {
    if (!active) return;
    requestAnimationFrame(() => sendResizeIfDifferent('tab-activate'));
  }, [active, sendResizeIfDifferent]);

  // ── Font size / zoom ──

  const fontSizeRef = useRef(fontSize);
  fontSizeRef.current = fontSize;
  const pinchActiveRef = useRef(false);

  useEffect(() => {
    if (!active) return;
    const wt = termRef.current;
    if (!wt?.element) return;

    const wtermEl = wt.element as HTMLElement;
    wtermEl.style.setProperty('--term-font-size', `${fontSize}px`);
    void wtermEl.offsetHeight;

    const trueRowHeight = measureTrueRowHeight(wtermEl);
    if (trueRowHeight) {
      wtermEl.style.setProperty('--term-row-height', `${trueRowHeight}px`);
      wt._rowHeight = trueRowHeight;
    }

    if (pinchActiveRef.current) return;
    applyTerminalSize(wt);
  }, [fontSize, active]);

  // Pinch-to-zoom
  useEffect(() => {
    const viewEl = containerRef.current;
    if (!viewEl) return;

    let prevDist = -1;
    let currentSize = 0;
    let pinching = false;
    const STEP_THRESHOLD = 50;

    const getDist = (t: TouchList) => {
      const dx = t[0].clientX - t[1].clientX;
      const dy = t[0].clientY - t[1].clientY;
      return Math.sqrt(dx * dx + dy * dy);
    };

    const updateVisualSize = (size: number) => {
      const wt = termRef.current;
      if (!wt?.element) return;
      const wtermEl = wt.element as HTMLElement;
      wtermEl.style.setProperty('--term-font-size', `${size}px`);
      void wtermEl.offsetHeight;
      const trueRowHeight = measureTrueRowHeight(wtermEl);
      if (trueRowHeight) {
        wtermEl.style.setProperty('--term-row-height', `${trueRowHeight}px`);
        wt._rowHeight = trueRowHeight;
      }
    };

    const onTouchStart = (e: TouchEvent) => {
      if (!active) return;
      const rect = viewEl.getBoundingClientRect();
      const inView = Array.from(e.touches).some(t =>
        t.clientX >= rect.left && t.clientX <= rect.right &&
        t.clientY >= rect.top && t.clientY <= rect.bottom
      );
      if (!inView) return;
      if (e.touches.length === 2) {
        prevDist = getDist(e.touches);
        currentSize = fontSizeRef.current;
        pinching = true;
        pinchActiveRef.current = true;
      }
    };

    const onTouchMove = (e: TouchEvent) => {
      if (!pinching || e.touches.length !== 2 || prevDist < 0) return;
      e.preventDefault();
      const curDist = getDist(e.touches);
      const delta = curDist - prevDist;

      if (Math.abs(delta) > STEP_THRESHOLD) {
        const step = delta > 0 ? 1 : -1;
        const target = Math.max(8, Math.min(32, currentSize + step));
        if (target !== currentSize) {
          currentSize = target;
          updateVisualSize(target);
          viewEl.dispatchEvent(new CustomEvent('term-zoom', {
            bubbles: true,
            detail: { fontSize: target },
          }));
        }
        prevDist = curDist;
      }
    };

    const onTouchEnd = (e: TouchEvent) => {
      if (!pinching) return;
      if (e.touches.length < 2) {
        pinching = false;
        pinchActiveRef.current = false;
        prevDist = -1;
        const wt = termRef.current;
        if (wt?.element) applyTerminalSize(wt);
      }
    };

    document.addEventListener('touchstart', onTouchStart, { passive: true });
    document.addEventListener('touchmove', onTouchMove, { passive: false });
    document.addEventListener('touchend', onTouchEnd, { passive: true });

    return () => {
      document.removeEventListener('touchstart', onTouchStart);
      document.removeEventListener('touchmove', onTouchMove);
      document.removeEventListener('touchend', onTouchEnd);
    };
  }, [active]);

  // Ctrl+scroll zoom
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const onWheel = (e: WheelEvent) => {
      if (e.ctrlKey) {
        e.preventDefault();
        const delta = e.deltaY > 0 ? -1 : 1;
        el.dispatchEvent(new CustomEvent('term-zoom', {
          bubbles: true,
          detail: { fontSize: Math.max(8, Math.min(32, fontSizeRef.current + delta)) },
        }));
      }
    };

    el.addEventListener('wheel', onWheel, { passive: false });
    return () => el.removeEventListener('wheel', onWheel);
  }, []);

  // ── Input handling ──

  const handleData = useCallback((data: string) => {
    if (active) {
      if (needsReclaimRef.current) {
        needsReclaimRef.current = false;
      }
      sendResizeIfDifferent('keypress');
    }

    if (vkCtrlRef.current && data.length === 1) {
      const ctrlChar = ctrlKeySequence(data);
      if (ctrlChar !== null) {
        vkCtrlRef.current = false;
        data = ctrlChar;
      }
    }
    if (vkAltRef.current && data.length === 1) {
      vkAltRef.current = false;
      data = '\x1b' + data;
    }
    let payload: Uint8Array = new TextEncoder().encode(data);

    if (streamWriterRef.current) {
      streamWriterRef.current.write(payload).catch(() => {
        streamWriterRef.current = null;
      });
      return;
    }
  }, [active, sendResizeIfDifferent]);

  // Track scroll position
  useEffect(() => {
    if (!active) return;
    const wt = termRef.current;
    const el = wt?.element as HTMLElement | null;
    if (!el) return;

    const onScroll = () => { atBottomRef.current = isScrolledToBottom(wt); };
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => el.removeEventListener('scroll', onScroll);
  }, [active]);

  // wterm ready
  const termReadyRef = useRef(0);

  const handleReady = useCallback((wt: WTerm) => {
    termRef.current = wt;

    const wtermEl = wt.element as HTMLElement;
    wtermEl.style.setProperty('--term-font-size', `${fontSizeRef.current}px`);
    void wtermEl.offsetHeight;

    const trueRowHeight = measureTrueRowHeight(wtermEl);
    if (trueRowHeight) {
      wtermEl.style.setProperty('--term-row-height', `${trueRowHeight}px`);
      wt._rowHeight = trueRowHeight;
    }

    applyTerminalSize(wt);
    termReadyRef.current++;
  }, []);

  // Register event listeners on terminal element
  useEffect(() => {
    const wt = termRef.current;
    if (!wt?.element) return;

    const el = wt.element as HTMLElement;
    const onScroll = () => { atBottomRef.current = isScrolledToBottom(wt); };
    el.addEventListener('scroll', onScroll, { passive: true });

    const textarea = wt.input?.textarea as HTMLTextAreaElement | undefined;
    if (!textarea) {
      return () => el.removeEventListener('scroll', onScroll);
    }

    const windowCtrlCHandler = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.altKey && !e.metaKey && e.key.toLowerCase() === 'c') {
        e.preventDefault();
        e.stopPropagation();
        wt.input?.onData("\x03");
      }
    };
    window.addEventListener('keydown', windowCtrlCHandler, true);

    const ctrlStopPropHandler = (e: KeyboardEvent) => {
      if (e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1) {
        e.stopPropagation();
      }
    };
    textarea.addEventListener('keydown', ctrlStopPropHandler, true);

    const vkModifierHandler = (e: KeyboardEvent) => {
      if (vkCtrlRef.current && !e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1) {
        const code = e.key.toLowerCase().charCodeAt(0);
        if (code >= 0x61 && code <= 0x7a) {
          e.preventDefault();
          e.stopPropagation();
          vkCtrlRef.current = false;
          wt.input?.onData(String.fromCharCode(code - 0x60));
          textarea.value = '';
          return;
        }
      }
      if (vkAltRef.current && !e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1) {
        e.preventDefault();
        e.stopPropagation();
        vkAltRef.current = false;
        wt.input?.onData('\x1b' + e.key);
        textarea.value = '';
        return;
      }
    };
    textarea.addEventListener('keydown', vkModifierHandler, true);

    return () => {
      el.removeEventListener('scroll', onScroll);
      window.removeEventListener('keydown', windowCtrlCHandler, true);
      textarea.removeEventListener('keydown', ctrlStopPropHandler, true);
      textarea.removeEventListener('keydown', vkModifierHandler, true);
    };
  }, [termReadyRef.current, active]);

  const handleClick = useCallback(() => {
    if (active) sendResizeIfDifferent('click');
  }, [active, sendResizeIfDifferent]);

  // Register input handler with App for VirtualKeyboard routing
  const handleDataRef = useRef(handleData);
  handleDataRef.current = handleData;

  useEffect(() => {
    if (active) {
      onSetActiveInput((data: string) => handleDataRef.current(data));
      return () => onSetActiveInput(null);
    }
  }, [active, onSetActiveInput]);

  return (
    <div
      ref={containerRef}
      className={`terminal-view ${active ? 'terminal-active' : 'terminal-hidden'}`}
      style={{ '--term-font-size': `${fontSize}px` } as React.CSSProperties}
      onClick={handleClick}
    >
      <div className="terminal-container">
        <Terminal
          autoResize={true}
          minRenderInterval={100}
          onData={handleData}
          onResize={handleResize}
          onReady={handleReady}
          onTitle={active && onTitle ? (title: string) => {
            onTitle(sessionId, title);
            const t = transportRef.current;
            const cid = clientIdRef.current;
            if (t?.connected && cid) {
              t.send({ type: 'ClientSetTitle', sessionId, clientId: cid, title });
            }
          } : undefined}
        />
      </div>
    </div>
  );
}
