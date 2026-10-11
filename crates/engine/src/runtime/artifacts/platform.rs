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

    use fs2::FileExt as _;
    use rustix::{
        fs::{AtFlags, Mode, OFlags, fsync, linkat, openat, renameat, unlinkat},
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

        /// Creates a new private file and locks it for as long as it is open.
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
            let file = fs::File::from(file);
            file.lock_exclusive()?;
            Ok(file)
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

        /// Makes the directory's entries durable.
        pub(in crate::runtime::artifacts) fn sync(&self) -> std::io::Result<()> {
            Ok(fsync(&self.0)?)
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{fs, path::PathBuf};

    use fs2::FileExt as _;

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

        /// Creates a new private file and locks it for as long as it is open.
        pub(in crate::runtime::artifacts) fn create(
            &self,
            name: &str,
        ) -> std::io::Result<fs::File> {
            let file =
                cookie_agent_models::secure_store::create_windows_private_file(&self.0.join(name))?;
            file.lock_exclusive()?;
            Ok(file)
        }

        /// Opens for reading and writing: refreshing a blob's times needs write
        /// access on Windows.
        pub(in crate::runtime::artifacts) fn open(
            &self,
            name: &str,
        ) -> std::io::Result<Option<fs::File>> {
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.0.join(name))
            {
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
            cookie_agent_models::secure_store::replace_windows_path(
                &self.0.join(from),
                &self.0.join(to),
            )
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

        /// Renames already flush their parent directory on Windows.
        pub(in crate::runtime::artifacts) fn sync(&self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
