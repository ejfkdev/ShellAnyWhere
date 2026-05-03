/// Bincode v2 decoder (standard config: varint for all integers, lengths, enums)
/// Matches Rust's `bincode::config::standard()` encoding which uses
/// `IntEncoding::Varint` — ALL integers (u8, u16, u32, u64, i8, i16, i32, i64)
/// are encoded as varints.

export class BincodeReader {
  private buf: Uint8Array;
  private pos: number;

  constructor(data: Uint8Array) {
    this.buf = data;
    this.pos = 0;
  }

  get remaining(): number {
    return this.buf.length - this.pos;
  }

  readU8(): number {
    if (this.pos >= this.buf.length)
      throw new Error("bincode: unexpected end of data");
    return this.buf[this.pos++];
  }

  readBool(): boolean {
    return this.readVarint() !== 0;
  }

  /// In bincode v2 standard config, u16 is varint-encoded.
  readU16(): number {
    return this.readVarint();
  }

  /// In bincode v2 standard config, u32 is varint-encoded.
  readU32(): number {
    return this.readVarint();
  }

  /// In bincode v2 standard config, u64 is varint-encoded.
  readU64(): number {
    return this.readVarint();
  }

  /// Read a varint (used for ALL integers, lengths, and enum discriminants).
  /// bincode v2 varint encoding:
  ///   0-250 → 1 byte
  ///   251 → followed by u16 LE
  ///   252 → followed by u32 LE
  ///   253 → followed by u64 LE
  readVarint(): number {
    const first = this.readU8();
    if (first <= 250) return first;
    if (first === 251) {
      const v = new DataView(
        this.buf.buffer,
        this.buf.byteOffset + this.pos,
        2,
      );
      this.pos += 2;
      return v.getUint16(0, true);
    }
    if (first === 252) {
      const v = new DataView(
        this.buf.buffer,
        this.buf.byteOffset + this.pos,
        4,
      );
      this.pos += 4;
      return v.getUint32(0, true);
    }
    if (first === 253) {
      const lo = new DataView(
        this.buf.buffer,
        this.buf.byteOffset + this.pos,
        4,
      ).getUint32(0, true);
      this.pos += 4;
      const hi = new DataView(
        this.buf.buffer,
        this.buf.byteOffset + this.pos,
        4,
      ).getUint32(0, true);
      this.pos += 4;
      return lo + hi * 0x100000000;
    }
    throw new Error(`bincode: invalid varint marker ${first}`);
  }

  /// Read a length-prefixed string (varint length + UTF-8 bytes).
  readString(): string {
    const len = this.readVarint();
    const bytes = this.readBytes(len);
    return new TextDecoder().decode(bytes);
  }

  /// Read a length-prefixed byte vector (varint length + bytes).
  readVecU8(): Uint8Array {
    const len = this.readVarint();
    return this.readBytes(len);
  }

  /// Read a fixed-size byte array.
  readBytes(n: number): Uint8Array {
    if (this.pos + n > this.buf.length)
      throw new Error("bincode: unexpected end of data");
    const slice = this.buf.slice(this.pos, this.pos + n);
    this.pos += n;
    return slice;
  }

  /// Read a fixed 32-byte array (for public keys).
  readBytes32(): Uint8Array {
    return this.readBytes(32);
  }

  /// Read Option<T>: varint(0=None, 1=Some) + T if Some.
  readOption<T>(readT: () => T): T | null {
    const tag = this.readVarint();
    if (tag === 0) return null;
    return readT();
  }
}

export class BincodeWriter {
  private parts: Uint8Array[] = [];
  private totalLen = 0;

  private append(data: Uint8Array) {
    this.parts.push(data);
    this.totalLen += data.length;
  }

  writeU8(v: number) {
    this.writeVarint(v);
  }

  writeBool(v: boolean) {
    this.writeVarint(v ? 1 : 0);
  }

  /// In bincode v2 standard config, u16 is varint-encoded.
  writeU16(v: number) {
    this.writeVarint(v);
  }

  /// In bincode v2 standard config, u32 is varint-encoded.
  writeU32(v: number) {
    this.writeVarint(v);
  }

  /// In bincode v2 standard config, u64 is varint-encoded.
  writeU64(v: number) {
    this.writeVarint(v);
  }

  writeVarint(v: number) {
    if (v <= 250) {
      this.append(new Uint8Array([v & 0xff]));
    } else if (v <= 0xffff) {
      const buf = new Uint8Array(3);
      buf[0] = 251;
      new DataView(buf.buffer).setUint16(1, v, true);
      this.append(buf);
    } else if (v <= 0xffffffff) {
      const buf = new Uint8Array(5);
      buf[0] = 252;
      new DataView(buf.buffer).setUint32(1, v, true);
      this.append(buf);
    } else {
      const buf = new Uint8Array(9);
      buf[0] = 253;
      new DataView(buf.buffer).setUint32(1, v & 0xffffffff, true);
      new DataView(buf.buffer).setUint32(
        5,
        Math.floor(v / 0x100000000) & 0xffffffff,
        true,
      );
      this.append(buf);
    }
  }

  writeString(s: string) {
    const bytes = new TextEncoder().encode(s);
    this.writeVarint(bytes.length);
    this.append(bytes);
  }

  writeVecU8(data: Uint8Array) {
    this.writeVarint(data.length);
    this.append(data);
  }

  writeBytes32(data: Uint8Array) {
    if (data.length !== 32) throw new Error("expected 32 bytes");
    this.append(data);
  }

  writeOption<T>(v: T | null, writeT: (v: T) => void) {
    if (v === null) {
      this.writeVarint(0);
    } else {
      this.writeVarint(1);
      writeT(v);
    }
  }

  toBytes(): Uint8Array {
    const result = new Uint8Array(this.totalLen);
    let offset = 0;
    for (const part of this.parts) {
      result.set(part, offset);
      offset += part.length;
    }
    return result;
  }
}
