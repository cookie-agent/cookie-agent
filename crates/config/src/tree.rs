//! The authored configuration as documents: `config.toml` and the agent
//! documents under `agents/`, keyed by their path below the user root.
//! [`crate::load_from_tree`] decodes and validates a tree with no file
//! access, so a candidate can be checked before anything is written.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use cookie_agent_identity::{AgentId, ConfigRevision};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::{
    ConfigError,
    secure_fs::{
        open_layer_root, open_optional_directory, read_optional_file, read_required_file,
        regular_file_at,
    },
};

/// The engine configuration file below the user root.
pub const CONFIG_FILE: &str = "config.toml";
pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_AGENT_BYTES: u64 = 256 * 1024;

/// A document of the configuration tree: `config.toml`, `agents/<id>.md`
/// (a shared agent), or `agents/<preset>/<id>.md` (a preset's agent). IDs and
/// preset names follow the agent ID grammar.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfigPath(String);

/// What a [`ConfigPath`] names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigDocument<'a> {
    Config,
    Agent {
        preset: Option<&'a str>,
        id: &'a str,
    },
}

impl ConfigPath {
    /// The engine configuration file.
    #[must_use]
    pub fn config() -> Self {
        Self(CONFIG_FILE.to_owned())
    }

    /// Validates a path below the user root.
    ///
    /// # Errors
    ///
    /// [`ConfigError::UnsafePath`] for anything but the three document forms,
    /// [`ConfigError::AgentFilename`] for an agent file whose name is not an
    /// agent ID, and [`ConfigError::AgentPresetName`] for such a preset.
    pub fn new(path: &str) -> Result<Self, ConfigError> {
        if path == CONFIG_FILE {
            return Ok(Self::config());
        }
        let parts = path.split('/').collect::<Vec<_>>();
        if parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        {
            return Err(ConfigError::UnsafePath);
        }
        let (preset, file) = match parts.as_slice() {
            ["agents", file] => (None, *file),
            ["agents", preset, file] => (Some(*preset), *file),
            _ => return Err(ConfigError::UnsafePath),
        };
        if let Some(preset) = preset {
            AgentId::new(preset).map_err(|_| ConfigError::AgentPresetName {
                path: PathBuf::from("agents").join(preset),
                name: preset.to_owned(),
            })?;
        }
        let id = file.strip_suffix(".md").ok_or(ConfigError::UnsafePath)?;
        AgentId::new(id).map_err(|_| ConfigError::AgentFilename(file.to_owned()))?;
        Ok(Self(path.to_owned()))
    }

