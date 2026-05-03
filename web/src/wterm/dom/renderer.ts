import type { WasmBridge, CellBuf } from "../core/index.js";

const DEFAULT_COLOR = 256;
const FLAG_BOLD = 0x01;
const FLAG_DIM = 0x02;
const FLAG_ITALIC = 0x04;
const FLAG_UNDERLINE = 0x08;
const FLAG_REVERSE = 0x20;
const FLAG_INVISIBLE = 0x40;
const FLAG_STRIKETHROUGH = 0x80;

const FLAG_WIDE = 0x01;
const FLAG_CONTINUATION = 0x02;

function colorToCSS(index: number): string | null {
  if (index === DEFAULT_COLOR) return null;
  if (index < 16) return `var(--term-color-${index})`;
  if (index < 232) {
    const n = index - 16;
    const r = Math.floor(n / 36) * 51;
    const g = (Math.floor(n / 6) % 6) * 51;
    const b = (n % 6) * 51;
    return `rgb(${r},${g},${b})`;
  }
  const level = (index - 232) * 10 + 8;
  return `rgb(${level},${level},${level})`;
}

function buildCellStyle(fg: number, bg: number, flags: number): string {
  let fgC = fg,
    bgC = bg;
  if (flags & FLAG_REVERSE) {
    const tmp = fgC;
    fgC = bgC;
    bgC = tmp;
    if (fgC === DEFAULT_COLOR) fgC = 0;
    if (bgC === DEFAULT_COLOR) bgC = 7;
  }

  const fgCSS = colorToCSS(fgC);
  const bgCSS = colorToCSS(bgC);

  let style = "";
  if (fgCSS) style += `color:${fgCSS};`;
  if (bgCSS) style += `background:${bgCSS};`;
  if (flags & FLAG_BOLD) style += "font-weight:bold;";
  if (flags & FLAG_DIM) style += "opacity:0.5;";
  if (flags & FLAG_ITALIC) style += "font-style:italic;";

  const decorations: string[] = [];
  if (flags & FLAG_UNDERLINE) decorations.push("underline");
  if (flags & FLAG_STRIKETHROUGH) decorations.push("line-through");
  if (decorations.length) style += `text-decoration:${decorations.join(" ")};`;

  if (flags & FLAG_INVISIBLE) style += "visibility:hidden;";
  return style;
}

/// Numeric key for (fg, bg, flags) — fast integer comparison for run boundaries.
function styleKey(fg: number, bg: number, flags: number): number {
  return (fg * 257 + bg) * 256 + flags;
}

/// Cache buildCellStyle results keyed by numeric styleKey.
const _styleCache = new Map<number, string>();
const _STYLE_CACHE_MAX = 4096;

function getCachedCellStyle(fg: number, bg: number, flags: number): string {
  const key = styleKey(fg, bg, flags);
  let cached = _styleCache.get(key);
  if (cached !== undefined) return cached;
  if (_styleCache.size >= _STYLE_CACHE_MAX) _styleCache.clear();
  cached = buildCellStyle(fg, bg, flags);
  _styleCache.set(key, cached);
  return cached;
}

function resolveColors(
  fg: number,
  bg: number,
  flags: number,
): { fg: string; bg: string } {
  let fgC = fg,
    bgC = bg;
  if (flags & FLAG_REVERSE) {
    [fgC, bgC] = [bgC, fgC];
    if (fgC === DEFAULT_COLOR) fgC = 0;
    if (bgC === DEFAULT_COLOR) bgC = 7;
  }
  return {
    fg: colorToCSS(fgC) || "var(--term-fg)",
    bg: colorToCSS(bgC) || "var(--term-bg)",
  };
}

