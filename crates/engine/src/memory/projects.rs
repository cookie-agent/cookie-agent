//! The memory folders of projects (`docs/site/specs/projects.md` section 3):
//! `memory/projects/<name>/`, which the project registry
//! ([`crate::projects`]) moves, empties, and gives an `overview` memory whose
//! description the daemon owns.
//!
//! Every function that changes a folder takes the held memory lock as proof,
//! as the machine folders' do.

use std::{fs, io, path::PathBuf};

use super::{
    FolderMoves, MEMORY_EXTENSION, MemoryError, MemoryLock, MemoryRoot, PROJECTS_DIR,
    agents::with_frontmatter,
    file::MemoryDocument,
    store::{io_error, read_bytes, read_dir_sorted, write_file},
    url::{MemoryName, MemoryUrl},
};

/// The memory every project has, loaded in full into its sessions.
pub(crate) const OVERVIEW: &str = "overview";

/// The description the daemon gives a project's overview.
pub(crate) fn overview_description(project: &str) -> String {
    format!("Overview of the `{project}` project, loaded in full into its sessions")
}

/// The URL of `project`'s overview.
pub(crate) fn overview_url(project: &MemoryName) -> MemoryUrl {
    MemoryUrl::Project {
        project: project.clone(),
        name: MemoryName::new(OVERVIEW).expect("`overview` is a memory name"),
    }
}

/// Whether `url` is a project's overview.
pub(crate) fn is_overview(url: &MemoryUrl) -> bool {
    matches!(url, MemoryUrl::Project { name, .. } if name.as_str() == OVERVIEW)
}

/// `document` with the daemon's description for the overview of `project`.
pub(crate) fn managed_overview(
    project: &MemoryName,
    document: &MemoryDocument,
) -> Result<MemoryDocument, super::file::InvalidMemory> {
    MemoryDocument::new(
        overview_description(project.as_str()),
        document.keywords().to_vec(),
        document.body(),
    )
}

impl MemoryRoot {
    /// `memory/projects/<project>/`.
    pub(crate) fn project_dir(&self, project: &MemoryName) -> PathBuf {
        self.root.join(PROJECTS_DIR).join(project.as_str())
    }

    /// The folders under `memory/projects/` whose names are project names,
    /// in name order.
    pub(crate) fn project_folders(&self) -> Result<Vec<MemoryName>, MemoryError> {
        Ok(read_dir_sorted(&self.root.join(PROJECTS_DIR))?
            .into_iter()
            .filter(|(_, kind)| kind.is_dir())
            .filter_map(|(name, _)| MemoryName::new(name))
            .collect())
    }

    /// The memory files (`*.md`) directly in a project's folder; none when
    /// it has no folder.
    pub(crate) fn project_memory_count(&self, project: &MemoryName) -> usize {
        count_memories(&self.project_dir(project))
    }

    /// Makes sure `project`'s overview exists and carries the daemon's
    /// description: a missing file is created with an empty body, and a
    /// valid file with another description has its frontmatter rewritten,
    /// its keywords and body kept. An invalid file, something other than a
    /// regular file, or a rewrite past `max_file_bytes` is left alone, and
    /// so is everything while the memory folder does not exist. Returns
    /// whether it wrote anything, which it signals.
    pub(crate) fn ensure_overview(
        &self,
        _lock: &MemoryLock,
        project: &MemoryName,
        max_file_bytes: usize,
    ) -> Result<bool, MemoryError> {
        if !self.root.is_dir() {
            return Ok(false);
        }
        let path = self.file_path(&overview_url(project));
        let description = overview_description(project.as_str());
        let text = match read_bytes(&path)? {
            None => Some(
                MemoryDocument::new(description, Vec::new(), "")
                    .expect("the overview's frontmatter is valid")
                    .render(),
            ),
            Some((_, metadata)) if !metadata.is_file() => None,
            Some((bytes, _)) => MemoryDocument::parse(&bytes, max_file_bytes)
                .ok()
                .filter(|document| document.description() != description)
                .and_then(|document| managed_overview(project, &document).ok())
                .and_then(|managed| {
                    std::str::from_utf8(&bytes)
                        .ok()
                        .and_then(|text| with_frontmatter(text, &managed))
                })
                .filter(|text| text.len() <= max_file_bytes),
        };
        let Some(text) = text else {
            return Ok(false);
        };
        write_file(&path, text.as_bytes())?;
        self.changed();
        Ok(true)
    }

    /// Moves `memory/projects/<from>/` to `<to>/`. Nothing moves when
    /// `from` has no folder; a `to` folder that already exists fails.
    pub(crate) fn rename_project_folder(
        &self,
        _lock: &MemoryLock,
        from: &MemoryName,
        to: &MemoryName,
    ) -> Result<FolderMoves, MemoryError> {
        let (source, target) = (self.project_dir(from), self.project_dir(to));
        if fs::symlink_metadata(&target).is_ok() {
            return Err(MemoryError::Exists(target));
        }
        let mut moves = FolderMoves::default();
        if fs::symlink_metadata(&source).is_err() {
            return Ok(moves);
        }
        fs::rename(&source, &target).map_err(io_error(&source))?;
        moves.renamed.push((source, target));
        self.changed();
        Ok(moves)
    }

    /// Deletes `memory/projects/<project>/` with everything in it, memory
    /// files or not, and returns how many memory files it held. Nothing
    /// happens when it has no folder.
    pub(crate) fn remove_project_folder(
        &self,
        _lock: &MemoryLock,
        project: &MemoryName,
    ) -> Result<u64, MemoryError> {
        let path = self.project_dir(project);
        let count = count_memories(&path) as u64;
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                self.changed();
                Ok(count)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(MemoryError::Io { path, source }),
        }
    }
}

fn count_memories(dir: &std::path::Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| {
                std::path::Path::new(&entry.file_name())
                    .extension()
                    .is_some_and(|extension| extension == MEMORY_EXTENSION)
            })
            .count()
    })
}

#[cfg(test)]
mod tests;
