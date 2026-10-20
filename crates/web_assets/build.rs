//! Embeds the built web client from `bundle/` (written by
//! `scripts/build-web.sh`, ignored by git), or the placeholder page when no
//! bundle has been built, as a table of paths, content types, and bytes,
//! with a gzip copy of each file that compresses, and the page's
//! Content-Security-Policy, which allows its inline scripts by hash.

use std::{
    env,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use base64::Engine as _;
use sha2::Digest as _;

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
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    let mut table = String::from("&[\n");
    let mut inline_scripts = Vec::new();
    for (index, (path, source)) in files.iter().enumerate() {
        let bytes = fs::read(source).expect("read a bundle file");
        let content_type = content_type(path);
        if path == "index.html" {
            inline_scripts = inline_script_hashes(&String::from_utf8_lossy(&bytes));
        }
        let gzip = compressible(content_type)
            .then(|| gzip(&bytes))
            .filter(|compressed| compressed.len() < bytes.len())
            .map(|compressed| {
                let target = out.join(format!("asset-{index}.gz"));
                fs::write(&target, compressed).expect("write a compressed copy");
                format!("Some(include_bytes!({:?}))", target.display().to_string())
            })
            .unwrap_or_else(|| "None".to_owned());
        writeln!(
            table,
            "    Asset {{ path: {path:?}, content_type: {content_type:?}, immutable: {}, etag: {:?}, bytes: include_bytes!({:?}), gzip: {gzip} }},",
            hashed(path),
            etag(&bytes),
            source.display().to_string(),
        )
        .expect("write to a string");
    }
    table.push(']');
    let policy = content_security_policy(&inline_scripts);
    fs::write(
        out.join("assets.rs"),
        format!(
            "pub(crate) const BUILT: bool = {built};\npub(crate) const ASSETS: &[Asset] = {table};\npub(crate) const POLICY: &str = {policy:?};\n"
        ),
    )
    .expect("write the asset table");
}

/// The page's Content-Security-Policy: everything from its own origin, its
/// inline scripts by hash, WebAssembly, `blob:` media read from the daemon,
/// and WebSocket connections to any daemon, since the page may name one on
/// another host. Inline style attributes carry terminal colours.
fn content_security_policy(inline_scripts: &[String]) -> String {
    let mut scripts = String::from("'self' 'wasm-unsafe-eval'");
    for hash in inline_scripts {
        scripts.push_str(&format!(" 'sha256-{hash}'"));
    }
    format!(
        "default-src 'self'; script-src {scripts}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' blob: data:; media-src 'self' blob:; font-src 'self'; \
         connect-src 'self' ws: wss:; object-src 'none'; base-uri 'none'; \
         frame-ancestors 'none'; form-action 'none'"
    )
}

/// The base64 SHA-256 of each inline `<script>` of `html`.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let attributes = &after[..open_end];
        let body = &after[open_end + 1..];
        let Some(close) = body.find("</script>") else {
            break;
        };
        if !attributes.contains("src=") {
            hashes.push(
                base64::engine::general_purpose::STANDARD
                    .encode(sha2::Sha256::digest(&body.as_bytes()[..close])),
            );
        }
        rest = &body[close..];
    }
    hashes
}

fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/wasm" | "application/json" | "application/manifest+json" | "image/svg+xml"
        )
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).expect("compress in memory");
    encoder.finish().expect("compress in memory")
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
