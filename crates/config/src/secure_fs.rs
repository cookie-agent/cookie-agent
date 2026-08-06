use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use crate::{AgentDocumentSource, ConfigError};
use zeroize::Zeroizing;

pub(crate) struct LayerRoot {
    pub(crate) source: AgentDocumentSource,
    pub(crate) path: PathBuf,
}

pub(crate) fn regular_file_at(parent: &Path, name: &str) -> Result<bool, ConfigError> {
    fs::metadata(parent.join(name))
        .map(|metadata| metadata.is_file())
        .map_err(ConfigError::Io)
}

pub(crate) fn open_layer_root(
    path: &Path,
    source: AgentDocumentSource,
) -> Result<Option<LayerRoot>, ConfigError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(LayerRoot {
            source,
            path: path.to_path_buf(),
        })),
        Ok(_) => Err(ConfigError::UnsafePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

pub(crate) fn open_optional_directory(
    parent: &Path,
    name: &str,
) -> Result<Option<PathBuf>, ConfigError> {
    let path = parent.join(name);
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(path)),
        Ok(_) => Err(ConfigError::UnsafePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

pub(crate) fn read_optional_file(
    parent: &Path,
    name: &str,
    limit: u64,
) -> Result<Option<Zeroizing<Vec<u8>>>, ConfigError> {
    match read_file(parent, name, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(ConfigError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_required_file(
    parent: &Path,
    name: &str,
    limit: u64,
) -> Result<Zeroizing<Vec<u8>>, ConfigError> {
    read_file(parent, name, limit)
}

fn read_file(parent: &Path, name: &str, limit: u64) -> Result<Zeroizing<Vec<u8>>, ConfigError> {
    let path = parent.join(name);
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::NotFound);
        }
        Err(error) => return Err(ConfigError::Io(error)),
    };
    let metadata = file.metadata().map_err(ConfigError::Io)?;
    if metadata.len() > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ConfigError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ConfigError::TooLarge(name.to_owned()));
    }
    Ok(bytes)
}
