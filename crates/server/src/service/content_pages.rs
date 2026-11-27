//! The daemon's content route (`docs/site/specs/folder-artifacts.md`
//! section 6): `/f/<store>/<sha256>/<key>/<path>` serves a file of a
//! folder, or a file whatever `<path>` is, and `/f/<store>/<sha256>/<key>.zip`
//! a folder as a zip. The key is an HMAC of the daemon token, the store, and
//! the digest, computed as the page ticket is: nothing is stored, and it
//! holds until the content is gone or the token changes. No token, ticket,
//! or origin is checked; every failure answers the same 404.
//!
//! An HTML page reaches its folder's files from an opaque origin, so every
//! answer allows any origin, without credentials. HTML and SVG carry the
//! sandbox policy shared links carry, so a file opened at the top level
//! cannot act as the daemon's origin.

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use cookie_agent_engine::{ContentEntry, ContentStore, Engine};
use cookie_agent_protocol::FolderManifest;

use super::{share_pages, websocket::WebSocketState};

pub(crate) const PREFIX: &str = "/f/";
/// What the key is derived for; a new derivation gets a new label.
const KEY_LABEL: &[u8] = b"cookie-folder/1";
/// The bytes of a key, before base64.
const KEY_BYTES: usize = 16;

/// The key of `sha256` in the store labelled `store`, for daemon token
/// `token`: `HMAC-SHA256(token, label ‖ 0 ‖ store ‖ 0 ‖ sha256)`, cut to
/// [`KEY_BYTES`], base64url. Stateless, so rotating the token changes every
/// key.
pub(crate) fn content_key(token: &str, store: &str, sha256: &str) -> String {
    use base64::Engine as _;
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, token.as_bytes());
    let mut context = aws_lc_rs::hmac::Context::with_key(&key);
    context.update(KEY_LABEL);
    context.update(&[0]);
    context.update(store.as_bytes());
    context.update(&[0]);
    context.update(sha256.as_bytes());
    let tag = context.sign();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&tag.as_ref()[..KEY_BYTES])
}

/// The path, from the daemon's root, of content `sha256` of `store`: a
/// folder's base, ending in `/`, or a file's URL ending in `name`.
pub(crate) fn content_path(
    token: &str,
    store: &ContentStore,
    sha256: &str,
    folder: bool,
    name: &str,
) -> String {
    let label = store.label();
    let key = content_key(token, &label, sha256);
    if folder {
        format!("f/{label}/{sha256}/{key}/")
    } else {
        format!("f/{label}/{sha256}/{key}/{}", percent_encode(name))
    }
}

/// What a path below `/f/` asks for.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Wanted {
    pub(crate) store: String,
    pub(crate) sha256: String,
    pub(crate) key: String,
    /// The zip of a folder.
    pub(crate) zip: bool,
    /// The rest of the path, decoded: a folder's file, or a file's name.
    pub(crate) rest: String,
}

pub(crate) fn wanted(below: &str) -> Option<Wanted> {
    let mut parts = below.splitn(4, '/');
    let store = parts.next()?.to_owned();
    let sha256 = parts.next()?.to_owned();
    let key = parts.next()?;
    let rest = parts.next();
    let (key, zip) = match (key.strip_suffix(".zip"), rest) {
        (Some(key), None) => (key.to_owned(), true),
        (_, Some(_)) => (key.to_owned(), false),
        (None, None) => return None,
    };
    Some(Wanted {
        store,
        sha256,
        key,
        zip,
        rest: percent_decode(rest.unwrap_or_default())?,
    })
}

/// What a path below a share's `f/` asks for: `<store>/<sha256>/<path>`,
/// with no key, since the share is the key.
pub(crate) fn wanted_shared(below: &str) -> Option<Wanted> {
    let mut parts = below.splitn(3, '/');
    let store = parts.next()?.to_owned();
    let sha256 = parts.next()?;
    let rest = parts.next();
    let (sha256, zip) = match (sha256.strip_suffix(".zip"), rest) {
        (Some(sha256), None) => (sha256.to_owned(), true),
        _ => (sha256.to_owned(), false),
    };
    Some(Wanted {
        store,
        sha256,
        key: String::new(),
        zip,
        rest: percent_decode(rest.unwrap_or_default())?,
    })
}

