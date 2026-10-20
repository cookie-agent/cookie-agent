//! Embeds the built web client from `bundle/` (written by
//! `scripts/build-web.sh`, ignored by git), or the placeholder page when no
//! bundle has been built, as a table of paths, content types, and bytes.

use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let bundle = manifest.join("bundle");
    // The whole crate: watching `bundle/` itself would rerun every build
    // while it does not exist.
    println!("cargo:rerun-if-changed=.");
    let mut files = Vec::new();
    if bundle.join("index.html").is_file() {
        collect(&bundle, &bundle, &mut files);
    }
    files.sort();
    let built = !files.is_empty();
    if !built {
        files.push(("index.html".to_owned(), manifest.join("placeholder.html")));
    }
    let mut table = String::from("&[\n");
    for (path, source) in &files {
        writeln!(
            table,
            "    Asset {{ path: {path:?}, content_type: {:?}, immutable: {}, etag: {:?}, bytes: include_bytes!({:?}) }},",
            content_type(path),
            hashed(path),
            etag(&fs::read(source).expect("read a bundle file")),
            source.display().to_string(),
        )
        .expect("write to a string");
    }
    table.push(']');
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    fs::write(
        out.join("assets.rs"),
        format!("pub(crate) const BUILT: bool = {built};\npub(crate) const ASSETS: &[Asset] = {table};\n"),
    )
    .expect("write the asset table");
}

/// Every file under `directory`, as its `/`-separated path below `root`.
fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(directory).expect("read the bundle");
    for entry in entries {
        let path = entry.expect("bundle entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("below the bundle")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
}

/// A strong `ETag` for `bytes`: their 64-bit FNV-1a hash, quoted.
fn etag(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("\"{hash:016x}\"")
}

/// Whether Trunk named the file after a hash of its content
/// (`name-<16 hex digits>` before `.` or `_`), so it never changes.
fn hashed(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.match_indices('-').any(|(dash, _)| {
        let digits = &name[dash + 1..];
        digits.len() > 16
            && digits.as_bytes()[..16]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            && matches!(digits.as_bytes()[16], b'.' | b'_')
    })
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