function getBlockBackground(cp: number, fg: string, bg: string): string {
  switch (cp) {
    case 0x2580:
      return `linear-gradient(${fg} 50%,${bg} 50%)`;
    case 0x2581:
      return `linear-gradient(${bg} 87.5%,${fg} 87.5%)`;
    case 0x2582:
      return `linear-gradient(${bg} 75%,${fg} 75%)`;
    case 0x2583:
      return `linear-gradient(${bg} 62.5%,${fg} 62.5%)`;
    case 0x2584:
      return `linear-gradient(${bg} 50%,${fg} 50%)`;
    case 0x2585:
      return `linear-gradient(${bg} 37.5%,${fg} 37.5%)`;
    case 0x2586:
      return `linear-gradient(${bg} 25%,${fg} 25%)`;
    case 0x2587:
      return `linear-gradient(${bg} 12.5%,${fg} 12.5%)`;
    case 0x2588:
      return fg;
    case 0x2589:
      return `linear-gradient(to right,${fg} 87.5%,${bg} 87.5%)`;
    case 0x258a:
      return `linear-gradient(to right,${fg} 75%,${bg} 75%)`;
    case 0x258b:
      return `linear-gradient(to right,${fg} 62.5%,${bg} 62.5%)`;
    case 0x258c:
      return `linear-gradient(to right,${fg} 50%,${bg} 50%)`;
    case 0x258d:
      return `linear-gradient(to right,${fg} 37.5%,${bg} 37.5%)`;
    case 0x258e:
      return `linear-gradient(to right,${fg} 25%,${bg} 25%)`;
    case 0x258f:
      return `linear-gradient(to right,${fg} 12.5%,${bg} 12.5%)`;
    case 0x2590:
      return `linear-gradient(to right,${bg} 50%,${fg} 50%)`;
    case 0x2591:
      return `color-mix(in srgb,${fg} 25%,${bg})`;
    case 0x2592:
      return `color-mix(in srgb,${fg} 50%,${bg})`;
    case 0x2593:
      return `color-mix(in srgb,${fg} 75%,${bg})`;
    case 0x2594:
      return `linear-gradient(${fg} 12.5%,${bg} 12.5%)`;
    case 0x2595:
      return `linear-gradient(to right,${bg} 87.5%,${fg} 87.5%)`;
    default: {
      const QUADRANTS: Record<number, [boolean, boolean, boolean, boolean]> = {
        0x2596: [false, false, true, false],
        0x2597: [false, false, false, true],
        0x2598: [true, false, false, false],
        0x2599: [true, false, true, true],
        0x259a: [true, false, false, true],
        0x259b: [true, true, true, false],
        0x259c: [true, true, false, true],
        0x259d: [false, true, false, false],
        0x259e: [false, true, true, false],
        0x259f: [false, true, true, true],
      };
      const q = QUADRANTS[cp];
      if (!q) return fg;
      const [tl, tr, bl, br] = q;
      if (tl && tr && bl && br) return fg;
      const layers: string[] = [];
      const POS = ["0 0", "100% 0", "0 100%", "100% 100%"];
      q.forEach((filled, i) => {
        if (filled)
          layers.push(
            `linear-gradient(${fg},${fg}) ${POS[i]}/50% 50% no-repeat`,
          );
      });
      layers.push(bg);
      return layers.join(",");
    }
  }
}

/// Describes a single span element to render.
interface RunInfo {
  text: string;
  cssText: string;
  className: string;
  styleKey: number;
  colCount: number;
}

export class Renderer {
  private container: HTMLElement;
  private rows = 0;
  private cols = 0;

  private rowEls: HTMLDivElement[] = [];
  private prevCursorRow = -1;
  private prevCursorCol = -1;
  private prevContainerBg = "";
  private prevRowBg: string[] = [];

  private _spanStyleKeys = new WeakMap<HTMLElement, number>();

  // Virtual scrolling for scrollback
  private _topSpacer!: HTMLDivElement;
  private _bottomSpacer!: HTMLDivElement;
  private _scrollbackRowEls: HTMLDivElement[] = [];
  private _renderedScrollbackCount = 0;
  private _visibleStart = -1;
  private _visibleEnd = -1;

  private _cellBuf: CellBuf = { char: 0, fg: 0, bg: 0, flags: 0, wide: 0 };
  private _cpScan: Uint32Array = new Uint32Array(0);
  private _wideScan: Uint8Array = new Uint8Array(0);
  private _runs: RunInfo[] = [];
  private _runPool: RunInfo[] = [];
  private _runPoolUsed = 0;

