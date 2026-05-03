/// Main application: control connection + per-session terminal tabs.
/// App.tsx owns a "control" transport for session list and lifecycle events.
/// Each TerminalView owns its own transport for TerminalIO I/O.

import { useState, useCallback, useRef, useEffect, lazy, Suspense } from 'react';
import { StoredConfig, loadConfig, saveConfig } from './store';
import { LoginDialog } from './components/LoginDialog';
import { SessionTabs } from './components/SessionTabs';

import { Frame, SessionInfo } from './protocol/frame';
import {
  type Transport,
  type TransportType,
  tryConnect,
  authenticateTransport,
  waitForFrame,
  isSessionList,
  isSessionUpdate,
  isSessionRegister,
  isSessionClose,
  isDesktopNotification,
} from './protocol/transport-utils';
import { VirtualKeyboard } from './components/VirtualKeyboard';

const TerminalView = lazy(() => import('./components/TerminalView').then(m => ({ default: m.TerminalView })));

type AppStage = 'login' | 'main';

export function App() {
  const saved = loadConfig();
  const [config, setConfig] = useState<StoredConfig>(saved);
  const [stage, setStage] = useState<AppStage>(saved.serverToken ? 'main' : 'login');
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [attached, setAttached] = useState<string[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [fontSize, setFontSize] = useState(saved.fontSize || 14);
  const [error, setError] = useState('');
  const [connecting, setConnecting] = useState(false);
  const [showPicker, setShowPicker] = useState(false);
  const [connected, setConnected] = useState(false);

  // Virtual keyboard state
  const [vkCtrl, setVkCtrl] = useState(false);
  const [vkAlt, setVkAlt] = useState(false);
  const activeInputRef = useRef<((data: string) => void) | null>(null);

  // Control transport (session list + events)
  const controlTransportRef = useRef<Transport | null>(null);
  const controlInterceptors = useRef<((frame: Frame) => boolean)[]>([]);
  const lastTransportTypeRef = useRef<TransportType>('webrtc');
  const reconnectingRef = useRef(false);
  const everConnectedRef = useRef(false);
  const reconnectWakeRef = useRef<(() => void) | null>(null);
  const connectedRef = useRef(false);
  const configRef = useRef(config);
  configRef.current = config;

  const mainRef = useRef<HTMLDivElement>(null);

  // Terminal titles from wterm OSC 0/2
  const [shellTitles, setShellTitles] = useState<Record<string, string>>({});

  // Adapt layout to visual viewport on mobile
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;

    const update = () => {
      const appHeight = vv.height;
      document.documentElement.style.setProperty('--app-height', `${Math.max(appHeight, 100)}px`);
      document.body.style.setProperty('--app-height', `${Math.max(appHeight, 100)}px`);
    };
    update();
    vv.addEventListener('resize', update);
    vv.addEventListener('scroll', update);

    const preventScroll = (e: TouchEvent) => {
      const target = e.target as HTMLElement;
      if (target.closest('.terminal-container, input, textarea, .picker-panel, .picker-list')) return;
      let node: Node | null = target;
      while (node) {
        if (node instanceof HTMLElement && node.classList.contains('eruda-container')) return;
        const p: Node | null = node.parentNode ?? (node instanceof ShadowRoot ? node.host : null);
        node = p;
      }
      e.preventDefault();
    };
    document.addEventListener('touchmove', preventScroll, { passive: false });

    return () => {
      vv.removeEventListener('resize', update);
      vv.removeEventListener('scroll', update);
      document.removeEventListener('touchmove', preventScroll);
    };
  }, []);

  // ── Control connection ──

  const setupControlDispatch = useCallback((t: Transport) => {
    t.onFrame((frame: Frame) => {
      for (const interceptor of [...controlInterceptors.current]) {
        if (interceptor(frame)) return;
      }

      if (isSessionUpdate(frame)) {
        const s = frame.session;
        setSessions(prev => prev.map(x => x.sessionId === s.sessionId ? s : x));
      } else if (isSessionRegister(frame)) {
        const s = frame.session;
        setSessions(prev => prev.some(x => x.sessionId === s.sessionId) ? prev : [...prev, s]);
      } else if (isSessionClose(frame)) {
        const sid = frame.sessionId;
        setSessions(prev => prev.filter(x => x.sessionId !== sid));
        setAttached(prev => prev.filter(x => x !== sid));
        setActiveId(prev => prev === sid ? (prev && attached.length > 1 ? attached.find(a => a !== sid) || null : null) : prev);
      } else if (isDesktopNotification(frame)) {
        const { title, body } = frame;
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
      setConnected(false);
      if (everConnectedRef.current) {
        setError('Connection lost — reconnecting...');
        startControlReconnect();
      }
    });
  }, []);

  const connectControl = useCallback(async (serverToken: string | undefined): Promise<SessionInfo[] | null> => {
    const url = window.location.origin;
    const preferred = lastTransportTypeRef.current;
    const order: TransportType[] = preferred === 'ws' ? ['ws', 'webrtc'] : ['webrtc', 'ws'];
    const result = await tryConnect(url, order);
    if (!result) return null;

    const t = result.transport;
    lastTransportTypeRef.current = result.used;
    controlInterceptors.current = [];

    // Register frame dispatch BEFORE auth so waitForFrame interceptors work
    setupControlDispatch(t);

    const authOk = await authenticateTransport(t, serverToken, controlInterceptors);
    if (!authOk) return null;

    controlTransportRef.current = t;

    t.send({ type: 'SessionList', sessions: [] });
    const listFrame = await waitForFrame(controlInterceptors, isSessionList, 10000);
    return listFrame.sessions;
  }, [setupControlDispatch]);

  // Control connection reconnect with two-phase backoff
  const startControlReconnect = useCallback(async () => {
    if (reconnectingRef.current) return;
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

    while (reconnectingRef.current) {
      if (attempt > 0) {
        const delayMs = attempt < FAST_ATTEMPTS
          ? FAST_MIN + Math.random() * (FAST_MAX - FAST_MIN)
          : SLOW_MIN + Math.random() * (SLOW_MAX - SLOW_MIN);
        console.log(`[Control] Reconnect attempt ${attempt + 1}, waiting ${Math.round(delayMs)}ms`);
        await interruptibleDelay(delayMs);
        reconnectWakeRef.current = null;
      }

      try {
        const serverToken = configRef.current.serverToken;
        const sessions = await connectControl(serverToken);
        if (sessions !== null) {
          setSessions(sessions);
          setConnected(true);
          everConnectedRef.current = true;
          setError('');
          reconnectingRef.current = false;
          console.log('[Control] Reconnected');
          return;
        }
      } catch {}
      attempt++;
    }
    reconnectingRef.current = false;
  }, [connectControl]);

  // ── Login / auto-connect ──

  const handleLogin = useCallback(async (cfg: StoredConfig) => {
    setError('');
    setConfig(cfg);
    saveConfig(cfg);
    setConnecting(true);

    const sessions = await connectControl(cfg.serverToken);
    if (sessions) {
      setConnected(true);
      everConnectedRef.current = true;
      reconnectingRef.current = false;
      setSessions(sessions);
      setStage('main');

      if (sessions.length === 1) {
        setTimeout(() => openSession(sessions[0].sessionId), 100);
      } else if (sessions.length > 1) {
        setShowPicker(true);
      }
    } else {
      setError('Connection failed — check token and ensure the server certificate is trusted');
    }
    setConnecting(false);
  }, [connectControl]);

  // Auto-connect on mount
  useEffect(() => {
    if (!saved.serverToken) return;

    let cancelled = false;
    (async () => {
      setConnecting(true);
      const sessions = await connectControl(saved.serverToken);
      if (cancelled) return;
      if (sessions) {
        setConnected(true);
        everConnectedRef.current = true;
        reconnectingRef.current = false;
        setSessions(sessions);
        setStage('main');

        if (sessions.length === 1) {
          setTimeout(() => openSession(sessions[0].sessionId), 100);
        } else if (sessions.length > 1) {
          setShowPicker(true);
        }
      } else {
        setStage('login');
        setError('');
      }
      setConnecting(false);
    })();

    return () => { cancelled = true; };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Session management ──

  const openSession = useCallback((sessionId: string) => {
    setAttached(prev => prev.includes(sessionId) ? prev : [...prev, sessionId]);
    setActiveId(sessionId);
    setShowPicker(false);
  }, []);

  const closeSession = useCallback((sessionId: string) => {
    setAttached(prev => {
      const remaining = prev.filter(x => x !== sessionId);
      setActiveId(prevActive => {
        if (prevActive === sessionId) {
          return remaining.length > 0 ? remaining[0] : null;
        }
        return prevActive;
      });
      if (remaining.length === 0) {
        setShowPicker(true);
      }
      return remaining;
    });
    setShellTitles(prev => {
      if (!(sessionId in prev)) return prev;
      const next = { ...prev };
      delete next[sessionId];
      return next;
    });
  }, []);

  const handleTerminalAttached = useCallback((sessionId: string, _clientId: string) => {
    // TerminalView successfully connected and attached
    console.log(`[App] Terminal attached: ${sessionId.slice(0,8)}`);
  }, []);

  const handleTerminalDetached = useCallback((sessionId: string) => {
    // TerminalView unmounted — remove from attached list
    closeSession(sessionId);
  }, [closeSession]);

  const handleTerminalError = useCallback((sessionId: string, err: string) => {
    console.error(`[App] Terminal error for ${sessionId.slice(0,8)}: ${err}`);
    setError(err);
  }, []);

  // Transport factory for TerminalView
  const transportFactory = useCallback(async () => {
    const url = window.location.origin;
    const preferred = lastTransportTypeRef.current;
    const order: TransportType[] = preferred === 'ws' ? ['ws', 'webrtc'] : ['webrtc', 'ws'];
    const result = await tryConnect(url, order);
    if (result) lastTransportTypeRef.current = result.used;
    return result;
  }, []);

  // ── Font size ──

  const handleFontSizeChange = useCallback((size: number) => {
    setFontSize(size);
    saveConfig({ fontSize: size });
  }, []);

  useEffect(() => {
    const el = mainRef.current;
    if (!el) return;
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail;
      if (detail?.fontSize) handleFontSizeChange(detail.fontSize);
    };
    el.addEventListener('term-zoom', handler);
    return () => el.removeEventListener('term-zoom', handler);
  }, [handleFontSizeChange]);

  // ── Title sync ──

  const handleTitle = useCallback((sessionId: string, title: string) => {
    setShellTitles(prev => {
      if (prev[sessionId] === title) return prev;
      return { ...prev, [sessionId]: title };
    });
  }, []);

  useEffect(() => {
    if (!activeId) {
      document.title = 'ShellAnyWhere';
      return;
    }
    const info = sessions.find(s => s.sessionId === activeId);
    const title = info?.title || shellTitles[activeId];
    document.title = title || 'ShellAnyWhere';
  }, [activeId, sessions, shellTitles]);

  // ── Session list refresh ──

  const refreshSessions = useCallback(() => {
    const t = controlTransportRef.current;
    if (!t?.connected) return;
    t.send({ type: 'SessionList', sessions: [] });
    waitForFrame(controlInterceptors, isSessionList, 10000)
      .then(f => setSessions(f.sessions))
      .catch(() => {});
  }, []);

  // ── Virtual keyboard ──

  const handleVkData = useCallback((data: string) => {
    activeInputRef.current?.(data);
  }, []);

  const handleSetActiveInput = useCallback((handler: ((data: string) => void) | null) => {
    activeInputRef.current = handler;
  }, []);

  // ── Visibility change for control connection ──

  const hiddenAtRef = useRef(0);

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

      if (!connectedRef.current && everConnectedRef.current) {
        startControlReconnect();
        return;
      }

      if (hiddenDuration > REFRESH_THRESHOLD && connectedRef.current) {
        const t = controlTransportRef.current;
        if (!t) return;

        const probeTimeout = setTimeout(() => {
          console.warn('[App] Control connection probe failed — reconnecting');
          if (controlTransportRef.current === t) {
            t.close();
            controlTransportRef.current = null;
          }
          connectedRef.current = false;
          setConnected(false);
          setError('Connection lost — reconnecting...');
          startControlReconnect();
        }, 5000);

        t.send({ type: 'SessionList', sessions: [] });
        waitForFrame(controlInterceptors, isSessionList, 10000)
          .then(f => { clearTimeout(probeTimeout); setSessions(f.sessions); })
          .catch(() => clearTimeout(probeTimeout));
      }
    };

    document.addEventListener('visibilitychange', handleVisibility);
    return () => document.removeEventListener('visibilitychange', handleVisibility);
  }, [startControlReconnect]);

  // bfcache restoration for control connection
  useEffect(() => {
    const handlePageshow = (e: PageTransitionEvent) => {
      if (!e.persisted) return;
      const t = controlTransportRef.current;
      if (t) t.close();
      controlTransportRef.current = null;
      connectedRef.current = false;
      setConnected(false);
      reconnectingRef.current = false;
      if (everConnectedRef.current) {
        setError('Connection lost — reconnecting...');
        startControlReconnect();
      }
    };
    window.addEventListener('pageshow', handlePageshow);
    return () => window.removeEventListener('pageshow', handlePageshow);
  }, [startControlReconnect]);

  // beforeunload: close control transport
  useEffect(() => {
    const handleBeforeUnload = () => {
      const t = controlTransportRef.current;
      if (t) { t.close(); controlTransportRef.current = null; }
    };
    window.addEventListener('beforeunload', handleBeforeUnload);
    return () => window.removeEventListener('beforeunload', handleBeforeUnload);
  }, []);

  // Auto-clear error
  useEffect(() => {
    if (!error) return;
    const timer = setTimeout(() => setError(''), 5000);
    return () => clearTimeout(timer);
  }, [error]);

  // ── Render ──

  return (
    <div className="app">
      {stage === 'login' && (
        <LoginDialog initialConfig={config} onLogin={handleLogin} error={error} connecting={connecting} />
      )}
      {stage === 'main' && (
        <div className="main-view" ref={mainRef}>
          <SessionTabs
            sessions={sessions}
            attached={attached}
            activeId={activeId}
            connected={connected}
            fontSize={fontSize}
            shellTitles={shellTitles}
            onSwitch={setActiveId}
            onDetach={closeSession}
            onAttach={openSession}
            onFontSizeChange={handleFontSizeChange}
            onRefreshSessions={refreshSessions}
            onSetTitle={() => {}}
            attaching={false}
            autoOpenPicker={showPicker}
            onPickerOpened={() => setShowPicker(false)}
          />
          <div className="terminal-area-wrapper">
            <div className="terminal-area">
            <Suspense fallback={null}>
            {attached.map(sid => (
              <TerminalView
                key={sid}
                sessionId={sid}
                active={sid === activeId}
                fontSize={fontSize}
                sessionInfo={sessions.find(s => s.sessionId === sid)}
                serverToken={config.serverToken || undefined}
                transportFactory={transportFactory}
                onAttached={handleTerminalAttached}
                onDetached={handleTerminalDetached}
                onError={handleTerminalError}
                onTitle={handleTitle}
                vkCtrl={vkCtrl}
                vkAlt={vkAlt}
                onSetActiveInput={handleSetActiveInput}
              />
            ))}
            </Suspense>
            </div>
          </div>
          <VirtualKeyboard
            onData={handleVkData}
            onModifierChange={(ctrl, alt) => { setVkCtrl(ctrl); setVkAlt(alt); }}
          />
          {error && <div className="global-error">{error}</div>}
        </div>
      )}
    </div>
  );
}
