/// Virtual keyboard for mobile/touch devices.
/// 2 rows × 7 columns on phone, 1 row on tablet.
/// Keyboard emoji button toggles 2 extra rows (content TBD).
/// Only visible on touch devices via CSS media query.

import { useState, useCallback, useEffect, useRef } from 'react';
import { loadEruda } from '../utils/eruda';
import { ctrlKeySequence } from '../utils/ctrl-keys';

interface Props {
  onData: (data: string) => void;
  onModifierChange?: (ctrl: boolean, alt: boolean) => void;
}

/// Key definition: label, the string to send, and optional behavior
interface VkDef {
  label: string;
  send: string;
  /** If true, this is a sticky modifier (toggle on/off) */
  modifier?: boolean;
  /** If true, this key toggles the extra rows panel instead of sending */
  toggle?: boolean;
  /** If true, this key triggers a special action (e.g. load vConsole) */
  special?: boolean;
}

const ESC = '\x1b';

/// Phone layout: 2×7 keyboard
/// Row 1: ESC ALT HOME ↑ END PGUP ⌨
/// Row 2: TAB CTRL ← ↓ → PGDN ENTER
const PHONE_ROWS: VkDef[][] = [
  [
    { label: 'ESC', send: ESC },
    { label: 'ALT', send: '', modifier: true },
    { label: 'HOME', send: `${ESC}[H` },
    { label: '↑', send: `${ESC}[A` },
    { label: 'END', send: `${ESC}[F` },
    { label: 'PGUP', send: `${ESC}[5~` },
    { label: '⌨', send: '', toggle: true },
  ],
  [
    { label: 'TAB', send: '\t' },
    { label: 'CTRL', send: '', modifier: true },
    { label: '←', send: `${ESC}[D` },
    { label: '↓', send: `${ESC}[B` },
    { label: '→', send: `${ESC}[C` },
    { label: 'PGDN', send: `${ESC}[6~` },
    { label: 'ENTER', send: '\r' },
  ],
];

/// Extra rows toggled by keyboard emoji button
const EXTRA_ROWS: VkDef[][] = [
  [
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: 'DBG', send: '', special: true },
  ],
  [
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
    { label: '', send: '' },
  ],
];

/// Tablet layout: 1 row — ESC TAB CTRL ALT HOME ↑ END PGUP ← ↓ → PGDN ENTER ⌨
const TABLET_ROW: VkDef[] = [
  { label: 'ESC', send: ESC },
  { label: 'TAB', send: '\t' },
  { label: 'CTRL', send: '', modifier: true },
  { label: 'ALT', send: '', modifier: true },
  { label: 'HOME', send: `${ESC}[H` },
  { label: '↑', send: `${ESC}[A` },
  { label: 'END', send: `${ESC}[F` },
  { label: 'PGUP', send: `${ESC}[5~` },
  { label: '←', send: `${ESC}[D` },
  { label: '↓', send: `${ESC}[B` },
  { label: '→', send: `${ESC}[C` },
  { label: 'PGDN', send: `${ESC}[6~` },
  { label: 'ENTER', send: '\r' },
  { label: '⌨', send: '', toggle: true },
];

