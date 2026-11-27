//! Content by its digest for the daemon's `/f/` route
//! (`docs/site/specs/folder-artifacts.md` section 6): a session tree's
//! artifact, or an asset's current content, a file or a folder, read as
//! HTTP serves it. The route checks its key before it asks; this checks
//! that the store still has the content.

use cookie_agent_protocol::{AssetStore, FolderManifest, SessionId};

use crate::{Engine, EngineError, runtime::blocking_io};

/// The store content is read from.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ContentStore {
    /// The artifacts of the session tree whose root this is.
    Artifacts(SessionId),
    /// An asset store; the content must be one of its assets' current.
    Assets(AssetStore),
}

impl ContentStore {
    /// The store as a path segment of the route: `a_<root>`, `s_<root>`,
    /// `p_<project>`, or `shared`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Artifacts(root) => format!("a_{root}"),
            Self::Assets(AssetStore::Session { session }) => format!("s_{session}"),
            Self::Assets(AssetStore::Project { project }) => format!("p_{}", project.as_str()),
            Self::Assets(AssetStore::Shared) => "shared".to_owned(),
        }
    }

    /// The store a [`Self::label`] names.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        if label == "shared" {
            return Some(Self::Assets(AssetStore::Shared));
        }
        let (kind, rest) = label.split_once('_')?;
        match kind {
            "a" => Some(Self::Artifacts(rest.parse().ok()?)),
            "s" => Some(Self::Assets(AssetStore::Session {
                session: rest.parse().ok()?,
            })),
            "p" => Some(Self::Assets(AssetStore::Project {
                project: cookie_agent_protocol::ProjectId::new(rest).ok()?,
            })),
            _ => None,
        }
    }
}

/// One file of content, as the route serves it.
#[derive(Clone, Debug)]
pub struct ContentFile {
    /// The blob that holds it.
    pub digest: String,
    pub media_type: String,
    pub byte_length: u64,
}

/// What a digest names in a store.
#[derive(Clone, Debug)]
pub enum ContentEntry {
    File(ContentFile),
    Folder(FolderManifest),
}

impl Engine {
    /// What `sha256` is in `store`: a file, or a folder; `None` when the
    /// store does not hold it as content, such as an asset's earlier
    /// content.
    pub async fn content_entry(
        &self,
        store: ContentStore,
        sha256: String,
    ) -> Result<Option<ContentEntry>, EngineError> {
        if !crate::runtime::artifacts::is_digest_name_common(&sha256) {
            return Ok(None);
        }
        match store {
            ContentStore::Artifacts(root) => {
                let artifacts = std::sync::Arc::clone(&self.inner.artifacts);
                blocking_io::run(move || {
                    let Some(tree) = artifacts.read_store(root)? else {
                        return Ok(None);
                    };
                    match crate::runtime::artifacts::read_folder_manifest(&tree, &sha256) {
                        Ok(Some(manifest)) => Ok(Some(ContentEntry::Folder(manifest))),
                        Ok(None) => {
                            let range = tree.read_range(&sha256, 0, 0)?;
                            Ok(Some(ContentEntry::File(ContentFile {
                                media_type: crate::runtime::artifacts::uploads::content_media_type(
                                    &range.head,
                                    range.total_bytes,
                                )
                                .to_owned(),
                                byte_length: range.total_bytes,
                                digest: sha256,
                            })))
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error),
                    }
                })
                .await
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?
                .or_else(|error: std::io::Error| match error.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData => Ok(None),
                    _ => Err(error.into()),
                })
            }
            ContentStore::Assets(store) => {
                self.assets_blocking(move |_, stores| Ok(stores.content_entry(&store, &sha256)?))
                    .await
            }
        }
    }

    /// Up to `length` bytes from `offset` of blob `digest` of `store`, which
    /// [`Self::content_entry`] named; `None` once it is gone.
    pub async fn content_range(
        &self,
        store: ContentStore,
        digest: String,
        offset: u64,
        length: u64,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        match store {
            ContentStore::Artifacts(root) => {
                let artifacts = std::sync::Arc::clone(&self.inner.artifacts);
                blocking_io::run(move || {
                    let Some(tree) = artifacts.read_store(root)? else {
                        return Ok(None);
                    };
                    match tree.read_range(&digest, offset, length) {
                        Ok(range) => Ok(Some(range.data)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error),
                    }
                })
                .await
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?
                .map_err(EngineError::from)
            }
            ContentStore::Assets(store) => {
                self.assets_blocking(move |_, stores| {
                    Ok(stores.read_blob(&store, &digest, offset, length)?)
                })
                .await
            }
        }
    }
}