/// A path's rest as a file's name or a folder's path, decoded.
pub(crate) fn decoded(rest: &str) -> Option<String> {
    percent_decode(rest)
}

/// Serves a path below `/f/`.
pub(crate) async fn content_pages(
    State(state): State<WebSocketState>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return gone();
    }
    let Some(below) = request.uri().path().strip_prefix(PREFIX) else {
        return gone();
    };
    let Some(wanted) = wanted(below) else {
        return gone();
    };
    let expected = content_key(state.token.as_str(), &wanted.store, &wanted.sha256);
    if aws_lc_rs::constant_time::verify_slices_are_equal(expected.as_bytes(), wanted.key.as_bytes())
        .is_err()
    {
        return gone();
    }
    let Some(store) = ContentStore::parse(&wanted.store) else {
        return gone();
    };
    let Some(server) = state.server.get() else {
        return gone();
    };
    let engine = &server.engine;
    let download = request
        .uri()
        .query()
        .is_some_and(|query| query.split('&').any(|pair| pair == "download"));
    let name = query_value(request.uri().query(), "name");
    serve(
        engine,
        store,
        &wanted,
        download,
        name,
        request.headers(),
        false,
    )
    .await
}

/// Answers `wanted` of `store`; `shared` serves under a share, whose
/// answers are never cached.
pub(crate) async fn serve(
    engine: &Engine,
    store: ContentStore,
    wanted: &Wanted,
    download: bool,
    name: Option<String>,
    request: &HeaderMap,
    shared: bool,
) -> Response {
    let Ok(Some(entry)) = engine
        .content_entry(store.clone(), wanted.sha256.clone())
        .await
    else {
        return gone();
    };
    let file = match entry {
        ContentEntry::Folder(manifest) if wanted.zip => {
            let name = name
                .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
                .unwrap_or_else(|| "folder".to_owned());
            return zip_response(engine.clone(), store, manifest, name, shared);
        }
        ContentEntry::Folder(manifest) => {
            let path = if wanted.rest.is_empty() {
                manifest.main.clone()
            } else {
                wanted.rest.clone()
            };
            let Some(entry) = manifest.entry(&path) else {
                return gone();
            };
            (
                entry.sha256.clone(),
                entry.media_type.clone(),
                entry.byte_length,
                path.rsplit('/').next().unwrap_or(&path).to_owned(),
            )
        }
        ContentEntry::File(_) if wanted.zip => return gone(),
        ContentEntry::File(file) => {
            let name = if wanted.rest.is_empty() {
                "file".to_owned()
            } else {
                wanted.rest.clone()
            };
            (file.digest, file.media_type, file.byte_length, name)
        }
    };
    let (digest, media_type, total, name) = file;
    let part = share_pages::wanted_part(request, total);
    let (offset, length) = part.extent(total);
    let bytes = if length == 0 {
        Vec::new()
    } else {
        match engine.content_range(store, digest, offset, length).await {
            Ok(Some(bytes)) => bytes,
            _ => return gone(),
        }
    };
    let mut response = share_pages::bytes_response_typed(
        &served_type(&media_type),
        &media_type,
        &name,
        download,
        part,
        total,
        bytes,
    );
    content_headers(response.headers_mut(), shared);
    response
}

/// The headers every content answer adds to a share answer's.
fn content_headers(headers: &mut HeaderMap, shared: bool) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    if !shared {
        // Content-addressed: a URL always names the same bytes.
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=31536000, immutable"),
        );
    }
}

/// A file's `Content-Type`: its own, so scripts, styles, and data load as
/// what they are; text in UTF-8.
fn served_type(media_type: &str) -> String {
    if media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/xml" | "image/svg+xml" | "application/geo+json"
        )
    {
        format!("{media_type}; charset=utf-8")
    } else {
        media_type.to_owned()
    }
}

fn gone() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    (StatusCode::NOT_FOUND, headers).into_response()
}

/// The value of `name` in `query`, decoded.
fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .and_then(|value| percent_decode(&value.replace('+', " ")))
}