    fn agent(preset: Option<&str>, file: &str) -> Result<Self, ConfigError> {
        Self::new(&match preset {
            Some(preset) => format!("agents/{preset}/{file}"),
            None => format!("agents/{file}"),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn document(&self) -> ConfigDocument<'_> {
        if self.0 == CONFIG_FILE {
            return ConfigDocument::Config;
        }
        let rest = self
            .0
            .strip_prefix("agents/")
            .expect("validated agent path");
        let (preset, file) = match rest.split_once('/') {
            Some((preset, file)) => (Some(preset), file),
            None => (None, rest),
        };
        ConfigDocument::Agent {
            preset,
            id: file.strip_suffix(".md").expect("validated agent path"),
        }
    }

    fn limit(&self) -> u64 {
        if self.0 == CONFIG_FILE {
            MAX_CONFIG_BYTES
        } else {
            MAX_AGENT_BYTES
        }
    }
}

impl std::fmt::Display for ConfigPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The configuration documents of one user root.
#[derive(Clone, Debug, Default)]
pub struct ConfigTree {
    root: Option<PathBuf>,
    documents: BTreeMap<ConfigPath, Zeroizing<Vec<u8>>>,
    /// Preset directories that hold no document; a preset exists while its
    /// directory does.
    empty_presets: BTreeSet<String>,
}

impl ConfigTree {
    /// An empty tree for `root`, which names documents in errors.
    #[must_use]
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    /// Reads the documents below `root`; a missing root is an empty tree.
    ///
    /// # Errors
    ///
    /// A root or `agents/` that is not a directory, an unreadable or oversized
    /// document, and names that are not documents: a preset directory holding
    /// anything but `.md` files, or a directory below a preset.
    pub fn read(root: Option<&Path>) -> Result<Self, ConfigError> {
        let mut tree = Self::new(root.map(Path::to_path_buf));
        let Some(root) = root else {
            return Ok(tree);
        };
        let Some(root) = open_layer_root(root)? else {
            return Ok(tree);
        };
        if let Some(bytes) = read_optional_file(&root.path, CONFIG_FILE, MAX_CONFIG_BYTES)? {
            tree.documents.insert(ConfigPath::config(), bytes);
        }
        let Some(agents) = open_optional_directory(&root.path, "agents")? else {
            return Ok(tree);
        };
        for name in sorted_entry_names(&agents)? {
            if name.ends_with(".md") {
                tree.read_agent(&agents, None, &name)?;
                continue;
            }
            if regular_file_at(&agents, &name)? {
                continue;
            }
            let directory =
                open_optional_directory(&agents, &name)?.ok_or(ConfigError::UnsafePath)?;
            AgentId::new(&name).map_err(|_| ConfigError::AgentPresetName {
                path: directory.clone(),
                name: name.clone(),
            })?;
            let names = sorted_entry_names(&directory)?;
            if names.is_empty() {
                tree.empty_presets.insert(name.clone());
            }
            for document in names {
                if !document.ends_with(".md") {
                    return Err(ConfigError::UnsafePath);
                }
                tree.read_agent(&directory, Some(&name), &document)?;
            }
        }
        Ok(tree)
    }

    fn read_agent(
        &mut self,
        directory: &Path,
        preset: Option<&str>,
        name: &str,
    ) -> Result<(), ConfigError> {
        if !regular_file_at(directory, name)? {
            return Err(ConfigError::UnsafePath);
        }
        let path = ConfigPath::agent(preset, name)?;
        let bytes = read_required_file(directory, name, MAX_AGENT_BYTES)?;
        self.documents.insert(path, bytes);
        Ok(())
    }

    /// The user root the tree belongs to.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Where `path` lives on disk, for messages; relative without a root.
    #[must_use]
    pub fn display_path(&self, path: &ConfigPath) -> PathBuf {
        self.root.as_ref().map_or_else(
            || PathBuf::from(path.as_str()),
            |root| root.join(path.as_str()),
        )
    }

    #[must_use]
    pub fn get(&self, path: &ConfigPath) -> Option<&[u8]> {
        self.documents.get(path).map(|bytes| bytes.as_slice())
    }

    /// Every document, in path order.
    pub fn documents(&self) -> impl Iterator<Item = (&ConfigPath, &[u8])> {
        self.documents
            .iter()
            .map(|(path, bytes)| (path, bytes.as_slice()))
    }

    /// Preset directories without documents.
    pub fn empty_presets(&self) -> impl Iterator<Item = &str> {
        self.empty_presets.iter().map(String::as_str)
    }

    /// Creates or replaces a document.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TooLarge`] beyond the document's size limit.
    pub fn put(&mut self, path: ConfigPath, bytes: Vec<u8>) -> Result<(), ConfigError> {
        if bytes.len() as u64 > path.limit() {
            return Err(ConfigError::TooLarge(path.0));
        }
        if let ConfigDocument::Agent {
            preset: Some(preset),
            ..
        } = path.document()
        {
            self.empty_presets.remove(preset);
        }
        self.documents.insert(path, Zeroizing::new(bytes));
        Ok(())
    }

    /// Removes a document; whether it existed. Removing a preset's last
    /// document removes the preset.
    pub fn delete(&mut self, path: &ConfigPath) -> bool {
        self.documents.remove(path).is_some()
    }

    /// Whether `preset` exists: it has a document or an empty directory.
    #[must_use]
    pub fn has_preset(&self, preset: &str) -> bool {
        self.empty_presets.contains(preset)
            || self.documents.keys().any(|path| {
                matches!(path.document(), ConfigDocument::Agent { preset: Some(name), .. } if name == preset)
            })
    }

    /// Sets the key `key` of `config.toml` (a path of table and key names)
    /// to `value`, or removes it when `value` is `None`, editing the text so
    /// the rest of the file, comments included, stays as it is. Missing
    /// tables on the way are created; a table value is written as a table
    /// section where its parent allows one.
    ///
    /// # Errors
    ///
    /// An empty key, a `config.toml` that is not valid TOML, or a key below
    /// something that is not a table.
    pub fn set_key(
        &mut self,
        key: &[String],
        value: Option<toml::Value>,
    ) -> Result<(), ConfigError> {
        let path = self.display_path(&ConfigPath::config());
        let invalid = |detail: &str| ConfigError::Toml(format!("{}: {detail}", path.display()));
        let (last, parents) = key.split_last().ok_or_else(|| invalid("empty key"))?;
        let text = std::str::from_utf8(self.get(&ConfigPath::config()).unwrap_or_default())
            .map_err(|_| invalid("content is not UTF-8"))?;
        let mut document = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| invalid(&error.to_string()))?;
        let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
        // Within an inline table (or a dotted key) only values fit.
        let mut values_only = false;
        for (depth, name) in parents.iter().enumerate() {
            let entry = table.entry(name).or_insert_with(|| {
                let mut created = toml_edit::Table::new();
                created.set_implicit(true);
                toml_edit::Item::Table(created)
            });
            values_only = values_only
                || entry.is_inline_table()
                || entry.as_table().is_some_and(toml_edit::Table::is_dotted);
            table = entry.as_table_like_mut().ok_or_else(|| {
                invalid(&format!("`{}` is not a table", parents[..=depth].join(".")))
            })?;
        }
        match value {
            None => {
                table.remove(last);
            }
            Some(value) => {
                let mut wrapper = toml::Table::new();
                wrapper.insert("value".to_owned(), value);
                let encoded =
                    toml::to_string(&wrapper).map_err(|error| invalid(&error.to_string()))?;
                let mut encoded = encoded
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| invalid(&error.to_string()))?;
                let item = encoded
                    .as_table_mut()
                    .remove("value")
                    .expect("the wrapper holds the value");
                let item = if values_only {
                    item.into_value()
                        .map(toml_edit::Item::Value)
                        .unwrap_or_else(|item| item)
                } else {
                    item
                };
                table.insert(last, item);
            }
        }
        self.put(ConfigPath::config(), document.to_string().into_bytes())
    }