  private _acquireRun(
    text: string,
    cssText: string,
    className: string,
    styleKey: number,
    colCount: number,
  ): RunInfo {
    if (this._runPoolUsed < this._runPool.length) {
      const run = this._runPool[this._runPoolUsed++];
      run.text = text;
      run.cssText = cssText;
      run.className = className;
      run.styleKey = styleKey;
      run.colCount = colCount;
      return run;
    }
    const run: RunInfo = { text, cssText, className, styleKey, colCount };
    this._runPool.push(run);
    this._runPoolUsed++;
    return run;
  }

  private _resetPool(): void {
    this._runPoolUsed = 0;
  }

  private static readonly SCROLL_BUFFER = 10;

  constructor(container: HTMLElement) {
    this.container = container;
  }

  getScrollbackCount(): number {
    return this._renderedScrollbackCount;
  }

  setup(cols: number, rows: number): void {
    this.cols = cols;
    this.rows = rows;
    this.container.innerHTML = "";
    this.rowEls = [];
    this.prevRowBg = [];
    this._scrollbackRowEls = [];
    this._renderedScrollbackCount = 0;
    this._visibleStart = -1;
    this._visibleEnd = -1;
    this._spanStyleKeys = new WeakMap();

    this._topSpacer = document.createElement("div");
    this._topSpacer.style.height = "0";
    this._bottomSpacer = document.createElement("div");
    this._bottomSpacer.style.height = "0";

    const fragment = document.createDocumentFragment();
    fragment.appendChild(this._topSpacer);
    fragment.appendChild(this._bottomSpacer);

    for (let r = 0; r < rows; r++) {
      const rowEl = document.createElement("div");
      rowEl.className = "term-row";
      fragment.appendChild(rowEl);
      this.rowEls.push(rowEl);
    }
    this.container.appendChild(fragment);
    this.prevCursorRow = -1;
    this.prevCursorCol = -1;
  }