export function VirtualKeyboard({ onData, onModifierChange }: Props) {
  const [ctrlActive, setCtrlActive] = useState(false);
  const [altActive, setAltActive] = useState(false);
  const [extraOpen, setExtraOpen] = useState(false);
  const [isTablet, setIsTablet] = useState(false);

  // Use refs to always read latest modifier state in handleKey,
  // avoiding stale closure issues with useCallback.
  const ctrlRef = useRef(false);
  const altRef = useRef(false);
  const onDataRef = useRef(onData);
  const onModRef = useRef(onModifierChange);
  onDataRef.current = onData;
  onModRef.current = onModifierChange;

  // Sync refs when state changes
  useEffect(() => { ctrlRef.current = ctrlActive; }, [ctrlActive]);
  useEffect(() => { altRef.current = altActive; }, [altActive]);

  // Detect tablet width
  useEffect(() => {
    const mq = window.matchMedia('(hover: none) and (pointer: coarse) and (min-width: 600px)');
    setIsTablet(mq.matches);
    const handler = (e: MediaQueryListEvent) => setIsTablet(e.matches);
    mq.addEventListener('change', handler);
    return () => mq.removeEventListener('change', handler);
  }, []);

  const handleKey = useCallback((def: VkDef) => {
    // Toggle extra rows
    if (def.toggle) {
      setExtraOpen(prev => !prev);
      return;
    }

    // Special keys (e.g. eruda devtools)
    if (def.special) {
      loadEruda();
      return;
    }

    // Toggle modifier keys
    if (def.modifier) {
      if (def.label === 'CTRL') {
        setCtrlActive(prev => {
          const next = !prev;
          ctrlRef.current = next;
          onModRef.current?.(next, altRef.current);
          return next;
        });
      } else if (def.label === 'ALT') {
        setAltActive(prev => {
          const next = !prev;
          altRef.current = next;
          onModRef.current?.(ctrlRef.current, next);
          return next;
        });
      }
      return;
    }

    // Non-modifier key: check active modifiers from refs (always up-to-date)
    const ctrl = ctrlRef.current;
    const alt = altRef.current;

    if (ctrl || alt) {
      const ctrlChar = ctrl ? ctrlKeySequence(def.send) : null;

      if (ctrlChar) {
        // CTRL (+ optional ALT) + key
        ctrlRef.current = false;
        altRef.current = false;
        setCtrlActive(false);
        setAltActive(false);
        onModRef.current?.(false, false);
        if (alt) {
          onDataRef.current(ESC + ctrlChar); // ALT + CTRL + key
        } else {
          onDataRef.current(ctrlChar); // CTRL + key
        }
      } else if (alt) {
        // ALT + key (no valid CTRL mapping)
        ctrlRef.current = false;
        altRef.current = false;
        setCtrlActive(false);
        setAltActive(false);
        onModRef.current?.(false, false);
        onDataRef.current(ESC + def.send);
      } else {
        // CTRL held but no valid mapping → send key as-is, clear modifiers
        ctrlRef.current = false;
        setCtrlActive(false);
        onModRef.current?.(false, false);
        onDataRef.current(def.send);
      }
      return;
    }

    // No modifiers active — just send the key
    onDataRef.current(def.send);
  }, []); // No deps — reads everything from refs

  const mainRows = isTablet ? [TABLET_ROW] : PHONE_ROWS;

  return (
    <div className="virtual-keyboard" onMouseDown={(e) => e.preventDefault()}>
      {extraOpen && EXTRA_ROWS.map((row, ri) => (
        <div className="vk-row vk-extra-row" key={`extra-${ri}`}>
          {row.map((def, ki) => {
            const isActive =
              (def.label === 'CTRL' && ctrlActive) ||
              (def.label === 'ALT' && altActive);
            return (
              <button
                key={ki}
                tabIndex={-1}
                className={`vk-key ${isActive ? 'active' : ''}`}
                onPointerDown={(e) => {
                  e.preventDefault();
                  if (def.send || def.modifier || def.toggle || def.special) handleKey(def);
                }}
              >
                {def.label}
              </button>
            );
          })}
        </div>
      ))}
      {mainRows.map((row, ri) => (
        <div className="vk-row" key={ri}>
          {(Array.isArray(row) ? row : row).map((def, ki) => {
            const isActive =
              (def.label === 'CTRL' && ctrlActive) ||
              (def.label === 'ALT' && altActive);
            return (
              <button
                key={ki}
                tabIndex={-1}
                className={`vk-key ${isActive ? 'active' : ''} ${def.toggle ? 'vk-toggle' : ''}`}
                onPointerDown={(e) => {
                  e.preventDefault();
                  handleKey(def);
                }}
              >
                {def.label}
              </button>
            );
          })}
        </div>
      ))}
    </div>
  );
}