    /// [`Self::set_key`] with a JSON value: `null` removes the key, and an
    /// object is a table.
    ///
    /// # Errors
    ///
    /// As [`Self::set_key`], and a value TOML cannot hold, such as a `null`
    /// inside an array or object.
    pub fn set_key_json(
        &mut self,
        key: &[String],
        value: &serde_json::Value,
    ) -> Result<(), ConfigError> {
        let value = (!value.is_null())
            .then(|| toml::Value::try_from(value))
            .transpose()
            .map_err(|error| {
                ConfigError::Toml(format!("`{}`: not a TOML value: {error}", key.join(".")))
            })?;
        self.set_key(key, value)
    }

    /// Sets `[mcp.servers.<name>]` to `server`, or removes it.
    ///
    /// # Errors
    ///
    /// As [`Self::set_key`].
    pub fn set_mcp_server(
        &mut self,
        name: &str,
        server: Option<&crate::McpServerConfig>,
    ) -> Result<(), ConfigError> {
        let value = server
            .map(toml::Value::try_from)
            .transpose()
            .map_err(|error| ConfigError::Toml(format!("MCP server `{name}`: {error}")))?;
        self.set_key(
            &["mcp".to_owned(), "servers".to_owned(), name.to_owned()],
            value,
        )
    }

    /// The SHA-256 fingerprint of a document's bytes, `sha256:<hex>`.
    #[must_use]
    pub fn fingerprint(bytes: &[u8]) -> String {
        format!("sha256:{}", hex(&Sha256::digest(bytes)))
    }

    /// The revision of the whole tree: a hash over every document's path and
    /// fingerprint and every empty preset.
    #[must_use]
    pub fn revision(&self) -> ConfigRevision {
        let mut digest = Sha256::new();
        for (path, bytes) in &self.documents {
            digest.update(path.as_str().as_bytes());
            digest.update([0]);
            digest.update(Self::fingerprint(bytes).as_bytes());
            digest.update([0]);
        }
        for preset in &self.empty_presets {
            digest.update(format!("agents/{preset}/").as_bytes());
            digest.update([0]);
        }
        ConfigRevision::new(format!("sha256:{}", hex(&digest.finalize())))
            .expect("a SHA-256 digest is a revision")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sorted_entry_names(directory: &Path) -> Result<Vec<String>, ConfigError> {
    let mut names = std::fs::read_dir(directory)
        .map_err(ConfigError::Io)?
        .map(|entry| {
            entry
                .map_err(ConfigError::Io)?
                .file_name()
                .into_string()
                .map_err(|_| ConfigError::UnsafePath)
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}
