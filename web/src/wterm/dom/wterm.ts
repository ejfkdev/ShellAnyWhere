import { WasmBridge } from "../core/index.js";
import { Renderer } from "./renderer.js";
import { InputHandler } from "./input.js";
import type { DebugAdapter } from "./debug.js";

export interface WTermOptions {
  cols?: number;
  rows?: number;
  wasmUrl?: string;
  autoResize?: boolean;
  cursorBlink?: boolean;
  debug?: boolean;
  /** Minimum interval in ms between render frames triggered by write().
   *  0 = render every animation frame (default).
   *  Set to e.g. 200 to throttle renders to at most once per 200ms.
   *  User input (keystrokes) always triggers an immediate render.
   *  Resize renders are not throttled. */
  minRenderInterval?: number;
  onData?: (data: string) => void;
  onTitle?: (title: string) => void;
  onResize?: (cols: number, rows: number) => void;
}

export class WTerm {
  element: HTMLElement;
  cols: number;
  rows: number;
  bridge: WasmBridge | null = null;
  autoResize: boolean;
  debug: DebugAdapter | null = null;

  private wasmUrl: string | undefined;
  private _debugEnabled: boolean;
  private renderer: Renderer | null = null;
  input: InputHandler | null = null;
  private rafId: number | null = null;
  private resizeObserver: ResizeObserver | null = null;
  private _destroyed = false;
  private _shouldScrollToBottom = true;
  private _programmaticScroll = false;
  _rowHeight = 0;
  private _onClickFocus: () => void;
  private _onScroll!: () => void;

  onData: ((data: string) => void) | null;
  onTitle: ((title: string) => void) | null;
  onResize: ((cols: number, rows: number) => void) | null;

  // Cached layout values — NEVER read clientHeight/scrollHeight during
  // rendering as they force synchronous layout recalculation (layout thrashing).
  // clientHeight is updated only from ResizeObserver (contentRect is free).
  // maxScroll is computed from scrollback row count × row height.
  private _cachedClientHeight = 0;
  private _cachedMaxScroll = 0;
  private _lastScrollbackCount = 0;
  private _lastCursorRow = -1;
  private _lastCursorCol = -1;
  private _lastCursorScrollTop = -1;
  private _hadScrollback = false;
  private _charWidth = 9;
  private _cachedScrollTop = 0;

  // Render throttling
  private _minRenderInterval = 0;
  private _lastRenderTime = 0;
  private _throttleTimerId: ReturnType<typeof setTimeout> | null = null;
  private _inputPending = false;

  private _container: HTMLDivElement;

  constructor(element: HTMLElement, options: WTermOptions = {}) {
    this.element = element;
    this.wasmUrl = options.wasmUrl;
    this.cols = options.cols || 80;
    this.rows = options.rows || 24;
    this.autoResize = options.autoResize !== false;
    this._debugEnabled = options.debug ?? false;
    this._minRenderInterval = options.minRenderInterval ?? 0;

    this.onData = options.onData || null;
    this.onTitle = options.onTitle || null;
    this.onResize = options.onResize || null;

    this._container = document.createElement("div");
    this._container.className = "term-grid";
    this.element.appendChild(this._container);
    this.element.classList.add("wterm");
    if (options.cursorBlink) this.element.classList.add("cursor-blink");

    this._onClickFocus = () => {
      const sel = window.getSelection();
      if (!sel || sel.isCollapsed) {
        const saved = this.element.scrollTop;
        this.input?.focus();
        if (this.element.scrollTop !== saved) {
          this.element.scrollTop = saved;
        }
      }
    };
    this.element.addEventListener("click", this._onClickFocus);

    this._onScroll = () => {
      // Cache scrollTop from the scroll event — the browser has already
      // computed layout for this event, so reading scrollTop is cheap here.
      this._cachedScrollTop = this.element.scrollTop;

      if (!this.element.classList.contains("has-scrollback")) {
        // No scrollback — just ensure the flag stays true. No render needed
        // since all rows are always visible and there's nothing to scroll.
        this._shouldScrollToBottom = true;
      } else {
        // With scrollback — check if user scrolled to bottom.
        // If at bottom, set the flag so new output auto-scrolls.
        // If scrolled up, clear the flag so output doesn't force the user down.
        const rh = this._rowHeight || 19;
        const tolerance = rh;
        const atBottom =
          this._cachedMaxScroll - this._cachedScrollTop < tolerance;
        this._shouldScrollToBottom = atBottom;
        // Virtual scrolling: user scroll changes visible range, need to re-render.
        // Skip if this scroll was triggered by _scrollToBottom() to avoid
        // feedback loops (programmatic scroll → scroll event → render → scroll).
        if (!this._programmaticScroll) {
          this._scheduleRender();
        }
      }
    };
    this.element.addEventListener("scroll", this._onScroll, { passive: true });
  }

