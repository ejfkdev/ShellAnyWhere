/// Session tabs: desktop tab bar + mobile compact header + session picker.
/// Handles session switching, attaching, detaching, and zoom controls.
/// Supports editable tab names, drag-to-reorder, and tooltip on hover.

import { useState, useEffect, useRef, useCallback } from 'react';
import { SessionInfo } from '../protocol/frame';
import { loadTabOrder, saveTabOrder } from '../store';

interface Props {
  sessions: SessionInfo[];
  attached: string[];
  activeId: string | null;
  connected: boolean;
  fontSize: number;
  shellTitles: Record<string, string>;
  onSwitch: (sessionId: string) => void;
  onDetach: (sessionId: string) => void;
  onAttach: (sessionId: string) => void;
  onFontSizeChange: (size: number) => void;
  onRefreshSessions: () => void;
  onSetTitle: (sessionId: string, title: string) => void;
  attaching: boolean;
  autoOpenPicker?: boolean;
  onPickerOpened?: () => void;
}

/// Format a unix timestamp as "MM/DD HH:MM" + relative time.
function formatTime(ts: number): string {
  const dt = new Date(ts * 1000);
  const date = dt.toLocaleDateString(undefined, { month: '2-digit', day: '2-digit' });
  const time = dt.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  const ago = Math.floor(Date.now() / 1000 - ts);
  if (ago < 0) return `${date} ${time}`;
  if (ago < 60) return `${date} ${time} (just now)`;
  if (ago < 3600) return `${date} ${time} (${Math.floor(ago / 60)}m ago)`;
  if (ago < 86400) return `${date} ${time} (${Math.floor(ago / 3600)}h ago)`;
  return `${date} ${time} (${Math.floor(ago / 86400)}d ago)`;
}

/// Abbreviate a cwd path like ~/D/p/MyProject from /username/Documents/project/MyProject
function abbreviateCwd(cwd: string, username: string): string {
  if (!cwd) return '';
  const sep = cwd.includes('\\') ? '\\' : '/';
  const parts = cwd.split(sep).filter(Boolean);

  let homeLen = 0;
  if (parts.length >= 2) {
    if ((parts[0] === 'Users' || parts[0] === 'home') && parts[1] === username) {
      homeLen = 2;
    } else if (parts[0].length === 2 && parts[0][1] === ':' && parts[1] === 'Users' && parts[2] === username) {
      homeLen = 3;
    }
  }

  if (homeLen > 0) {
    const afterHome = parts.slice(homeLen);
    if (afterHome.length === 0) return '~';
    const abbreviated = afterHome.slice(0, -1).map(s => s.charAt(0));
    return '~/' + [...abbreviated, afterHome[afterHome.length - 1]].join('/');
  }

  if (parts.length <= 1) return cwd;
  const abbreviated = parts.slice(0, -1).map(s => s.charAt(0));
  return [...abbreviated, parts[parts.length - 1]].join('/');
}

function buildTooltip(info: SessionInfo | undefined, sessionId: string): string {
  if (!info) return sessionId;
  const lines = [
    `User: ${info.username}@${info.hostname}`,
    `CWD: ${info.cwd}`,
    `Shell: ${info.shell}`,
    `Size: ${info.cols}×${info.rows}`,
    `Activity: ${formatTime(info.lastActivityAt)}`,
  ];
  if (info.terminalProgram) lines.push(`Terminal: ${info.terminalProgram}`);
  return lines.join('\n');
}

/// Sort attached sessions using persisted tab order.
function sortAttached(attached: string[], order: string[]): string[] {
  if (order.length === 0) return attached;
  const index = new Map(order.map((id, i) => [id, i]));
  return [...attached].sort((a, b) => {
    const ai = index.get(a) ?? Infinity;
    const bi = index.get(b) ?? Infinity;
    return ai - bi;
  });
}

