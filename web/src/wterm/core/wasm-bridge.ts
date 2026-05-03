export interface CellData {
  char: number;
  fg: number;
  bg: number;
  flags: number;
  wide: number;
}

/// Reusable cell buffer — avoids allocating a new object per getCell() call.
/// Use with getCellInto() / getScrollbackCellInto().
export interface CellBuf {
  char: number;
  fg: number;
  bg: number;
  flags: number;
  wide: number;
}

export interface CursorState {
  row: number;
  col: number;
  visible: boolean;
}

interface WasmExports {
  memory: WebAssembly.Memory;
  init(cols: number, rows: number): void;
  resizeTerminal(cols: number, rows: number): void;
  getWriteBuffer(): number;
  writeBytes(len: number): void;
  getGridPtr(): number;
  getDirtyPtr(): number;
  clearDirty(): void;
  getCursorRow(): number;
  getCursorCol(): number;
  getCursorVisible(): number;
  getCols(): number;
  getRows(): number;
  getCursorKeysApp(): number;
  getBracketedPaste(): number;
  getUsingAltScreen(): number;
  getTitlePtr(): number;
  getTitleLen(): number;
  getTitleChanged(): number;
  getScrollbackCount(): number;
  getScrollbackLine(offset: number): number;
  getScrollbackLineLen(offset: number): number;
  getResponsePtr(): number;
  getResponseLen(): number;
  clearResponse(): void;
  getCellSize(): number;
  getMaxCols(): number;
  getDebugLogPtr(): number;
  getDebugLogCount(): number;
  getDebugLogEntrySize(): number;
  getDebugLogMax(): number;
}

export interface UnhandledSequence {
  final: string;
  private: string;
  paramCount: number;
  params: number[];
}

export class WasmBridge {
  private exports: WasmExports;
  private memory: WebAssembly.Memory;
  private gridPtr = 0;
  private dirtyPtr = 0;
  private writeBufferPtr = 0;
  private cellSize = 12;
  private maxCols = 256;
  private encoder = new TextEncoder();
  private decoder = new TextDecoder();
  private _dv!: DataView;

  constructor(instance: WebAssembly.Instance) {
    this.exports = instance.exports as unknown as WasmExports;
    this.memory = this.exports.memory;
  }

  static async load(url?: string): Promise<WasmBridge> {
    const wasmUrl =
      url || new URL("../../assets/wterm.wasm", import.meta.url).href;
    const response = await fetch(wasmUrl);
    if (!response.ok) {
      throw new Error(
        `[wterm] Failed to load WASM from ${wasmUrl}: ${response.status} ${response.statusText}`,
      );
    }
    const bytes = await response.arrayBuffer();
    const { instance } = await WebAssembly.instantiate(bytes);
    return new WasmBridge(instance);
  }

  init(cols: number, rows: number): void {
    this.exports.init(cols, rows);
    this._updatePointers();
  }

  private _updatePointers(): void {
    this.gridPtr = this.exports.getGridPtr();
    this.dirtyPtr = this.exports.getDirtyPtr();
    this.writeBufferPtr = this.exports.getWriteBuffer();
    this.cellSize = this.exports.getCellSize();
    this.maxCols = this.exports.getMaxCols();
    this._dv = new DataView(this.memory.buffer);
  }

  writeString(str: string): void {
    const encoded = this.encoder.encode(str);
    this.writeRaw(encoded);
  }

  writeRaw(data: Uint8Array): void {
    const buf = new Uint8Array(this.memory.buffer, this.writeBufferPtr, 8192);
    let offset = 0;
    while (offset < data.length) {
      const chunk = Math.min(data.length - offset, 8192);
      buf.set(data.subarray(offset, offset + chunk));
      try {
        this.exports.writeBytes(chunk);
      } catch (e) {
        const memPages = this.memory.buffer.byteLength / 65536;
        console.error(
          `[wterm] OOB! cols=${this.exports.getCols()} rows=${this.exports.getRows()} ` +
            `altScreen=${this.exports.getUsingAltScreen()} gridPtr=${this.gridPtr} ` +
            `memBytes=${this.memory.buffer.byteLength} pages=${memPages} ` +
            `chunk=${chunk} dataLen=${data.length}`,
        );
        throw e;
      }
      offset += chunk;
    }
  }

  getCell(row: number, col: number): CellData {
    const offset = this.gridPtr + (row * this.maxCols + col) * this.cellSize;
    const dv = this._dv;
    return {
      char: dv.getUint32(offset, true),
      fg: dv.getUint16(offset + 4, true),
      bg: dv.getUint16(offset + 6, true),
      flags: dv.getUint8(offset + 8),
      wide: dv.getUint8(offset + 9),
    };
  }