  async init(): Promise<this> {
    try {
      this.bridge = await WasmBridge.load(this.wasmUrl);
      if (this._destroyed) return this;
      this.bridge.init(this.cols, this.rows);

      if (this._debugEnabled) {
        const { DebugAdapter } = await import("./debug.js");
        this.debug = new DebugAdapter();
        this.debug.setBridge(this.bridge);
        (globalThis as Record<string, unknown>).__wterm = this;
      }

      this._setRowHeight();
      this._measureAndSetCharWidth();

      this.renderer = new Renderer(this._container);
      this.renderer.setup(this.cols, this.rows);

      this.input = new InputHandler(
        this.element,
        (data) => {
          // Keyboard input should always scroll to the bottom so the user can
          // see the command line and echo output, even if they scrolled up.
          this._shouldScrollToBottom = true;
          // Mark that user input is pending so the next render bypasses throttle.
          this._inputPending = true;
          if (this.onData) {
            this.onData(data);
          } else {
            this.write(data);
          }
        },
        () => this.bridge,
      );

      if (this.autoResize) {
        this._setupResizeObserver();
      } else {
        this._lockHeight();
      }

      this.input.focus();
      this._initialRender();

      // Re-measure after custom fonts load — initial measurement may use
      // a fallback font with different metrics, causing wrong column count.
      // ResizeObserver won't fire because the element width is determined
      // by its parent, not by its content, so font changes don't trigger it.
      document.fonts.ready.then(() => {
        if (this._destroyed || !this.autoResize) return;
        const measured = this._measureCharSize();
        if (!measured) return;
        const w = this.element.clientWidth;
        const h = this._cachedClientHeight;
        const cs = getComputedStyle(this.element);
        const pl = parseFloat(cs.paddingLeft) || 0;
        const pr = parseFloat(cs.paddingRight) || 0;
        const contentWidth = w - pl - pr;
        const newCols = Math.max(
          1,
          Math.floor(contentWidth / measured.charWidth),
        );
        const newRows = Math.max(1, Math.floor(h / measured.rowHeight));
        if (newCols !== this.cols || newRows !== this.rows) {
          this.resize(newCols, newRows);
        }
      });
    } catch (err) {
      this.destroy();
      throw new Error(
        `wterm: failed to initialize: ${err instanceof Error ? err.message : err}`,
      );
    }

    return this;
  }

  private _isScrolledToBottom(): boolean {
    // Use cached maxScroll and cached scrollTop instead of reading
    // scrollHeight/clientHeight/scrollTop which force synchronous layout.
    if (this._cachedClientHeight === 0) {
      // Not yet rendered, assume at bottom
      return true;
    }
    const tolerance = this._rowHeight || 19;
    return this._cachedMaxScroll - this._cachedScrollTop < tolerance;
  }

  write(data: string | Uint8Array): void {
    if (!this.bridge) return;
    if (this.debug) this.debug.traceWrite(data);
    // Auto-scroll to bottom when new data arrives, but only if the user
    // hasn't scrolled up (i.e. we were already at the bottom).
    // Uses cached scroll position from the scroll event handler to avoid
    // reading scrollTop which can force synchronous layout.
    if (this._isScrolledToBottom()) this._shouldScrollToBottom = true;
    if (typeof data === "string") {
      this.bridge.writeString(data);
    } else {
      this.bridge.writeRaw(data);
    }
    this._scheduleRender();
  }