  private _buildRowContent(
    rowEl: HTMLDivElement,
    getCell: (col: number, out: CellBuf) => void,
    lineLen: number,
    cursorCol: number,
    rowIndex: number,
  ): void {
    const c = this._cellBuf;
    const runs = this._runs;
    runs.length = 0;
    this._resetPool();

    const cols = this.cols;
    if (this._cpScan.length < cols) {
      this._cpScan = new Uint32Array(cols);
      this._wideScan = new Uint8Array(cols);
    }
    const cpScan = this._cpScan;
    const wideScan = this._wideScan;
    for (let col = 0; col < cols; col++) {
      getCell(col, c);
      cpScan[col] = c.char;
      wideScan[col] = c.wide;
    }

    let effectiveCursorCol = cursorCol;
    if (cursorCol >= 1) {
      getCell(cursorCol, c);
      if (c.wide & FLAG_CONTINUATION) effectiveCursorCol = cursorCol - 1;
    }

    // Phase 1: Collect runs — one span per single-width cell to ensure
    // each glyph occupies exactly 1ch in layout, preventing wide symbols
    // (✶ ✳ ⏺ etc.) from shifting subsequent content.  Glyphs may visually
    // overflow their cell (matching Ghostty's behavior) but the layout
    // width is strictly 1ch.
    for (let col = 0; col < cols; col++) {
      const cp = cpScan[col];
      const wide = wideScan[col];
      const inBounds = col < lineLen;

      if (inBounds && wide & FLAG_CONTINUATION) continue;

      const effectiveCp = inBounds ? cp : 0;
      const isCursor = col === effectiveCursorCol;

      if (inBounds && effectiveCp >= 0x2580 && effectiveCp <= 0x259f) {
        // Block elements: render as background gradient, no text
        getCell(col, c);
        const colors = resolveColors(c.fg, c.bg, c.flags);
        let cssText = `background:${getBlockBackground(effectiveCp, colors.fg, colors.bg)}`;
        if (c.flags & FLAG_DIM) cssText += ";opacity:0.5";
        runs.push(
          this._acquireRun(
            "",
            cssText,
            isCursor ? "term-block term-cursor" : "term-block",
            -1,
            1,
          ),
        );
      } else if (inBounds && wide & FLAG_WIDE) {
        // Wide characters: 2ch span
        getCell(col, c);
        const ch = effectiveCp >= 32 ? String.fromCodePoint(effectiveCp) : " ";
        const style = getCachedCellStyle(c.fg, c.bg, c.flags);
        const sk = styleKey(c.fg, c.bg, c.flags);
        runs.push(
          this._acquireRun(
            ch,
            style,
            isCursor ? "term-wide term-cursor" : "term-wide",
            sk,
            2,
          ),
        );
      } else {
        // Normal single-width: one span per character, fixed 1ch layout
        getCell(col, c);
        const ch =
          inBounds && effectiveCp >= 32
            ? String.fromCodePoint(effectiveCp)
            : " ";
        const style = inBounds
          ? getCachedCellStyle(c.fg, c.bg, c.flags)
          : "";
        const sk = inBounds ? styleKey(c.fg, c.bg, c.flags) : 0;
        const base = style ? style + ";" : "";
        runs.push(
          this._acquireRun(
            ch,
            base + "width:1ch",
            isCursor ? "term-cursor" : "",
            sk,
            1,
          ),
        );
      }
    }

    // Phase 2: Apply runs to DOM — always reset cssText to prevent stale inline styles
    const runCount = runs.length;
    const existing = rowEl.children;
    const existingCount = existing.length;

    if (existingCount === runCount) {
      for (let i = 0; i < runCount; i++) {
        const child = existing[i] as HTMLElement;
        const run = runs[i];
        const classChanged = child.className !== run.className;
        const prevSk = this._spanStyleKeys.get(child);
        const styleChanged = prevSk !== run.styleKey;
        if (!styleChanged && !classChanged) {
          const tn = child.firstChild as Text | null;
          if (tn) {
            if (tn.nodeValue !== run.text) tn.nodeValue = run.text;
          } else if (run.text) child.textContent = run.text;
          continue;
        }
        if (classChanged) child.className = run.className;
        // Always reset cssText when anything changed to clear stale inline styles
        child.style.cssText = run.cssText;
        this._spanStyleKeys.set(child, run.styleKey);
        const tn = child.firstChild as Text | null;
        if (tn) {
          if (tn.nodeValue !== run.text) tn.nodeValue = run.text;
        } else if (run.text) child.textContent = run.text;
      }
    } else if (existingCount > 0) {
      const minCount = Math.min(existingCount, runCount);
      for (let i = 0; i < minCount; i++) {
        const run = runs[i];
        const child = existing[i] as HTMLElement;
        if (child.className !== run.className) child.className = run.className;
        child.style.cssText = run.cssText;
        this._spanStyleKeys.set(child, run.styleKey);
        const tn = child.firstChild as Text | null;
        if (tn) {
          if (tn.nodeValue !== run.text) tn.nodeValue = run.text;
        } else if (run.text) child.textContent = run.text;
      }
      if (runCount > existingCount) {
        const frag = document.createDocumentFragment();
        for (let i = existingCount; i < runCount; i++) {
          const run = runs[i];
          const span = document.createElement("span");
          if (run.className) span.className = run.className;
          if (run.cssText) span.style.cssText = run.cssText;
          if (run.text) span.textContent = run.text;
          if (run.styleKey !== 0) this._spanStyleKeys.set(span, run.styleKey);
          frag.appendChild(span);
        }
        rowEl.appendChild(frag);
      }
      while (rowEl.children.length > runCount) {
        rowEl.lastChild!.remove();
      }
    } else {
      const frag = document.createDocumentFragment();
      for (let i = 0; i < runCount; i++) {
        const run = runs[i];
        const span = document.createElement("span");
        if (run.className) span.className = run.className;
        if (run.cssText) span.style.cssText = run.cssText;
        if (run.text) span.textContent = run.text;
        if (run.styleKey !== 0) this._spanStyleKeys.set(span, run.styleKey);
        frag.appendChild(span);
      }
      rowEl.appendChild(frag);
    }

    // Row background
    let bgCss = "";
    if (lineLen >= this.cols && this.cols > 0) {
      getCell(this.cols - 1, c);
      let bgC = c.bg;
      if (c.flags & FLAG_REVERSE) {
        bgC = c.fg;
        if (bgC === DEFAULT_COLOR) bgC = 7;
      }
      bgCss = colorToCSS(bgC) || "";
    }
    const boxShadow = bgCss ? `0 1px 0 ${bgCss}` : "";
    if (rowIndex >= 0) {
      if (bgCss !== (this.prevRowBg[rowIndex] ?? "")) {
        rowEl.style.background = bgCss;
        rowEl.style.boxShadow = boxShadow;
        this.prevRowBg[rowIndex] = bgCss;
      }
    } else {
      rowEl.style.background = bgCss;
      rowEl.style.boxShadow = boxShadow;
    }
  }

