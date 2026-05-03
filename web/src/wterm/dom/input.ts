import type { WasmBridge } from "../core/index.js";

const NORMAL_KEYS: Record<string, string> = {
  ArrowUp: "\x1b[A",
  ArrowDown: "\x1b[B",
  ArrowRight: "\x1b[C",
  ArrowLeft: "\x1b[D",
  Home: "\x1b[H",
  End: "\x1b[F",
};

const APP_KEYS: Record<string, string> = {
  ArrowUp: "\x1bOA",
  ArrowDown: "\x1bOB",
  ArrowRight: "\x1bOC",
  ArrowLeft: "\x1bOD",
  Home: "\x1bOH",
  End: "\x1bOF",
};

const FIXED_KEYS: Record<string, string> = {
  Enter: "\r",
  Backspace: "\x7f",
  Tab: "\t",
  Escape: "\x1b",
  Insert: "\x1b[2~",
  Delete: "\x1b[3~",
  PageUp: "\x1b[5~",
  PageDown: "\x1b[6~",
  F1: "\x1bOP",
  F2: "\x1bOQ",
  F3: "\x1bOR",
  F4: "\x1bOS",
  F5: "\x1b[15~",
  F6: "\x1b[17~",
  F7: "\x1b[18~",
  F8: "\x1b[19~",
  F9: "\x1b[20~",
  F10: "\x1b[21~",
  F11: "\x1b[23~",
  F12: "\x1b[24~",
};

export class InputHandler {
  private element: HTMLElement;
  textarea: HTMLTextAreaElement;
  onData: (data: string) => void;
  private getBridge: () => WasmBridge | null;
  private composing = false;
  private _rowHeight = 19;
  private _charWidth = 9;

  private _onKeyDown: (e: KeyboardEvent) => void;
  private _onPaste: (e: ClipboardEvent) => void;
  private _onCompositionStart: () => void;
  private _onCompositionEnd: (e: CompositionEvent) => void;
  private _onInput: () => void;
  private _onFocus: () => void;
  private _onBlur: () => void;

  constructor(
    element: HTMLElement,
    onData: (data: string) => void,
    getBridge: () => WasmBridge | null,
  ) {
    this.element = element;
    this.onData = onData;
    this.getBridge = getBridge;

    this.textarea = document.createElement("textarea");
    this.textarea.setAttribute("autocapitalize", "off");
    this.textarea.setAttribute("autocomplete", "off");
    this.textarea.setAttribute("autocorrect", "off");
    this.textarea.setAttribute("spellcheck", "false");
    this.textarea.setAttribute("enterkeyhint", "send");
    this.textarea.setAttribute("tabindex", "0");
    this.textarea.setAttribute("aria-hidden", "true");
    // Position at cursor location (updated by positionAtCursor).
    // Unlike off-screen positioning (left:-9999px), this keeps the textarea
    // in the visible area so: (1) IME candidate window appears at the cursor,
    // (2) the browser doesn't scroll the terminal to show the textarea.
    const s = this.textarea.style;
    s.position = "absolute";
    s.left = "0";
    s.top = "0";
    s.width = "0";
    s.height = "0";
    s.opacity = "0";
    s.overflow = "hidden";
    s.border = "0";
    s.padding = "0";
    s.margin = "0";
    s.outline = "none";
    s.resize = "none";
    s.pointerEvents = "none";
    s.caretColor = "transparent";
    s.color = "transparent";
    s.background = "transparent";
    element.appendChild(this.textarea);

    this._onKeyDown = this.handleKeyDown.bind(this);
    this._onPaste = this.handlePaste.bind(this);
    this._onCompositionStart = this.handleCompositionStart.bind(this);
    this._onCompositionEnd = this.handleCompositionEnd.bind(this);
    this._onInput = this.handleInput.bind(this);
    this._onFocus = () => this.element.classList.add("focused");
    this._onBlur = () => this.element.classList.remove("focused");

    this.textarea.addEventListener("keydown", this._onKeyDown);
    this.textarea.addEventListener("paste", this._onPaste as EventListener);
    this.textarea.addEventListener(
      "compositionstart",
      this._onCompositionStart,
    );
    this.textarea.addEventListener(
      "compositionend",
      this._onCompositionEnd as EventListener,
    );
    this.textarea.addEventListener("input", this._onInput);
    this.textarea.addEventListener("focus", this._onFocus);
    this.textarea.addEventListener("blur", this._onBlur);
  }

  focus(): void {
    this.textarea.focus({ preventScroll: true });
  }

  /** Update layout metrics used for mathematical cursor positioning.
   *  Called from the parent WTerm when row height or char width changes. */
  setLayoutMetrics(rowHeight: number, charWidth: number): void {
    this._rowHeight = rowHeight;
    this._charWidth = charWidth;
  }

  destroy(): void {
    this.textarea.removeEventListener("keydown", this._onKeyDown);
    this.textarea.removeEventListener("paste", this._onPaste as EventListener);
    this.textarea.removeEventListener(
      "compositionstart",
      this._onCompositionStart,
    );
    this.textarea.removeEventListener(
      "compositionend",
      this._onCompositionEnd as EventListener,
    );
    this.textarea.removeEventListener("input", this._onInput);
    this.textarea.removeEventListener("focus", this._onFocus);
    this.textarea.removeEventListener("blur", this._onBlur);
    this.element.classList.remove("focused");
    this.textarea.remove();
  }