  resize(cols: number, rows: number): void {
    if (!this.bridge) return;
    // When the grid shrinks (e.g. mobile keyboard appearing), always scroll
    // to the bottom so the user can see the command line behind the keyboard.
    // Otherwise, only upgrade _shouldScrollToBottom — never downgrade.
    if (rows < this.rows || this._isScrolledToBottom()) {
      this._shouldScrollToBottom = true;
    }
    this.cols = cols;
    this.rows = rows;
    this.bridge.resize(cols, rows);
    this.renderer?.setup(cols, rows);
    this._cancelPendingRender();
    if (this.rafId != null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    this._doRender();
    this._lastRenderTime = performance.now();
    if (this.onResize) this.onResize(cols, rows);
  }

  focus(): void {
    if (this.input) {
      this.input.focus();
    } else {
      this.element.focus();
    }
  }

  private _scheduleRender(): void {
    // If user input is pending, bypass throttle and render immediately.
    if (this._inputPending) {
      this._inputPending = false;
      this._cancelPendingRender();
      if (this.rafId != null) {
        cancelAnimationFrame(this.rafId);
        this.rafId = null;
      }
      this._doRender();
      this._lastRenderTime = performance.now();
      return;
    }

    // If throttle is disabled (0ms), use original rAF behavior.
    const interval = this._minRenderInterval;
    if (interval <= 0) {
      if (this.rafId == null) {
        this.rafId = requestAnimationFrame(() => {
          this.rafId = null;
          this._doRender();
        });
      }
      return;
    }

    // Throttle is active — check if enough time has elapsed since last render.
    const now = performance.now();
    const remaining = interval - (now - this._lastRenderTime);

    if (remaining <= 0) {
      // Enough time passed — schedule via rAF (aligned to vsync).
      this._cancelPendingRender();
      if (this.rafId == null) {
        this.rafId = requestAnimationFrame(() => {
          this.rafId = null;
          this._doRender();
          this._lastRenderTime = performance.now();
        });
      }
    } else if (this._throttleTimerId == null && this.rafId == null) {
      // Not enough time — defer scheduling until the interval expires.
      this._throttleTimerId = setTimeout(() => {
        this._throttleTimerId = null;
        if (this.rafId == null) {
          this.rafId = requestAnimationFrame(() => {
            this.rafId = null;
            this._doRender();
            this._lastRenderTime = performance.now();
          });
        }
      }, remaining);
    }
  }

  private _cancelPendingRender(): void {
    if (this._throttleTimerId != null) {
      clearTimeout(this._throttleTimerId);
      this._throttleTimerId = null;
    }
  }

  private _initialRender(): void {
    // Initial clientHeight read — only happens once at init.
    // After this, ResizeObserver keeps _cachedClientHeight up to date
    // without forcing layout during rendering.
    if (this._cachedClientHeight === 0) {
      this._cachedClientHeight = this.element.clientHeight;
    }
    this._doRender();
    this._lastRenderTime = performance.now();
  }

  private _doRender(): void {
    if (!this.bridge || !this.renderer) return;

    const rh = this._rowHeight || 19;
    const scrollbackCount = this.bridge.getScrollbackCount();
    const hasScrollback = scrollbackCount > 0;
    const hadScrollbackBefore = this._hadScrollback;

    // Only toggle class when state actually changes — avoids per-frame
    // attribute mutation observers and potential repaints.
    if (hasScrollback !== this._hadScrollback) {
      this.element.classList.toggle("has-scrollback", hasScrollback);
      this._hadScrollback = hasScrollback;
    }

    const scrollbackChanged = scrollbackCount !== this._lastScrollbackCount;
    // Only update paddingBottom when has-scrollback state toggles.
    if (hasScrollback !== hadScrollbackBefore) {
      const extraSpace = this._cachedClientHeight - this.rows * rh;
      this._container.style.paddingBottom = hasScrollback
        ? `${extraSpace}px`
        : "";
    }
    if (scrollbackChanged) {
      this._lastScrollbackCount = scrollbackCount;
    }

    // Determine the effective scroll position for rendering.
    // When auto-scrolling to bottom, we must use the TARGET scroll position
    // (maxScroll) so the renderer creates visible rows for the bottom of the
    // scrollback. We cannot scroll BEFORE rendering because the DOM content
    // height depends on what the renderer renders — the browser would clamp
    // scrollTop to the old content height.
    let renderScrollTop = this._cachedScrollTop;
    let needAutoScroll = false;

    if (hasScrollback) {
      this._cachedMaxScroll = scrollbackCount * rh;
      if (this._shouldScrollToBottom) {
        renderScrollTop = this._cachedMaxScroll;
        needAutoScroll = true;
      }
    } else {
      this._cachedMaxScroll = 0;
      if (scrollbackChanged && this._lastScrollbackCount > 0) {
        this.element.scrollTop = 0;
        this._cachedScrollTop = 0;
      }
    }

    // Render with the effective scroll position. The renderer creates visible
    // scrollback rows for this position, so the DOM content height will be
    // correct for the target scroll position.
    let dirtyCount = 0;
    const t0 = this.debug ? performance.now() : 0;
    if (this.debug) {
      for (let r = 0; r < this.rows; r++) {
        if (this.bridge.isDirtyRow(r)) dirtyCount++;
      }
    }

    this.renderer.render(
      this.bridge,
      renderScrollTop,
      this._cachedClientHeight,
      rh,
    );

    if (this.debug) {
      this.debug.recordRender(performance.now() - t0, dirtyCount);
    }

    // After rendering, the DOM has the correct content height. Defer the
    // scrollTop assignment to the next microtask to avoid forcing synchronous
    // layout recalculation (scrollTop reads force layout if any DOM write
    // happened in the same task).
    if (needAutoScroll) {
      const targetScroll = this._cachedMaxScroll;
      this._cachedScrollTop = targetScroll;
      this._shouldScrollToBottom = false;
      // Use queueMicrotask — runs before the next paint, so the scroll
      // appears seamless, but after the current JS task so the browser
      // can batch the layout computation.
      queueMicrotask(() => {
        if (this._destroyed) return;
        this._programmaticScroll = true;
        this.element.scrollTop = targetScroll;
        this._programmaticScroll = false;
      });
    }

    // Position the hidden textarea at the cursor for IME support.
    if (this.input) {
      const cursor = this.bridge.getCursor();
      const scrollTopChanged =
        this._cachedScrollTop !== this._lastCursorScrollTop;
      if (
        cursor.row !== this._lastCursorRow ||
        cursor.col !== this._lastCursorCol ||
        scrollbackChanged ||
        scrollTopChanged
      ) {
        this._lastCursorRow = cursor.row;
        this._lastCursorCol = cursor.col;
        this._lastCursorScrollTop = this._cachedScrollTop;
        this.input.positionAtCursor(
          cursor.row,
          cursor.col,
          scrollbackCount,
          this._cachedClientHeight,
          this._cachedScrollTop,
        );
      }
    }

    const title = this.bridge.getTitle();
    if (title !== null && this.onTitle) {
      this.onTitle(title);
    }

    const response = this.bridge.getResponse();
    if (response !== null && this.onData) {
      this.onData(response);
    }
  }

  private _lockHeight(): void {
    const rh = this._rowHeight || 17;
    const gridHeight = this.rows * rh;
    const cs = getComputedStyle(this.element);
    let extra =
      (parseFloat(cs.paddingTop) || 0) + (parseFloat(cs.paddingBottom) || 0);
    if (cs.boxSizing === "border-box") {
      extra +=
        (parseFloat(cs.borderTopWidth) || 0) +
        (parseFloat(cs.borderBottomWidth) || 0);
    }
    this.element.style.height = `${gridHeight + extra}px`;
  }

  private _setRowHeight(): void {
    const probe = document.createElement("div");
    probe.className = "term-row";
    probe.style.visibility = "hidden";
    probe.style.position = "absolute";
    probe.textContent = "W";
    this._container.appendChild(probe);
    const h = probe.getBoundingClientRect().height;
    probe.remove();
    if (h > 0) {
      const rh = Math.ceil(h);
      this._rowHeight = rh;
      this.element.style.setProperty("--term-row-height", `${rh}px`);
    }
  }

  private _measureAndSetCharWidth(): void {
    // Use CSS ch unit — same as _measureCharSize but only measures charWidth.
    // The old method (<span class="term-row">W</span>) was broken because
    // .term-row is display:block, so getBoundingClientRect().width returned
    // the full row width instead of the character advance width.
    const chProbe = document.createElement("div");
    chProbe.style.cssText = "position:absolute;visibility:hidden;width:100ch";
    this._container.appendChild(chProbe);
    const w = chProbe.getBoundingClientRect().width / 100;
    chProbe.remove();
    if (w > 0) {
      this._charWidth = w;
      if (this.input) {
        this.input.setLayoutMetrics(this._rowHeight, this._charWidth);
      }
    }
  }

  _measureCharSize(): {
    charWidth: number;
    rowHeight: number;
  } | null {
    // Use CSS ch unit for charWidth — it directly reflects the font's
    // advance width, avoiding inline-block layout quirks that can make
    // a <span>W</span> measure wider than the actual character advance.
    const chProbe = document.createElement("div");
    chProbe.style.cssText = "position:absolute;visibility:hidden;width:100ch";
    this._container.appendChild(chProbe);
    const charWidth = chProbe.getBoundingClientRect().width / 100;
    chProbe.remove();

    const row = document.createElement("div");
    row.className = "term-row";
    row.style.visibility = "hidden";
    row.style.position = "absolute";
    const textProbe = document.createElement("span");
    textProbe.textContent = "M";
    row.appendChild(textProbe);
    this._container.appendChild(row);
    const rowHeight = row.getBoundingClientRect().height;
    row.remove();

    if (charWidth === 0 || rowHeight === 0) return null;
    this._rowHeight = rowHeight;
    this._charWidth = charWidth;
    if (this.input) {
      this.input.setLayoutMetrics(this._rowHeight, this._charWidth);
    }
    return { charWidth, rowHeight };
  }

  private _setupResizeObserver(): void {
    const initial = this._measureCharSize();
    if (!initial) return;

    let { charWidth, rowHeight } = initial;

    this.resizeObserver = new ResizeObserver((entries) => {
      const measured = this._measureCharSize();
      if (measured) {
        charWidth = measured.charWidth;
        rowHeight = measured.rowHeight;
      }

      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        this._cachedClientHeight = Math.round(height);
        const newCols = Math.max(1, Math.floor(width / charWidth));
        const newRows = Math.max(1, Math.floor(height / rowHeight));
        if (newCols !== this.cols || newRows !== this.rows) {
          this.resize(newCols, newRows);
        }
      }
    });
    this.resizeObserver.observe(this.element);
  }

  /** Force an immediate render, bypassing any throttle. */
  flush(): void {
    this._inputPending = false;
    this._cancelPendingRender();
    if (this.rafId != null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    this._doRender();
    this._lastRenderTime = performance.now();
  }

  destroy(): void {
    this._destroyed = true;
    if (this.rafId != null) cancelAnimationFrame(this.rafId);
    if (this._throttleTimerId != null) clearTimeout(this._throttleTimerId);
    if (this.resizeObserver) this.resizeObserver.disconnect();
    if (this.input) this.input.destroy();
    this.element.removeEventListener("click", this._onClickFocus);
    this.element.removeEventListener("scroll", this._onScroll);
    this.element.innerHTML = "";
    if (
      this.debug &&
      (globalThis as Record<string, unknown>).__wterm === this
    ) {
      delete (globalThis as Record<string, unknown>).__wterm;
    }
    this.debug = null;
  }
}