  private _buildScrollbackRowEl(
    bridge: WasmBridge,
    sbOffset: number,
    rowEl?: HTMLDivElement,
  ): HTMLDivElement {
    if (!rowEl) {
      rowEl = document.createElement("div");
      rowEl.className = "term-row term-scrollback-row";
    }
    const lineLen = bridge.getScrollbackLineLen(sbOffset);

    this._buildRowContent(
      rowEl,
      (col, out) => bridge.getScrollbackCellInto(sbOffset, col, out),
      lineLen,
      -1,
      -1,
    );
    return rowEl;
  }

  private _syncVirtualScrollback(
    bridge: WasmBridge,
    scrollTop: number,
    clientHeight: number,
    rh: number,
  ): void {
    const scrollbackCount = bridge.getScrollbackCount();

    if (scrollbackCount === 0) {
      if (this._scrollbackRowEls.length > 0) {
        for (const el of this._scrollbackRowEls) el.remove();
        this._scrollbackRowEls = [];
      }
      this._topSpacer.style.height = "0";
      this._bottomSpacer.style.height = "0";
      this._renderedScrollbackCount = 0;
      this._visibleStart = -1;
      this._visibleEnd = -1;
      return;
    }

    const BUFFER = Renderer.SCROLL_BUFFER;
    const visStart = Math.max(0, Math.floor(scrollTop / rh) - BUFFER);
    const visEnd = Math.min(
      scrollbackCount,
      Math.ceil((scrollTop + clientHeight) / rh) + BUFFER,
    );

    if (
      scrollbackCount === this._renderedScrollbackCount &&
      visStart === this._visibleStart &&
      visEnd === this._visibleEnd
    ) {
      return;
    }

    const sbOffset = (pos: number) => scrollbackCount - 1 - pos;

    const oldStart = this._visibleStart;
    const oldEnd = this._visibleEnd;
    const oldCount = this._scrollbackRowEls.length;

    if (oldCount === 0 || oldStart < 0) {
      const fragment = document.createDocumentFragment();
      for (let i = visStart; i < visEnd; i++) {
        const rowEl = this._buildScrollbackRowEl(bridge, sbOffset(i));
        fragment.appendChild(rowEl);
        this._scrollbackRowEls.push(rowEl);
      }
      this._topSpacer.style.height = `${visStart * rh}px`;
      this._topSpacer.after(fragment);
      this._bottomSpacer.style.height = `${(scrollbackCount - visEnd) * rh}px`;
    } else if (
      oldStart === visStart &&
      oldEnd === visEnd &&
      scrollbackCount !== this._renderedScrollbackCount
    ) {
      this._bottomSpacer.style.height = `${(scrollbackCount - visEnd) * rh}px`;
    } else {
      const overlapStart = Math.max(oldStart, visStart);
      const overlapEnd = Math.min(oldEnd, visEnd);

      if (overlapStart < overlapEnd && oldCount > 0) {
        const newEls: HTMLDivElement[] = [];

        if (visStart < oldStart) {
          const fragment = document.createDocumentFragment();
          for (let i = visStart; i < oldStart; i++) {
            const rowEl = this._buildScrollbackRowEl(bridge, sbOffset(i));
            fragment.appendChild(rowEl);
            newEls.push(rowEl);
          }
          this._topSpacer.after(fragment);
        }

        const reuseStartIdx = Math.max(0, overlapStart - oldStart);
        const reuseEndIdx = Math.min(oldCount, overlapEnd - oldStart);
        for (let i = reuseStartIdx; i < reuseEndIdx; i++) {
          const domPos = oldStart + i;
          if (domPos >= visStart && domPos < visEnd) {
            const rowEl = this._scrollbackRowEls[i];
            this._buildScrollbackRowEl(bridge, sbOffset(domPos), rowEl);
            newEls.push(rowEl);
          }
        }

        if (visEnd > oldEnd) {
          const fragment = document.createDocumentFragment();
          for (let i = oldEnd; i < visEnd; i++) {
            const rowEl = this._buildScrollbackRowEl(bridge, sbOffset(i));
            fragment.appendChild(rowEl);
            newEls.push(rowEl);
          }
          this._bottomSpacer.before(fragment);
        }

        for (let i = 0; i < reuseStartIdx; i++) {
          this._scrollbackRowEls[i].remove();
        }
        for (let i = reuseEndIdx; i < oldCount; i++) {
          this._scrollbackRowEls[i].remove();
        }

        this._scrollbackRowEls = newEls;
      } else {
        for (const el of this._scrollbackRowEls) el.remove();
        this._scrollbackRowEls = [];

        const fragment = document.createDocumentFragment();
        for (let i = visStart; i < visEnd; i++) {
          const rowEl = this._buildScrollbackRowEl(bridge, sbOffset(i));
          fragment.appendChild(rowEl);
          this._scrollbackRowEls.push(rowEl);
        }
        this._topSpacer.after(fragment);
      }

      this._topSpacer.style.height = `${visStart * rh}px`;
      this._bottomSpacer.style.height = `${(scrollbackCount - visEnd) * rh}px`;
    }

    this._renderedScrollbackCount = scrollbackCount;
    this._visibleStart = visStart;
    this._visibleEnd = visEnd;
  }