  /// Read cell data into an existing buffer (avoids object allocation).
  getCellInto(row: number, col: number, out: CellBuf): void {
    const offset = this.gridPtr + (row * this.maxCols + col) * this.cellSize;
    const dv = this._dv;
    out.char = dv.getUint32(offset, true);
    out.fg = dv.getUint16(offset + 4, true);
    out.bg = dv.getUint16(offset + 6, true);
    out.flags = dv.getUint8(offset + 8);
    out.wide = dv.getUint8(offset + 9);
  }

  isDirtyRow(row: number): boolean {
    return new Uint8Array(this.memory.buffer, this.dirtyPtr, 256)[row] !== 0;
  }

  /// Get a cached view of the dirty row flags.
  /// Reuse across isDirtyRow checks within a single render pass.
  /// Invalidated automatically when WASM memory grows.
  private _dirtyView: Uint8Array | null = null;
  getDirtyView(): Uint8Array {
    if (!this._dirtyView || this._dirtyView.buffer !== this.memory.buffer) {
      this._dirtyView = new Uint8Array(this.memory.buffer, this.dirtyPtr, 256);
    }
    return this._dirtyView;
  }

  clearDirty(): void {
    this.exports.clearDirty();
  }

  getCursor(): CursorState {
    return {
      row: this.exports.getCursorRow(),
      col: this.exports.getCursorCol(),
      visible: this.exports.getCursorVisible() !== 0,
    };
  }

  getCols(): number {
    return this.exports.getCols();
  }
  getRows(): number {
    return this.exports.getRows();
  }

  cursorKeysApp(): boolean {
    return this.exports.getCursorKeysApp() !== 0;
  }
  bracketedPaste(): boolean {
    return this.exports.getBracketedPaste() !== 0;
  }
  usingAltScreen(): boolean {
    return this.exports.getUsingAltScreen() !== 0;
  }

  getTitle(): string | null {
    if (this.exports.getTitleChanged() === 0) return null;
    const ptr = this.exports.getTitlePtr();
    const len = this.exports.getTitleLen();
    const bytes = new Uint8Array(this.memory.buffer, ptr, len);
    return this.decoder.decode(bytes);
  }

  getResponse(): string | null {
    const len = this.exports.getResponseLen();
    if (len === 0) return null;
    const ptr = this.exports.getResponsePtr();
    const bytes = new Uint8Array(this.memory.buffer, ptr, len);
    const str = this.decoder.decode(bytes);
    this.exports.clearResponse();
    return str;
  }

  getScrollbackCount(): number {
    return this.exports.getScrollbackCount();
  }

  getScrollbackCell(offset: number, col: number): CellData {
    const ptr = this.exports.getScrollbackLine(offset);
    const off = ptr + col * this.cellSize;
    const dv = this._dv;
    return {
      char: dv.getUint32(off, true),
      fg: dv.getUint16(off + 4, true),
      bg: dv.getUint16(off + 6, true),
      flags: dv.getUint8(off + 8),
      wide: dv.getUint8(off + 9),
    };
  }

  /// Read scrollback cell data into an existing buffer (avoids object allocation).
  getScrollbackCellInto(offset: number, col: number, out: CellBuf): void {
    const ptr = this.exports.getScrollbackLine(offset);
    const off = ptr + col * this.cellSize;
    const dv = this._dv;
    out.char = dv.getUint32(off, true);
    out.fg = dv.getUint16(off + 4, true);
    out.bg = dv.getUint16(off + 6, true);
    out.flags = dv.getUint8(off + 8);
    out.wide = dv.getUint8(off + 9);
  }

  getScrollbackLineLen(offset: number): number {
    return this.exports.getScrollbackLineLen(offset);
  }

  getUnhandledSequences(): UnhandledSequence[] {
    const count = this.exports.getDebugLogCount();
    if (count === 0) return [];
    const ptr = this.exports.getDebugLogPtr();
    const entrySize = this.exports.getDebugLogEntrySize();
    const maxEntries = this.exports.getDebugLogMax();
    const total = Math.min(count, maxEntries);
    const dv = new DataView(this.memory.buffer);
    const entries: UnhandledSequence[] = [];
    const startIdx = count >= maxEntries ? count % maxEntries : 0;
    for (let i = 0; i < total; i++) {
      const idx = (startIdx + i) % maxEntries;
      const off = ptr + idx * entrySize;
      const finalByte = dv.getUint8(off);
      if (finalByte === 0) continue;
      const privateByte = dv.getUint8(off + 1);
      const paramCount = dv.getUint8(off + 2);
      const params: number[] = [];
      for (let p = 0; p < Math.min(paramCount, 4); p++) {
        params.push(dv.getUint16(off + 4 + p * 2, true));
      }
      entries.push({
        final: String.fromCharCode(finalByte),
        private: privateByte ? String.fromCharCode(privateByte) : "",
        paramCount,
        params,
      });
    }
    return entries;
  }

  resize(cols: number, rows: number): void {
    this.exports.resizeTerminal(cols, rows);
    this._updatePointers();
  }
}
