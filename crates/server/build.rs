use std::path::Path;

fn main() {
    let web_dist = Path::new("../../web/dist");
    let out_dir = Path::new("web-compressed");

    if !web_dist.exists() {
        println!("cargo:warning=web/dist not found, skipping web asset compression");
        return;
    }

    // Clean output directory
    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir).ok();
    }

    compress_dir(web_dist, web_dist, out_dir);

    // Re-run when web assets change
    println!("cargo:rerun-if-changed=../../web/dist");
}

fn is_compressible(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "css" | "wasm" | "html" | "svg" | "json" | "xml" | "txt")
    )
}

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data).expect("gzip compress failed");
    encoder.finish().expect("gzip finish failed")
}

fn compress_dir(base: &Path, dir: &Path, out_base: &Path) {
    for entry in std::fs::read_dir(dir).expect("failed to read dir") {
        let entry = entry.expect("failed to read entry");
        let path = entry.path();
        let rel = path.strip_prefix(base).expect("strip prefix failed");
        let rel_str = rel.to_str().expect("non-utf8 path");

        // Skip sourcemap files
        if rel_str.ends_with(".map") {
            continue;
        }

        let out_path = out_base.join(rel);

        if path.is_dir() {
            compress_dir(base, &path, out_base);
            continue;
        }

        // Ensure parent directory exists
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create dir");
        }

        let data = std::fs::read(&path).expect("failed to read file");

        if is_compressible(&path) {
            let compressed = gzip_compress(&data);
            std::fs::write(&out_path, &compressed).expect("failed to write compressed file");
        } else {
            std::fs::write(&out_path, &data).expect("failed to write file");
        }
    }
}
