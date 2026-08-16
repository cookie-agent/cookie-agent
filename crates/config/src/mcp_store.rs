use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
};

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table};

use crate::{ConfigError, McpServerConfig, load_from_roots};

#[derive(Serialize)]
struct ServerDocument<'a> {
    mcp: McpDocument<'a>,
}

#[derive(Serialize)]
struct McpDocument<'a> {
    servers: std::collections::BTreeMap<&'a str, &'a McpServerConfig>,
}

/// Replace one `[mcp.servers.<name>]` table while preserving every unrelated
/// item and comment. The candidate is accepted only when the strict loader can
/// parse it as a complete configuration layer.
pub fn write_mcp_server(
    path: &Path,
    name: &str,
    config: &McpServerConfig,
) -> Result<(), ConfigError> {
    write_mcp_server_observed(path, name, config, |_| {})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitStep {
    TemporarySynced,
    IdentityChecked,
    Renamed,
    ParentSynced,
}

fn write_mcp_server_observed(
    path: &Path,
    name: &str,
    config: &McpServerConfig,
    mut observe: impl FnMut(CommitStep),
) -> Result<(), ConfigError> {
    config.validate(name)?;
    let (original, originally_existed) = match fs::read_to_string(path) {
        Ok(text) => (text, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(error) => return Err(ConfigError::Io(error)),
    };
    let mut document = original
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Toml(format!("{}: {error}", path.display())))?;
    let encoded = toml::to_string(&ServerDocument {
        mcp: McpDocument {
            servers: std::collections::BTreeMap::from([(name, config)]),
        },
    })
    .map_err(|error| ConfigError::Toml(format!("{}: {error}", path.display())))?;
    let encoded = encoded
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Toml(format!("{}: {error}", path.display())))?;
    let replacement = encoded["mcp"]["servers"][name].clone();

    if !document.as_table().contains_key("mcp") {
        document["mcp"] = Item::Table(Table::new());
    }
    if !document["mcp"]
        .as_table()
        .is_some_and(|table| table.contains_key("servers"))
    {
        document["mcp"]["servers"] = Item::Table(Table::new());
    }
    document["mcp"]["servers"][name] = replacement;
    let candidate = document.to_string();

    let validation_root = tempfile_root(path, &candidate)?;
    let validation = load_from_roots(Some(&validation_root), None);
    let _ = fs::remove_dir_all(&validation_root);
    validation?;

    let parent = path.parent().ok_or(ConfigError::UnsafePath)?;
    fs::create_dir_all(parent).map_err(ConfigError::Io)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml"),
        std::process::id()
    ));
    let mut open_options = OpenOptions::new();
    open_options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Preserve the existing file's mode (config may hold credentials);
        // default to owner-only when creating a new file.
        let mode = fs::metadata(path)
            .ok()
            .map(|metadata| {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o777
            })
            .unwrap_or(0o600);
        open_options.mode(mode);
    }
    let mut temporary_file = open_options.open(&temporary).map_err(ConfigError::Io)?;
    #[cfg(unix)]
    {
        // A stale temp file from a crashed write keeps its old mode; force it.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        if let Err(error) = temporary_file.set_permissions(fs::Permissions::from_mode(mode)) {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::Io(error));
        }
    }
    if let Err(error) = temporary_file
        .write_all(candidate.as_bytes())
        .and_then(|()| temporary_file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    drop(temporary_file);
    observe(CommitStep::TemporarySynced);

    let unchanged = match fs::read_to_string(path) {
        Ok(current) => originally_existed && current == original,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => !originally_existed,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::Io(error));
        }
    };
    if !unchanged {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::ChangedOnDisk(path.to_owned()));
    }
    observe(CommitStep::IdentityChecked);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    observe(CommitStep::Renamed);
    sync_directory(parent).map_err(ConfigError::Io)?;
    observe(CommitStep::ParentSynced);
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

fn tempfile_root(path: &Path, candidate: &str) -> Result<std::path::PathBuf, ConfigError> {
    let parent = path.parent().ok_or(ConfigError::UnsafePath)?;
    fs::create_dir_all(parent).map_err(ConfigError::Io)?;
    let root = parent.join(format!(".cookie-agent-validate-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).map_err(ConfigError::Io)?;
    }
    fs::create_dir(&root).map_err(ConfigError::Io)?;
    fs::write(root.join("config.toml"), candidate).map_err(ConfigError::Io)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs};

    use super::{CommitStep, write_mcp_server_observed};
    use crate::{ConfigError, McpServerConfig};

    fn server(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some(command.into()),
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            url: None,
            headers: Default::default(),
            enabled: true,
            lazy: false,
            timeout_ms: None,
        }
    }

    #[test]
    fn durable_replace_orders_sync_identity_rename_and_directory_sync() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "# retained\n").expect("initial config");
        let steps = RefCell::new(Vec::new());

        write_mcp_server_observed(&path, "demo", &server("new"), |step| {
            steps.borrow_mut().push(step);
        })
        .expect("durable write");

        assert_eq!(
            *steps.borrow(),
            [
                CommitStep::TemporarySynced,
                CommitStep::IdentityChecked,
                CommitStep::Renamed,
                CommitStep::ParentSynced,
            ]
        );
    }

    #[test]
    fn external_edit_before_commit_is_rejected_without_overwrite() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "# original\n").expect("initial config");

        let error = write_mcp_server_observed(&path, "demo", &server("new"), |step| {
            if step == CommitStep::TemporarySynced {
                fs::write(&path, "# external edit\n").expect("external edit");
            }
        })
        .expect_err("concurrent edit must conflict");

        assert!(matches!(error, ConfigError::ChangedOnDisk(changed) if changed == path));
        assert_eq!(
            fs::read_to_string(&path).expect("read preserved external edit"),
            "# external edit\n"
        );
    }
}
