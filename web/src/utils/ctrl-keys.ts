/// Map a single character to its Ctrl key sequence, or null if no mapping exists.
/// Ctrl+A..Ctrl+Z → 0x01..0x1A, plus [ \ ] @ mappings.

const ESC = "\x1b";

export function ctrlKeySequence(key: string): string | null {
  const ch = key.toLowerCase();
  if (ch.length !== 1) return null;
  const code = ch.charCodeAt(0);
  if (code >= 0x61 && code <= 0x7a) {
    return String.fromCharCode(code - 0x60);
  }
  if (ch === "[") return ESC;
  if (ch === "\\") return "\x1c";
  if (ch === "]") return "\x1d";
  if (ch === "@") return "\x00";
  return null;
}