  /** Position the textarea at the cursor using mathematical calculation.
   *  Avoids getBoundingClientRect() which forces synchronous layout (reflow)
   *  after DOM mutations in the render loop. Instead, computes the position
   *  from known row/col + cached metrics, which is a pure arithmetic operation.
   *  On mobile, the textarea is clamped to the visible area so the browser
   *  doesn't scroll to show a textarea that's behind the virtual keyboard.
   *  Accepts scrollTop as parameter to avoid reading element.scrollTop which
   *  can force synchronous layout recalculation. */
  positionAtCursor(
    row: number,
    col: number,
    scrollbackCount: number,
    clientHeight: number,
    scrollTop: number,
  ): void {
    const left = col * this._charWidth;
    // position: absolute inside a scroll container scrolls with the content,
    // so top must be in scroll-content coordinates (not viewport coordinates).
    // Subtracting scrollTop here would cause a double offset: the browser
    // applies the scroll offset when rendering, and we'd have subtracted it
    // too, placing the textarea far from the actual cursor.
    let top = (scrollbackCount + row) * this._rowHeight;

    // Clamp to visible area (in scroll-content coordinates) to prevent mobile
    // browsers from scrolling the terminal to show a textarea that's behind
    // the virtual keyboard.
    const maxTop = scrollTop + clientHeight - 1;
    const minTop = scrollTop;
    if (top > maxTop) top = maxTop;
    if (top < minTop) top = minTop;

    this.textarea.style.left = `${left}px`;
    this.textarea.style.top = `${top}px`;
  }

  private handleKeyDown(e: KeyboardEvent): void {
    if (this.composing) return;

    // Cmd+C: browser native copy
    if (e.metaKey && !e.ctrlKey && !e.shiftKey && e.key === "c") {
      return;
    }
    // Cmd+Shift+C: send SIGINT
    if (e.metaKey && !e.ctrlKey && e.shiftKey && e.key === "C") {
      e.preventDefault();
      e.stopPropagation();
      this.onData("\x03");
      return;
    }
    // Cmd+V: browser native paste
    if (e.metaKey && !e.ctrlKey && e.key === "v") {
      this.textarea.focus();
      return;
    }
    // Cmd+K: clear screen (macOS convention, maps to Ctrl+L not Ctrl+K)
    if (e.metaKey && !e.ctrlKey && e.key === "k") {
      e.preventDefault();
      e.stopPropagation();
      this.onData("\x0c");
      return;
    }

    // For single printable chars without Ctrl/Meta, let the browser handle
    // the key event. This allows IME composition to start (the first keydown
    // of an IME session has the actual keyCode, not 229). The typed character
    // will be processed by handleInput instead of keyToSequence.
    if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) {
      return;
    }

    e.preventDefault();
    e.stopPropagation();
    const seq = this.keyToSequence(e);
    if (seq) this.onData(seq);
  }

  private handlePaste(e: ClipboardEvent): void {
    e.preventDefault();
    const text = e.clipboardData?.getData("text");
    if (!text) return;

    const bridge = this.getBridge();
    if (bridge && bridge.bracketedPaste()) {
      const safe = text.replace(/\x1b/g, "");
      this.onData("\x1b[200~" + safe + "\x1b[201~");
    } else {
      this.onData(text);
    }
  }

  private handleCompositionStart(): void {
    this.composing = true;
  }

  private handleCompositionEnd(e: CompositionEvent): void {
    this.composing = false;
    if (e.data) this.onData(e.data);
    this.textarea.value = "";
  }

  private handleInput(): void {
    if (this.composing) return;
    const value = this.textarea.value;
    if (value) {
      this.onData(value);
      this.textarea.value = "";
    }
  }

  private keyToSequence(e: KeyboardEvent): string | null {
    // On macOS, Cmd maps to Ctrl for terminal control characters
    const ctrl = e.ctrlKey || (e.metaKey && !e.ctrlKey);

    if (ctrl && !e.altKey) {
      if (e.key.length === 1) {
        const code = e.key.toLowerCase().charCodeAt(0);
        if (code >= 97 && code <= 122) return String.fromCharCode(code - 96);
      }
      if (e.key === "[") return "\x1b";
      if (e.key === "\\") return "\x1c";
      if (e.key === "]") return "\x1d";
      if (e.key === "^") return "\x1e";
      if (e.key === "_") return "\x1f";
      // Cmd/Ctrl+Backspace/Delete → Ctrl+U (kill line)
      if (e.key === "Backspace" || e.key === "Delete") return "\x15";
    }

    // Cmd+Arrow → Home/End/PageUp/PageDown (macOS line/page movement)
    if (e.metaKey && !e.ctrlKey && !e.altKey) {
      if (e.key === "ArrowLeft") return "\x1b[H";
      if (e.key === "ArrowRight") return "\x1b[F";
      if (e.key === "ArrowUp") return "\x1b[5~";
      if (e.key === "ArrowDown") return "\x1b[6~";
    }

    if (e.key === "Enter" && e.shiftKey) return "\x1b[13;2u";
    if (e.key === "Tab" && e.shiftKey) return "\x1b[Z";

    const fixed = FIXED_KEYS[e.key];
    if (fixed) return e.altKey ? "\x1b" + fixed : fixed;

    const bridge = this.getBridge();
    const appMode = bridge && bridge.cursorKeysApp();
    const navMap = appMode ? APP_KEYS : NORMAL_KEYS;
    const nav = navMap[e.key];
    if (nav) return e.altKey ? "\x1b" + nav : nav;

    if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) {
      return e.altKey ? "\x1b" + e.key : e.key;
    }

    return null;
  }
}
