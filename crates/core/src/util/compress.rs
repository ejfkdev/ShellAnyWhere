//! Per-frame lz4 compression for stream data.
//!
//! Compression pipeline:
//!   raw data → compress() → framed message
//!
//! Decompression pipeline:
//!   framed message → decompress()
//!
//! Each frame is compressed independently — no cross-frame state,
//! so clients can join mid-stream without prior context.

/// Compress data with lz4.
/// Returns (compressed_data, true).
pub fn compress(data: &[u8]) -> (Vec<u8>, bool) {
    (lz4_flex::compress_prepend_size(data), true)
}

/// Decompress lz4 data (with 4-byte LE size prefix from compress_prepend_size).
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, lz4_flex::block::DecompressError> {
    lz4_flex::decompress_size_prepended(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data = b"Hello World! ".repeat(300);
        let (compressed, was_compressed) = compress(&data);
        assert!(was_compressed);
        let decompressed = decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_small_payload() {
        let data = b"hello";
        let (compressed, _) = compress(data);
        let decompressed = decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }
}