  render(bridge: WasmBridge, scrollTop = 0, clientHeight = 0, rh = 19): void {
    const rows = bridge.getRows();
    const cols = bridge.getCols();

    let resized = false;
    if (rows !== this.rows || cols !== this.cols) {
      this.setup(cols, rows);
      resized = true;
    }

    this._syncVirtualScrollback(bridge, scrollTop, clientHeight, rh);

    const cursor = bridge.getCursor();
    const cursorVisible = cursor.visible;

    const needsCursorUpdate =
      cursor.row !== this.prevCursorRow || cursor.col !== this.prevCursorCol;

    const dirtyArr = bridge.getDirtyView();

    for (let r = 0; r < this.rows; r++) {
      const isDirty = resized || dirtyArr[r] !== 0;
      const hadCursor = r === this.prevCursorRow && needsCursorUpdate;
      const hasCursor = r === cursor.row;

      if (isDirty || hadCursor || (hasCursor && needsCursorUpdate)) {
        const cCol = hasCursor && cursorVisible ? cursor.col : -1;
        this._buildRowContent(
          this.rowEls[r],
          (col, out) => bridge.getCellInto(r, col, out),
          this.cols,
          cCol,
          r,
        );
      }
    }

    this.prevCursorRow = cursor.row;
    this.prevCursorCol = cursor.col;

    const lastRowDirty = resized || dirtyArr[this.rows - 1] !== 0;
    if (lastRowDirty) {
      bridge.getCellInto(this.rows - 1, 0, this._cellBuf);
      const c = this._cellBuf;
      let gridBg = c.bg;
      if (c.flags & FLAG_REVERSE) {
        gridBg = c.fg;
        if (gridBg === DEFAULT_COLOR) gridBg = 7;
      }
      const containerBg = colorToCSS(gridBg) || "";
      if (containerBg !== this.prevContainerBg) {
        this.container.style.background = containerBg;
        this.prevContainerBg = containerBg;
      }
    }

    bridge.clearDirty();
  }
}
