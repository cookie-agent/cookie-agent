//! Reading a folder of a machine into a store
//! (`docs/site/specs/folder-artifacts.md` sections 3 and 7): the runtime
//! sends it as a tar stream (`fs/read_open` of a folder), which is kept in
//! an anonymous temporary file, then unpacked one file at a time into the
//! store's blobs, so no more than one file is in memory at once.

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read as _, Seek as _},
};

use cookie_agent_protocol::{
    ASSET_MAX_BYTES, FOLDER_MAX_BYTES, FOLDER_MAX_ENTRIES, FolderEntry, FolderManifest,
    human_bytes,
    runtime_protocol::{FsKind, MAX_FS_CHUNK_BYTES},
    validate_folder_path,
};

use crate::runtimes::RuntimeClient;

/// The most bytes of tar stream a folder at the limits takes: its files,
/// a header and padding for each, and long-name records.
const MAX_TAR_BYTES: u64 = FOLDER_MAX_BYTES + FOLDER_MAX_ENTRIES as u64 * 3 * 512 + (1 << 20);
/// How many links are followed one after another before a link is left out.
const MAX_LINK_HOPS: usize = 8;
/// The main file a folder shows as when none is named, in this order.
const DEFAULT_MAINS: [&str; 3] = ["index.html", "README.md", "index.md"];

/// Why a folder could not be read.
#[derive(Debug)]
pub(crate) enum FolderReadError {
    /// No main file was named and the folder has none of the defaults.
    NoMain,
    /// The named main file is not in the folder.
    MissingMain(String),
    /// Anything else, said for the model and the user.
    Failed(String),
}

impl FolderReadError {
    /// The failure as one sentence; `name_main` says how the caller names a
    /// main file (`main="…"` for a tag, `main` for a tool).
    pub(crate) fn describe(&self, name_main: &str) -> String {
        match self {
            Self::NoMain => format!(
                "the folder has no index.html, README.md, or index.md, and more than one file \
                 at its top; name its main file with {name_main}"
            ),
            Self::MissingMain(main) => format!("the folder has no file {main:?} to show"),
            Self::Failed(reason) => reason.clone(),
        }
    }
}

impl From<String> for FolderReadError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

/// The folder `route` names, read from `client` into a temporary file. The
/// route is used up either way.
pub(crate) async fn read_tar(client: &RuntimeClient, route: u64) -> Result<std::fs::File, String> {
    use base64::Engine as _;
    use std::io::Write as _;

    let opened = client.fs_read_open(route).await?;
    if opened.kind != FsKind::Folder {
        client.fs_close(opened.handle).await;
        return Err("it is not a folder".into());
    }
    let mut file = match tempfile::tempfile() {
        Ok(file) => file,
        Err(error) => {
            client.fs_close(opened.handle).await;
            return Err(format!(
                "the folder could not be kept while it is read: {error}"
            ));
        }
    };
    let mut written = 0u64;
    loop {
        let chunk = match client
            .fs_read_chunk(opened.handle, MAX_FS_CHUNK_BYTES)
            .await
        {
            Ok(chunk) => chunk,
            Err(error) => {
                client.fs_close(opened.handle).await;
                return Err(error);
            }
        };
        let data = match base64::engine::general_purpose::STANDARD.decode(chunk.data.as_bytes()) {
            Ok(data) => data,
            Err(error) => {
                client.fs_close(opened.handle).await;
                return Err(format!(
                    "the runtime sent data that does not decode: {error}"
                ));
            }
        };
        written += data.len() as u64;
        if written > MAX_TAR_BYTES {
            client.fs_close(opened.handle).await;
            return Err(format!(
                "its files hold more than the {} a folder may",
                human_bytes(FOLDER_MAX_BYTES)
            ));
        }
        if let Err(error) = file.write_all(&data) {
            client.fs_close(opened.handle).await;
            return Err(format!(
                "the folder could not be kept while it is read: {error}"
            ));
        }
        if chunk.summary.is_some() {
            break;
        }
    }
    file.rewind()
        .map_err(|error| format!("the folder could not be read back: {error}"))?;
    Ok(file)
}

