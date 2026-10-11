//! The few file-system operations an artifact store needs, per platform. All
//! names are relative to one private artifact directory; the store above
//! decides what to do with them.

#[cfg(unix)]
pub(super) use unix::Directory;
#[cfg(windows)]
pub(super) use windows::Directory;

#[cfg(unix)]
mod unix {
    use std::{fs, path::PathBuf};

    use rustix::{
        fs::{AtFlags, Mode, OFlags, linkat, openat, renameat, statat, unlinkat},
        io::Errno,
    };

    /// An open handle on the artifact directory. Every operation goes through
    /// it, so a directory replaced behind the store's back is never followed.
    #[derive(Debug)]
    pub(in crate::runtime::artifacts) struct Directory(fs::File);

    impl Directory {
        /// Opens `path`, creating it private to this user when missing.
        pub(in crate::runtime::artifacts) fn at(path: PathBuf) -> std::io::Result<Self> {
            if !path.exists() {
                use std::os::unix::fs::DirBuilderExt as _;

                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&path)?;
            }
            let handle = rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            Ok(Self(fs::File::from(handle)))
        }

        /// Creates a new private file.
        pub(in crate::runtime::artifacts) fn create(
            &self,
            name: &str,
        ) -> std::io::Result<fs::File> {
            let file = openat(
                &self.0,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            Ok(fs::File::from(file))
        }

        pub(in crate::runtime::artifacts) fn open(
            &self,
            name: &str,
        ) -> std::io::Result<Option<fs::File>> {
            match openat(
                &self.0,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => Ok(Some(fs::File::from(file))),
                Err(Errno::NOENT) => Ok(None),
                Err(error) => Err(error.into()),
            }
        }

        /// A handle for paged reads; any handle pins the file on Unix.
        pub(in crate::runtime::artifacts) fn open_reader(
            &self,
            name: &str,
        ) -> std::io::Result<Option<fs::File>> {
            self.open(name)
        }

        /// Renames `from` to `to`, replacing any file at `to`.
        pub(in crate::runtime::artifacts) fn rename(
            &self,
            from: &str,
            to: &str,
        ) -> std::io::Result<()> {
            Ok(renameat(&self.0, from, &self.0, to)?)
        }

        /// Removes `name`, reporting whether it existed.
        pub(in crate::runtime::artifacts) fn remove(&self, name: &str) -> std::io::Result<bool> {
            match unlinkat(&self.0, name, AtFlags::empty()) {
                Ok(()) => Ok(true),
                Err(Errno::NOENT) => Ok(false),
                Err(error) => Err(error.into()),
            }
        }

        /// Hard-links `name` into `into`; a missing source is skipped and an
        /// existing target kept.
        pub(in crate::runtime::artifacts) fn link_to(
            &self,
            into: &Self,
            name: &str,
        ) -> std::io::Result<()> {
            match linkat(&self.0, name, &into.0, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT | Errno::EXIST) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }

        /// The byte length of `name`, or `None` when it does not exist.
        pub(in crate::runtime::artifacts) fn len(
            &self,
            name: &str,
        ) -> std::io::Result<Option<u64>> {
            match statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => Ok(Some(stat.st_size as u64)),
                Err(Errno::NOENT) => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{fs, path::PathBuf};

    /// The artifact directory's path; Windows has no directory-relative opens.
    #[derive(Debug)]
    pub(in crate::runtime::artifacts) struct Directory(PathBuf);

    impl Directory {
        /// Opens `path`, creating it private to this user when missing.
        pub(in crate::runtime::artifacts) fn at(path: PathBuf) -> std::io::Result<Self> {
            if !path.exists() {
                cookie_agent_models::secure_store::create_windows_private_dir_all(&path)?;
            }
            Ok(Self(path))
        }

        /// Creates a new private file.
        pub(in crate::runtime::artifacts) fn create(
            &self,
            name: &str,
        ) -> std::io::Result<fs::File> {
            cookie_agent_models::secure_store::create_windows_private_file(&self.0.join(name))
        }

        pub(in crate::runtime::artifacts) fn open(
            &self,
            name: &str,
        ) -> std::io::Result<Option<fs::File>> {
            match fs::File::open(self.0.join(name)) {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        /// A handle for paged reads. It shares delete access so a revert can
        /// unlink the blob while a checked-out reader finishes.
        pub(in crate::runtime::artifacts) fn open_reader(
            &self,
            name: &str,
        ) -> std::io::Result<Option<fs::File>> {
            use std::os::windows::fs::OpenOptionsExt as _;

            // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
            const PAGED_READ_SHARE_MODE: u32 = 0x1 | 0x2 | 0x4;
            match fs::OpenOptions::new()
                .read(true)
                .share_mode(PAGED_READ_SHARE_MODE)
                .open(self.0.join(name))
            {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }

        /// Renames `from` to `to`, replacing any file at `to`.
        pub(in crate::runtime::artifacts) fn rename(
            &self,
            from: &str,
            to: &str,
        ) -> std::io::Result<()> {
            // A plain rename: unlike the secure store's replace, it does not
            // flush the directory, and artifacts are not fsynced anyway.
            fs::rename(self.0.join(from), self.0.join(to))
        }

        /// Removes `name`, reporting whether it existed.
        pub(in crate::runtime::artifacts) fn remove(&self, name: &str) -> std::io::Result<bool> {
            match fs::remove_file(self.0.join(name)) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }

        /// Hard-links `name` into `into`; a missing source is skipped and an
        /// existing target kept.
        pub(in crate::runtime::artifacts) fn link_to(
            &self,
            into: &Self,
            name: &str,
        ) -> std::io::Result<()> {
            match fs::hard_link(self.0.join(name), into.0.join(name)) {
                Ok(()) => Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
                    ) =>
                {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }

        /// The byte length of `name`, or `None` when it does not exist.
        pub(in crate::runtime::artifacts) fn len(
            &self,
            name: &str,
        ) -> std::io::Result<Option<u64>> {
            match fs::symlink_metadata(self.0.join(name)) {
                Ok(metadata) => Ok(Some(metadata.len())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }
    }
}