/// `text` with `%XX` escapes decoded; `None` when they do not make UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `name` escaped for a path segment.
fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// --- zip --------------------------------------------------------------------

/// A folder as a zip, its files stored as they are under one top folder
/// `name`, streamed as each file is read.
fn zip_response(
    engine: Engine,
    store: ContentStore,
    manifest: FolderManifest,
    name: String,
    shared: bool,
) -> Response {
    struct State {
        engine: Engine,
        store: ContentStore,
        manifest: FolderManifest,
        name: String,
        next: usize,
        offset: u64,
        central: Vec<u8>,
        done: bool,
    }
    let state = State {
        engine,
        store,
        manifest,
        name: name.clone(),
        next: 0,
        offset: 0,
        central: Vec::new(),
        done: false,
    };
    let (time, date) = dos_now();
    let stream = futures_util::stream::unfold(state, move |mut state| async move {
        if state.done {
            return None;
        }
        let Some(entry) = state.manifest.entries.get(state.next).cloned() else {
            let count = u16::try_from(state.manifest.entries.len()).unwrap_or(u16::MAX);
            let mut tail = std::mem::take(&mut state.central);
            let size = tail.len() as u32;
            tail.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
            tail.extend_from_slice(&[0, 0, 0, 0]);
            tail.extend_from_slice(&count.to_le_bytes());
            tail.extend_from_slice(&count.to_le_bytes());
            tail.extend_from_slice(&size.to_le_bytes());
            tail.extend_from_slice(&(state.offset as u32).to_le_bytes());
            tail.extend_from_slice(&[0, 0]);
            state.done = true;
            return Some((Ok::<Bytes, std::io::Error>(Bytes::from(tail)), state));
        };
        state.next += 1;
        let data = match state
            .engine
            .content_range(
                state.store.clone(),
                entry.sha256.clone(),
                0,
                entry.byte_length,
            )
            .await
        {
            Ok(Some(data)) => data,
            _ => {
                state.done = true;
                return Some((
                    Err(std::io::Error::other("a file of the folder is gone")),
                    state,
                ));
            }
        };
        let path = format!("{}/{}", state.name, entry.path);
        let crc = crc32(&data);
        let size = data.len() as u32;
        let mut chunk = Vec::with_capacity(30 + path.len() + data.len());
        chunk.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        chunk.extend_from_slice(&20u16.to_le_bytes());
        chunk.extend_from_slice(&0x0800u16.to_le_bytes());
        chunk.extend_from_slice(&0u16.to_le_bytes());
        chunk.extend_from_slice(&time.to_le_bytes());
        chunk.extend_from_slice(&date.to_le_bytes());
        chunk.extend_from_slice(&crc.to_le_bytes());
        chunk.extend_from_slice(&size.to_le_bytes());
        chunk.extend_from_slice(&size.to_le_bytes());
        chunk.extend_from_slice(&(path.len() as u16).to_le_bytes());
        chunk.extend_from_slice(&0u16.to_le_bytes());
        chunk.extend_from_slice(path.as_bytes());
        chunk.extend_from_slice(&data);
        let central = &mut state.central;
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(path.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&(0o100_644u32 << 16).to_le_bytes());
        central.extend_from_slice(&(state.offset as u32).to_le_bytes());
        central.extend_from_slice(path.as_bytes());
        state.offset += chunk.len() as u64;
        Some((Ok(Bytes::from(chunk)), state))
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(value) = HeaderValue::from_str(&share_pages::disposition(
        "attachment",
        &format!("{name}.zip"),
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let _ = shared;
    (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
}

/// Now as a zip entry's DOS time and date, in UTC.
fn dos_now() -> (u16, u16) {
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let year = u16::try_from(now.year().clamp(1980, 2107) - 1980).unwrap_or(0);
    let time =
        ((now.hour() as u16) << 11) | ((now.minute() as u16) << 5) | ((now.second() as u16) / 2);
    let date = (year << 9) | ((now.month() as u16) << 5) | (now.day() as u16);
    (time, date)
}

/// The CRC-32 (IEEE) of `data`, as zip records it.
fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xEDB8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        table
    });
    !data.iter().fold(!0u32, |crc, byte| {
        table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8)
    })
}