/// Unpacks the tar stream in `file` into a store: `store` keeps each
/// file's bytes and returns their digest. `main` names the main file, else
/// the first of [`DEFAULT_MAINS`] the folder has, else its only file at its
/// top. Blocking.
pub(crate) fn unpack(
    file: std::fs::File,
    main: Option<&str>,
    mut store: impl FnMut(&[u8]) -> Result<String, String>,
) -> Result<FolderManifest, FolderReadError> {
    let mut archive = tar::Archive::new(file);
    let mut files: BTreeMap<String, FolderEntry> = BTreeMap::new();
    let mut links: Vec<(String, String)> = Vec::new();
    let mut bytes = 0u64;
    let entries = archive
        .entries()
        .map_err(|error| format!("the folder's stream does not read: {error}"))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("the folder's stream does not read: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("a name in the folder does not read: {error}"))?;
        let Some(path) = path
            .to_str()
            .map(|path| path.trim_end_matches('/').to_owned())
        else {
            return Err(FolderReadError::Failed(
                "a name in the folder is not UTF-8".into(),
            ));
        };
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        validate_folder_path(&path).map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            if let Some(target) = entry
                .link_name()
                .ok()
                .flatten()
                .and_then(|target| target.to_str().map(str::to_owned))
            {
                links.push((path, target));
            }
            continue;
        }
        if !(kind.is_file() || kind == tar::EntryType::Continuous) {
            continue;
        }
        let size = entry.header().size().unwrap_or(u64::MAX);
        if size > ASSET_MAX_BYTES {
            return Err(FolderReadError::Failed(format!(
                "{path} holds {}, more than the {} one file may",
                human_bytes(size),
                human_bytes(ASSET_MAX_BYTES)
            )));
        }
        bytes = bytes.saturating_add(size);
        if bytes > FOLDER_MAX_BYTES {
            return Err(FolderReadError::Failed(format!(
                "its files hold more than the {} a folder may",
                human_bytes(FOLDER_MAX_BYTES)
            )));
        }
        if files.len() >= FOLDER_MAX_ENTRIES {
            return Err(too_many(files.len() + 1, &files));
        }
        let mut content = Vec::with_capacity(size as usize);
        entry
            .read_to_end(&mut content)
            .map_err(|error| format!("{path} does not read: {error}"))?;
        let sha256 = store(&content)?;
        let media_type = media_type(&path, &content).to_owned();
        files.insert(
            path.clone(),
            FolderEntry {
                path,
                sha256,
                byte_length: content.len() as u64,
                media_type,
            },
        );
    }
    resolve_links(&mut files, &links)?;
    if files.is_empty() {
        return Err(FolderReadError::Failed("the folder holds no files".into()));
    }
    let main = match main {
        Some(main) => {
            let main = main.trim_start_matches("./").trim_end_matches('/');
            if !files.contains_key(main) {
                return Err(FolderReadError::MissingMain(main.to_owned()));
            }
            main.to_owned()
        }
        None => default_main(&files).ok_or(FolderReadError::NoMain)?,
    };
    FolderManifest::new(main, files.into_values().collect())
        .map_err(|error| FolderReadError::Failed(error.to_string()))
}

/// The main file of `files` when none is named.
fn default_main(files: &BTreeMap<String, FolderEntry>) -> Option<String> {
    if let Some(main) = DEFAULT_MAINS.iter().find(|main| files.contains_key(**main)) {
        return Some((*main).to_owned());
    }
    let mut top = files.keys().filter(|path| !path.contains('/'));
    match (top.next(), top.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

/// Adds the files `links` lead to inside the folder: a link to a file is
/// that file again, and a link to a folder of it is each file under it. A
/// link that leaves the folder, or leads to nothing, is left out.
fn resolve_links(
    files: &mut BTreeMap<String, FolderEntry>,
    links: &[(String, String)],
) -> Result<(), FolderReadError> {
    let targets: HashMap<&str, Option<String>> = links
        .iter()
        .map(|(path, target)| (path.as_str(), join_inside(path, target)))
        .collect();
    for (path, _) in links {
        let mut target = targets.get(path.as_str()).cloned().flatten();
        // A link to a link is followed, a few hops at most.
        for _ in 0..MAX_LINK_HOPS {
            match target.as_deref().and_then(|next| targets.get(next)) {
                Some(next) => target = next.clone(),
                None => break,
            }
        }
        let Some(target) = target else { continue };
        if let Some(entry) = files.get(&target).cloned() {
            files.insert(
                path.clone(),
                FolderEntry {
                    path: path.clone(),
                    ..entry
                },
            );
        } else {
            let prefix = format!("{target}/");
            let under: Vec<FolderEntry> = files
                .range(prefix.clone()..)
                .take_while(|(name, _)| name.starts_with(&prefix))
                .map(|(_, entry)| entry.clone())
                .collect();
            for entry in under {
                let alias = format!("{path}/{}", &entry.path[prefix.len()..]);
                if validate_folder_path(&alias).is_ok() {
                    files.insert(
                        alias.clone(),
                        FolderEntry {
                            path: alias,
                            ..entry
                        },
                    );
                }
            }
        }
        if files.len() > FOLDER_MAX_ENTRIES {
            return Err(too_many(files.len(), files));
        }
    }
    Ok(())
}

/// The path `target`, a link at `link`, leads to inside the folder, or
/// `None` when it is absolute or climbs out.
fn join_inside(link: &str, target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = link.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The refusal of a folder with more than [`FOLDER_MAX_ENTRIES`] files,
/// naming its largest subfolders by count.
fn too_many(count: usize, files: &BTreeMap<String, FolderEntry>) -> FolderReadError {
    let mut by_folder: HashMap<&str, usize> = HashMap::new();
    for path in files.keys() {
        if let Some((folder, _)) = path.split_once('/') {
            *by_folder.entry(folder).or_default() += 1;
        }
    }
    let mut largest: Vec<_> = by_folder.into_iter().collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let named = largest
        .iter()
        .take(3)
        .map(|(folder, count)| format!("{folder}/ ({count}+ files)"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut reason = format!(
        "the folder holds more than the {FOLDER_MAX_ENTRIES} files a folder may ({count} so far)"
    );
    if !named.is_empty() {
        reason.push_str(&format!("; the largest of its folders: {named}"));
    }
    FolderReadError::Failed(reason)
}

/// The media type of a folder's file: by content first, then by its
/// extension, which decides for the files a page loads (scripts, styles,
/// data, fonts, WebAssembly), since a browser needs their types.
pub(crate) fn media_type(path: &str, bytes: &[u8]) -> &'static str {
    let recognized = crate::assets::media_type(path, bytes);
    if !matches!(recognized, "text/plain" | "application/octet-stream") {
        return recognized;
    }
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let text = recognized == "text/plain";
    match extension.as_str() {
        "js" | "mjs" | "cjs" if text => "text/javascript",
        "css" if text => "text/css",
        "json" | "map" if text => "application/json",
        "geojson" if text => "application/geo+json",
        "csv" if text => "text/csv",
        "tsv" if text => "text/tab-separated-values",
        "xml" if text => "application/xml",
        "txt" | "log" if text => "text/plain",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => recognized,
    }
}