export function SessionTabs({
  sessions, attached, activeId, connected, fontSize, shellTitles,
  onSwitch, onDetach, onAttach, onFontSizeChange, onRefreshSessions, onSetTitle,
  attaching, autoOpenPicker, onPickerOpened,
}: Props) {
  const [pickerOpen, setPickerOpen] = useState(false);
  const attachedIds = new Set(attached);
  const activeSession = attached.find(a => a === activeId);

  // Tab names (in-memory only, not persisted) and order (persisted)
  const [tabNames, setTabNames] = useState<Record<string, string>>({});
  const [tabOrder, setTabOrder] = useState<string[]>(loadTabOrder);
  const sortedAttached = sortAttached(attached, tabOrder);

  // Editing state
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editValue, setEditValue] = useState('');
  const editRef = useRef<HTMLInputElement>(null);

  // Drag state
  const [dragId, setDragId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);

  // Expanded state (multi-row tab list)
  const [expanded, setExpanded] = useState(false);
  const [tabsOverflow, setTabsOverflow] = useState(false);
  const tabListRef = useRef<HTMLDivElement>(null);

  // Check if tabs overflow the tab-list container
  useEffect(() => {
    const checkOverflow = () => {
      const el = tabListRef.current;
      if (el) {
        setTabsOverflow(el.scrollWidth > el.clientWidth + 2);
      }
    };
    checkOverflow();
    // Re-check when tabs change or window resizes
    window.addEventListener('resize', checkOverflow);
    return () => window.removeEventListener('resize', checkOverflow);
  }, [sortedAttached.length, expanded]);

  // Auto-open picker when all tabs are closed
  useEffect(() => {
    if (autoOpenPicker && !pickerOpen) {
      onRefreshSessions();
      setPickerOpen(true);
      onPickerOpened?.();
    }
  }, [autoOpenPicker, pickerOpen, onRefreshSessions, onPickerOpened]);

  // Focus edit input when editing starts
  useEffect(() => {
    if (editingId && editRef.current) {
      editRef.current.focus();
      editRef.current.select();
    }
  }, [editingId]);

  const openPicker = () => {
    onRefreshSessions();
    setPickerOpen(true);
  };

  // Tab label: user rename > server title > wterm onTitle > abbreviated cwd
  const getTabLabel = useCallback((info: SessionInfo | undefined, sessionId: string): string => {
    const customName = tabNames[sessionId];
    if (customName) return customName;
    if (info?.title) return info.title;
    const shellTitle = shellTitles[sessionId];
    if (shellTitle) return shellTitle;
    if (!info) return sessionId.slice(0, 12);
    return abbreviateCwd(info.cwd, info.username) || `${info.username}@${info.hostname}`;
  }, [tabNames, shellTitles]);

  const startEdit = (sessionId: string) => {
    setEditingId(sessionId);
    const info = sessions.find(s => s.sessionId === sessionId);
    setEditValue(tabNames[sessionId] || info?.title || '');
  };

  const commitEdit = () => {
    if (editingId) {
      const trimmed = editValue.trim();
      setTabNames(prev => ({ ...prev, [editingId]: trimmed }));
      if (trimmed) {
        onSetTitle(editingId, trimmed);
      }
    }
    setEditingId(null);
    setEditValue('');
  };

  const cancelEdit = () => {
    setEditingId(null);
    setEditValue('');
  };

  // Drag handlers (desktop)
  const handleDragStart = (e: React.DragEvent, sessionId: string) => {
    setDragId(sessionId);
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', sessionId);
  };

  const handleDragOver = (e: React.DragEvent, sessionId: string) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    if (dragId && sessionId !== dragId) {
      setDragOverId(sessionId);
    }
  };

  const handleDrop = (e: React.DragEvent, targetId: string) => {
    e.preventDefault();
    if (!dragId || dragId === targetId) { setDragId(null); setDragOverId(null); return; }
    const ids = sortedAttached;
    const fromIdx = ids.indexOf(dragId);
    const toIdx = ids.indexOf(targetId);
    if (fromIdx < 0 || toIdx < 0) { setDragId(null); setDragOverId(null); return; }
    const newIds = [...ids];
    newIds.splice(fromIdx, 1);
    newIds.splice(toIdx, 0, dragId);
    saveTabOrder(newIds);
    setTabOrder(newIds);
    setDragId(null);
    setDragOverId(null);
  };

  const handleDragEnd = () => {
    setDragId(null);
    setDragOverId(null);
  };

  // Long press to start editing on mobile
  const longPressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const longPressFired = useRef(false);

  const handleTouchStartEdit = (sessionId: string) => {
    longPressFired.current = false;
    longPressTimer.current = setTimeout(() => {
      longPressFired.current = true;
      startEdit(sessionId);
    }, 600);
  };

  const handleTouchCancelEdit = () => {
    if (longPressTimer.current) {
      clearTimeout(longPressTimer.current);
      longPressTimer.current = null;
    }
  };

  // Force re-render every 30s so "ago" timestamps stay current while picker is open
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!pickerOpen) return;
    const id = setInterval(() => setTick(t => t + 1), 30000);
    return () => clearInterval(id);
  }, [pickerOpen]);

  // Sort sessions by startedAt descending (newest first)
  const sortedSessions = [...sessions].sort((a, b) => b.startedAt - a.startedAt);
  void tick; // used to trigger re-render for time display

  // Mobile header label
  const mobileLabel = activeSession ? (() => {
    const info = sessions.find(s => s.sessionId === activeId);
    if (!info) return 'Session';
    const customName = tabNames[activeId!];
    if (customName) return customName;
    if (info.title) return info.title;
    const shellTitle = activeId ? shellTitles[activeId] : undefined;
    if (shellTitle) return shellTitle;
    return abbreviateCwd(info.cwd, info.username) || `${info.username}@${info.hostname}`;
  })() : 'Sessions';

  return (
    <>
      <div className={`session-tabs ${expanded ? 'session-tabs-expanded' : ''}`}>
        {/* Mobile: menu button / edit input */}
        {editingId && editingId === activeId ? (
          <div className="tab-menu-btn tab-menu-editing">
            <span className={`status-dot ${connected ? 'connected' : 'disconnected'}`}></span>
            <input
              ref={editRef}
              className="tab-edit-input tab-menu-edit-input"
              value={editValue}
              placeholder="Enter title..."
              onChange={(e) => setEditValue(e.target.value)}
              onBlur={commitEdit}
              onKeyDown={(e) => {
                if (e.key === 'Enter') commitEdit();
                if (e.key === 'Escape') cancelEdit();
                e.stopPropagation();
              }}
            />
          </div>
        ) : (
          <button className="tab-menu-btn" onClick={() => { if (!longPressFired.current) openPicker(); longPressFired.current = false; }} title="Sessions"
            onTouchStart={() => handleTouchStartEdit(activeId!)}
            onTouchMove={handleTouchCancelEdit}
            onTouchEnd={handleTouchCancelEdit}
          >
            <span className={`status-dot ${connected ? 'connected' : 'disconnected'}`}></span>
            <span className="tab-menu-label">{mobileLabel}</span>
            {activeSession && <span></span>}
          </button>
        )}

        {/* Desktop: tab list */}
        <div
          ref={tabListRef}
          className={`tab-list ${expanded ? 'tab-list-expanded' : ''}`}
        >
          {sortedAttached.map(a => {
            const info = sessions.find(s => s.sessionId === a);
            const isEditing = editingId === a;
            const label = getTabLabel(info, a);
            const tooltip = buildTooltip(info, a);
            const customName = tabNames[a];

            return (
            <button
              key={a}
              className={`tab-item ${a === activeId ? 'active' : ''} ${dragId === a ? 'tab-dragging' : ''} ${dragOverId === a ? 'tab-drag-over' : ''}`}
              onClick={() => { if (!isEditing) onSwitch(a); }}
              onDoubleClick={() => startEdit(a)}
              draggable={!isEditing}
              onDragStart={(e) => handleDragStart(e, a)}
              onDragOver={(e) => handleDragOver(e, a)}
              onDrop={(e) => handleDrop(e, a)}
              onDragEnd={handleDragEnd}
              title={isEditing ? undefined : (customName ? customName : tooltip)}
            >
              {isEditing ? (
                <input
                  ref={editRef}
                  className="tab-edit-input"
                  value={editValue}
                  placeholder="Enter title..."
                  onChange={(e) => setEditValue(e.target.value)}
                  onBlur={commitEdit}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') commitEdit();
                    if (e.key === 'Escape') cancelEdit();
                    e.stopPropagation();
                  }}
                  onClick={(e) => e.stopPropagation()}
                />
              ) : (
                <>
                  <span className={`status-dot ${connected ? 'connected' : 'disconnected'}`}></span>
                  <span className="tab-name">{label}</span>
                </>
              )}
              <span></span>
              <span
                className="tab-close"
                onClick={(e) => { e.stopPropagation(); onDetach(a); }}
                title="Detach"
              >×</span>
            </button>
            );
          })}
        </div>

        {/* Desktop: expand/collapse — only show when tabs overflow */}
        {tabsOverflow && (
        <button
          className={`tab-expand ${expanded ? 'tab-expand-active' : ''}`}
          onClick={() => setExpanded(!expanded)}
          title={expanded ? 'Collapse tabs' : 'Show all tabs'}
        >▾</button>
        )}
        <button className="tab-add" onClick={openPicker} title="Attach session">+</button>

        {/* Zoom controls */}
        <div className="tab-zoom">
          <button
            className="zoom-btn"
            onClick={() => onFontSizeChange(Math.max(8, fontSize - 1))}
            title="Zoom out"
          >−</button>
          <span className="zoom-label">{fontSize}</span>
          <button
            className="zoom-btn"
            onClick={() => onFontSizeChange(Math.min(32, fontSize + 1))}
            title="Zoom in"
          >+</button>
        </div>
      </div>

      {/* Session picker overlay */}
      {pickerOpen && (
        <div className="picker-overlay" onClick={() => setPickerOpen(false)}>
          <div className="picker-panel" onClick={e => e.stopPropagation()}>
            <div className="picker-header">
              <h3>Sessions</h3>
              <button className="picker-close" onClick={() => setPickerOpen(false)}>×</button>
            </div>

            <div className="picker-list">
              {sortedSessions.map(s => {
                const isAttached = attachedIds.has(s.sessionId);
                const isActive = s.sessionId === activeId;
                const customName = tabNames[s.sessionId];
                return (
                  <div
                    key={s.sessionId}
                    className={`picker-card ${isActive ? 'picker-active' : ''} ${!isAttached ? 'picker-available' : ''}`}
                    onClick={() => {
                      if (isAttached) { onSwitch(s.sessionId); setPickerOpen(false); }
                      else if (!attaching) { setPickerOpen(false); onAttach(s.sessionId); }
                    }}
                  >
                    <div className="picker-card-row">
                      <span className="picker-card-id">{customName || s.title || shellTitles[s.sessionId] || s.sessionId}</span>
                      {isAttached && (
                        <button
                          className="picker-detach-btn"
                          onClick={(e) => { e.stopPropagation(); onDetach(s.sessionId); }}
                        >Detach</button>
                      )}
                    </div>
                    <div className="picker-card-meta">{s.username}@{s.hostname} · {s.shell} · {s.cols}×{s.rows}</div>
                    {s.cwd && <div className="picker-card-meta">CWD: {abbreviateCwd(s.cwd, s.username)}</div>}
                    <div className="picker-card-detail">
                      <span>Created: {formatTime(s.startedAt)}</span>
                      <span>Last: {formatTime(s.lastActivityAt)}</span>
                    </div>
                    {s.terminalProgram && (
                      <div className="picker-card-detail">
                        <span>Terminal: {s.terminalProgram}</span>
                      </div>
                    )}
                    {attaching && !isAttached && <div className="picker-loading">Connecting...</div>}
                  </div>
                );
              })}
              {sortedSessions.length === 0 && (
                <div className="picker-empty">No sessions available</div>
              )}
            </div>
          </div>
        </div>
      )}
    </>
  );
}
