//! Unix descriptor-anchored filesystem capabilities used by prepared tools.

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{OsStr, OsString},
        fs::{self, File},
        io::{Read, Seek, SeekFrom, Write},
        os::fd::AsRawFd,
        os::unix::fs::MetadataExt,
        path::{Component, Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use cookie_agent_engine::ToolError;
    use cookie_agent_protocol::Sha256Digest;
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RawMode};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    static PREPARED_WEIGHT: AtomicUsize = AtomicUsize::new(0);
    static PREPARED_WEIGHT_LIMIT: AtomicUsize = AtomicUsize::new(16_384);
    static ATOMIC_WRITES_POISONED: AtomicBool = AtomicBool::new(false);

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct AtomicWriteOutcome {
        pub cleanup_warning: Option<String>,
    }

    struct BudgetReservation {
        weight: usize,
    }

    impl BudgetReservation {
        fn reserve(weight: usize) -> Result<Self, ToolError> {
            let limit = PREPARED_WEIGHT_LIMIT.load(Ordering::Acquire);
            reserve_weight(&PREPARED_WEIGHT, limit, weight)?;
            Ok(Self { weight })
        }
    }

    pub(super) fn reserve_weight(
        counter: &AtomicUsize,
        limit: usize,
        weight: usize,
    ) -> Result<(), ToolError> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(weight).filter(|next| *next <= limit)
            })
            .map(|_| ())
            .map_err(|_| {
                ToolError::resource_limit(format!(
                    "aggregate prepared-object weight would exceed {limit}"
                ))
            })
    }

    impl Drop for BudgetReservation {
        fn drop(&mut self) {
            PREPARED_WEIGHT.fetch_sub(self.weight, Ordering::AcqRel);
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ObjectIdentity {
        pub device: u64,
        pub inode: u64,
        pub mode: u32,
        pub size: u64,
    }

    impl ObjectIdentity {
        fn from_metadata(metadata: &fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                size: metadata.len(),
            }
        }

        pub fn canonical_bytes(&self) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&self.device.to_be_bytes());
            bytes.extend_from_slice(&self.inode.to_be_bytes());
            bytes.extend_from_slice(&self.mode.to_be_bytes());
            bytes.extend_from_slice(&self.size.to_be_bytes());
            bytes
        }
    }

    #[allow(clippy::unnecessary_cast)]
    fn stat_identity(stat: &rustix::fs::Stat) -> (u64, u64) {
        // rustix exposes native widths; Darwin's dev_t is narrower than MetadataExt's.
        (stat.st_dev as u64, stat.st_ino as u64)
    }

    #[allow(clippy::unnecessary_cast)]
    fn permission_mode(mode: u32) -> Mode {
        // RawMode is u32 on Linux but Darwin's libc mode_t is u16.
        Mode::from_raw_mode((mode & 0o777) as RawMode)
    }

    pub(super) struct ChainNode {
        pub(super) parent: File,
        pub(super) name: OsString,
        pub(super) identity: ObjectIdentity,
    }

    pub struct PreparedExisting {
        pub parent: File,
        pub file: File,
        pub basename: OsString,
        pub display_path: PathBuf,
        pub identity: ObjectIdentity,
        pub content_digest: Sha256Digest,
        pub directory: bool,
        chain: Vec<ChainNode>,
        _budget: BudgetReservation,
    }

    pub struct PreparedAbsent {
        pub parent: File,
        pub basename: OsString,
        pub display_path: PathBuf,
        missing: Vec<OsString>,
        chain: Vec<ChainNode>,
        _budget: BudgetReservation,
    }

    pub enum PreparedTarget {
        Existing(PreparedExisting),
        Absent(PreparedAbsent),
    }

    impl PreparedTarget {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            match self {
                Self::Existing(target) => target.revalidate(),
                Self::Absent(target) => target.revalidate(),
            }
        }

        pub fn serialization_bytes(&self) -> Result<Vec<u8>, ToolError> {
            let mut bytes = Vec::new();
            match self {
                Self::Existing(target) => {
                    bytes.extend_from_slice(&target.identity.device.to_be_bytes());
                    bytes.extend_from_slice(&target.identity.inode.to_be_bytes());
                }
                Self::Absent(target) => {
                    let parent = ObjectIdentity::from_metadata(
                        &target.parent.metadata().map_err(super::io_error)?,
                    );
                    bytes.extend_from_slice(&parent.device.to_be_bytes());
                    bytes.extend_from_slice(&parent.inode.to_be_bytes());
                    bytes.extend_from_slice(target.basename.as_encoded_bytes());
                }
            }
            Ok(bytes)
        }

        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            match self {
                Self::Existing(target) => target.manifest_bytes(),
                Self::Absent(target) => target.manifest_bytes(),
            }
        }
    }

    pub fn cwd_context_bytes(cwd: &Path) -> Result<Vec<u8>, ToolError> {
        let directory = open_absolute_directory(cwd)?;
        Ok(
            ObjectIdentity::from_metadata(&directory.metadata().map_err(super::io_error)?)
                .canonical_bytes(),
        )
    }

    pub fn ensure_atomic_write_supported() -> Result<(), ToolError> {
        #[cfg(target_os = "linux")]
        {
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ToolError::unsupported_platform(
                "atomic expected-target replacement is unsupported on this platform",
            ))
        }
    }

    pub fn prepare_existing(cwd: &Path, requested: &Path) -> Result<PreparedExisting, ToolError> {
        match prepare_target(cwd, requested)? {
            PreparedTarget::Existing(existing) => Ok(existing),
            PreparedTarget::Absent(_) => {
                Err(ToolError::execution("prepared target does not exist"))
            }
        }
    }

    pub fn prepare_target(cwd: &Path, requested: &Path) -> Result<PreparedTarget, ToolError> {
        let absolute = normalize_absolute(if requested.is_absolute() {
            requested.to_owned()
        } else {
            cwd.join(requested)
        })?;
        let weight = absolute.components().count().saturating_add(4);
        let budget = BudgetReservation::reserve(weight)?;
        let basename = absolute
            .file_name()
            .ok_or_else(|| ToolError::unsupported_security("target has no basename"))?
            .to_owned();
        let parent_path = absolute
            .parent()
            .ok_or_else(|| ToolError::unsupported_security("target has no parent"))?;
        let (parent, chain, missing_parent) = open_nearest_directory_chain(parent_path)?;
        if !missing_parent.is_empty() {
            let mut missing = missing_parent;
            missing.push(basename.clone());
            return Ok(PreparedTarget::Absent(PreparedAbsent {
                parent,
                basename,
                display_path: absolute,
                missing,
                chain,
                _budget: budget,
            }));
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rustix::fs::openat(&parent, &basename, flags, Mode::empty()) {
            Ok(fd) => {
                let file = File::from(fd);
                let metadata = file.metadata().map_err(super::io_error)?;
                if !metadata.is_file() && !metadata.is_dir() {
                    return Err(ToolError::unsupported_security(
                        "prepared filesystem object is not a regular file or directory",
                    ));
                }
                let identity = ObjectIdentity::from_metadata(&metadata);
                let content_digest = if metadata.is_dir() {
                    directory_digest(&file)?
                } else {
                    Sha256Digest::of_bytes(&read_all(&file)?)
                };
                Ok(PreparedTarget::Existing(PreparedExisting {
                    parent,
                    file,
                    basename,
                    display_path: absolute,
                    identity,
                    content_digest,
                    directory: metadata.is_dir(),
                    chain,
                    _budget: budget,
                }))
            }
            Err(rustix::io::Errno::NOENT) => Ok(PreparedTarget::Absent(PreparedAbsent {
                parent,
                basename: basename.clone(),
                display_path: absolute,
                missing: vec![basename],
                chain,
                _budget: budget,
            })),
            Err(rustix::io::Errno::LOOP) => Err(ToolError::unsupported_security(
                "prepared target is a symlink",
            )),
            Err(error) => Err(super::io_error(error)),
        }
    }

    impl PreparedExisting {
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            let parent =
                ObjectIdentity::from_metadata(&self.parent.metadata().map_err(super::io_error)?);
            Ok(existing_manifest_bytes(
                &self.chain,
                &parent,
                &self.basename,
                &self.identity,
                self.directory,
                &self.content_digest,
            ))
        }

        pub fn proc_fd_path(&self) -> PathBuf {
            PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }

        pub fn read_bytes(&self) -> Result<Vec<u8>, ToolError> {
            read_all(&self.file)
        }

        pub fn verified_bytes(&self) -> Result<Vec<u8>, ToolError> {
            self.revalidate()?;
            let bytes = read_all(&self.file)?;
            if Sha256Digest::of_bytes(&bytes) != self.content_digest {
                return Err(ToolError::operation_changed(
                    "prepared target content changed",
                ));
            }
            Ok(bytes)
        }

        pub fn directory_entries(&self) -> Result<Vec<(String, bool)>, ToolError> {
            directory_entries(&self.file)
        }

        pub fn revalidate(&self) -> Result<(), ToolError> {
            revalidate_chain(&self.chain)?;
            let stat = rustix::fs::statat(&self.parent, &self.basename, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| {
                    ToolError::operation_changed("prepared target disappeared or changed")
                })?;
            let (device, inode) = stat_identity(&stat);
            if device != self.identity.device || inode != self.identity.inode {
                return Err(ToolError::operation_changed(
                    "prepared target identity changed",
                ));
            }
            let digest = if self.file.metadata().map_err(super::io_error)?.is_dir() {
                directory_digest(&self.file)?
            } else {
                Sha256Digest::of_bytes(&read_all(&self.file)?)
            };
            if digest != self.content_digest {
                return Err(ToolError::operation_changed(
                    "prepared target content changed",
                ));
            }
            Ok(())
        }

        pub fn replace_atomically(&self, bytes: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            self.replace_atomically_inner(bytes, || {})
        }

        pub(super) fn replace_atomically_inner(
            &self,
            bytes: &[u8],
            before_exchange: impl FnOnce(),
        ) -> Result<AtomicWriteOutcome, ToolError> {
            if ATOMIC_WRITES_POISONED.load(Ordering::Acquire) {
                return Err(ToolError::operation_changed(
                    "atomic write subsystem is poisoned after an ambiguous rollback",
                ));
            }
            self.revalidate()?;
            let temporary = stage(&self.parent, bytes, self.identity.mode)?;
            before_exchange();
            if let Err(error) = rename_exchange(&self.parent, &temporary, &self.basename) {
                let _ = rustix::fs::unlinkat(&self.parent, &temporary, AtFlags::empty());
                return Err(ToolError::operation_changed(format!(
                    "target changed during atomic replacement: {}",
                    error.message()
                )));
            }
            let verification = (|| {
                injected_exchange_failure(ExchangeFailurePoint::OpenDisplaced)?;
                let displaced = rustix::fs::openat(
                    &self.parent,
                    &temporary,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(super::io_error)?;
                injected_exchange_failure(ExchangeFailurePoint::StatDisplaced)?;
                let displaced_identity =
                    ObjectIdentity::from_metadata(&displaced.metadata().map_err(super::io_error)?);
                injected_exchange_failure(ExchangeFailurePoint::ReadDisplaced)?;
                let displaced_digest = Sha256Digest::of_bytes(&read_all(&displaced)?);
                injected_exchange_failure(ExchangeFailurePoint::IdentityMismatch)?;
                if displaced_identity.device != self.identity.device
                    || displaced_identity.inode != self.identity.inode
                {
                    return Err(ToolError::operation_changed(
                        "target changed during atomic replacement",
                    ));
                }
                injected_exchange_failure(ExchangeFailurePoint::DigestMismatch)?;
                if displaced_digest != self.content_digest {
                    return Err(ToolError::operation_changed(
                        "target changed during atomic replacement",
                    ));
                }
                injected_exchange_failure(ExchangeFailurePoint::CommitFsync)?;
                rustix::fs::fsync(&self.parent).map_err(super::io_error)
            })();
            if let Err(error) = verification {
                if rollback_exchange(&self.parent, &temporary, &self.basename).is_err() {
                    ATOMIC_WRITES_POISONED.store(true, Ordering::Release);
                    return Err(ToolError::operation_changed(
                        "atomic replacement rollback durability is ambiguous; writes are poisoned",
                    ));
                }
                return Err(ToolError::operation_changed(format!(
                    "target changed during atomic replacement: {}",
                    error.message()
                )));
            }
            // Commit point: the validated exchange is now durable. Cleanup can
            // no longer turn the committed mutation into an ordinary failure.
            if let Err(error) = injected_exchange_failure(ExchangeFailurePoint::CleanupUnlink)
                .and_then(|()| {
                    rustix::fs::unlinkat(&self.parent, &temporary, AtFlags::empty())
                        .map_err(super::io_error)
                })
            {
                return Ok(AtomicWriteOutcome {
                    cleanup_warning: Some(format!(
                        "replacement committed; displaced object remains recoverable as {}: {}",
                        temporary.to_string_lossy(),
                        error.message()
                    )),
                });
            }
            if let Err(error) = injected_exchange_failure(ExchangeFailurePoint::CleanupFsync)
                .and_then(|()| rustix::fs::fsync(&self.parent).map_err(super::io_error))
            {
                return Ok(AtomicWriteOutcome {
                    cleanup_warning: Some(format!(
                        "replacement committed; displaced-object cleanup durability is uncertain: {}",
                        error.message()
                    )),
                });
            }
            Ok(AtomicWriteOutcome::default())
        }
    }

    impl PreparedAbsent {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            revalidate_chain(&self.chain)?;
            let first = self
                .missing
                .first()
                .ok_or_else(|| ToolError::execution("missing subtree is empty"))?;
            match rustix::fs::statat(&self.parent, first, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => Ok(()),
                Ok(_) => Err(ToolError::operation_changed(
                    "a prepared missing path component was inserted",
                )),
                Err(error) => Err(super::io_error(error)),
            }
        }

        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            let mut bytes = b"expected-absent\0".to_vec();
            for node in &self.chain {
                append_tagged_os_string(&mut bytes, b"ancestor-name", &node.name);
                bytes.extend_from_slice(b"ancestor-identity\0");
                bytes.extend_from_slice(&node.identity.canonical_bytes());
            }
            bytes.extend_from_slice(b"parent-identity\0");
            bytes.extend_from_slice(
                &ObjectIdentity::from_metadata(&self.parent.metadata().map_err(super::io_error)?)
                    .canonical_bytes(),
            );
            for component in &self.missing {
                append_tagged_os_string(&mut bytes, b"missing-component", component);
            }
            Ok(bytes)
        }

        pub fn create_atomically(&self, bytes: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            self.create_atomically_inner(bytes, || {})
        }

        pub(super) fn create_atomically_inner(
            &self,
            bytes: &[u8],
            before_publish: impl FnOnce(),
        ) -> Result<AtomicWriteOutcome, ToolError> {
            if ATOMIC_WRITES_POISONED.load(Ordering::Acquire) {
                return Err(ToolError::operation_changed(
                    "atomic write subsystem is poisoned after an ambiguous rollback",
                ));
            }
            self.revalidate()?;
            let first = self
                .missing
                .first()
                .ok_or_else(|| ToolError::execution("missing subtree is empty"))?;
            if self.missing.len() == 1 {
                let temporary = stage(&self.parent, bytes, 0o100600)?;
                before_publish();
                if let Err(error) = rename_noreplace(&self.parent, &temporary, first) {
                    let _ = rustix::fs::unlinkat(&self.parent, &temporary, AtFlags::empty());
                    return Err(error);
                }
            } else {
                publish_missing_subtree(&self.parent, &self.missing, bytes, before_publish)?;
            }
            rustix::fs::fsync(&self.parent).map_err(super::io_error)?;
            Ok(AtomicWriteOutcome::default())
        }
    }

    fn rollback_exchange(
        parent: &File,
        temporary: &OsStr,
        target: &OsStr,
    ) -> Result<(), ToolError> {
        rename_exchange(parent, temporary, target)?;
        rustix::fs::unlinkat(parent, temporary, AtFlags::empty()).map_err(super::io_error)?;
        rustix::fs::fsync(parent).map_err(super::io_error)
    }

    fn open_absolute_directory(path: &Path) -> Result<File, ToolError> {
        open_absolute_directory_chain(path).map(|(file, _)| file)
    }

    fn normalize_absolute(path: PathBuf) -> Result<PathBuf, ToolError> {
        if !path.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem path is not absolute",
            ));
        }
        let mut normalized = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => normalized.push(name),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(ToolError::unsupported_security(
                            "filesystem path traverses above its root",
                        ));
                    }
                }
                Component::Prefix(_) => {
                    return Err(ToolError::unsupported_security(
                        "filesystem path has an unsupported prefix",
                    ));
                }
            }
        }
        Ok(normalized)
    }

    fn open_nearest_directory_chain(
        path: &Path,
    ) -> Result<(File, Vec<ChainNode>, Vec<OsString>), ToolError> {
        if !path.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem anchor is not absolute",
            ));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut current =
            File::from(rustix::fs::open("/", flags, Mode::empty()).map_err(super::io_error)?);
        let mut chain = Vec::new();
        let mut missing = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) if missing.is_empty() => {
                    match rustix::fs::openat(&current, name, flags, Mode::empty()) {
                        Ok(fd) => {
                            let next = File::from(fd);
                            let identity = ObjectIdentity::from_metadata(
                                &next.metadata().map_err(super::io_error)?,
                            );
                            chain.push(ChainNode {
                                parent: current.try_clone().map_err(super::io_error)?,
                                name: name.to_owned(),
                                identity,
                            });
                            current = next;
                        }
                        Err(rustix::io::Errno::NOENT) => missing.push(name.to_owned()),
                        Err(rustix::io::Errno::LOOP) => {
                            return Err(ToolError::unsupported_security(
                                "prepared path contains a symlink",
                            ));
                        }
                        Err(error) => return Err(super::io_error(error)),
                    }
                }
                Component::Normal(name) => missing.push(name.to_owned()),
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(ToolError::unsupported_security(
                        "parent traversal is not supported by prepared filesystem operations",
                    ));
                }
            }
        }
        Ok((current, chain, missing))
    }

    fn open_absolute_directory_chain(path: &Path) -> Result<(File, Vec<ChainNode>), ToolError> {
        let (file, chain, missing) = open_nearest_directory_chain(path)?;
        if missing.is_empty() {
            Ok((file, chain))
        } else {
            Err(ToolError::execution("prepared directory does not exist"))
        }
    }

    fn revalidate_chain(chain: &[ChainNode]) -> Result<(), ToolError> {
        for node in chain {
            let stat = rustix::fs::statat(&node.parent, &node.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| ToolError::operation_changed("prepared ancestor changed"))?;
            let (device, inode) = stat_identity(&stat);
            if device != node.identity.device || inode != node.identity.inode {
                return Err(ToolError::operation_changed(
                    "prepared ancestor identity changed",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn existing_manifest_bytes(
        chain: &[ChainNode],
        parent: &ObjectIdentity,
        basename: &OsStr,
        leaf: &ObjectIdentity,
        directory: bool,
        digest: &Sha256Digest,
    ) -> Vec<u8> {
        let mut bytes = b"expected-existing\0".to_vec();
        for node in chain {
            append_tagged_os_string(&mut bytes, b"ancestor-name", &node.name);
            bytes.extend_from_slice(b"ancestor-identity\0");
            bytes.extend_from_slice(&node.identity.canonical_bytes());
        }
        bytes.extend_from_slice(b"parent-identity\0");
        bytes.extend_from_slice(&parent.canonical_bytes());
        append_tagged_os_string(&mut bytes, b"basename", basename);
        bytes.extend_from_slice(b"leaf-identity\0");
        bytes.extend_from_slice(&leaf.canonical_bytes());
        bytes.extend_from_slice(if directory {
            b"leaf-type\0directory\0"
        } else {
            b"leaf-type\0file\0"
        });
        bytes.extend_from_slice(b"leaf-content-digest\0");
        bytes.extend_from_slice(digest.as_str().as_bytes());
        bytes.push(0);
        bytes
    }

    fn append_tagged_os_string(bytes: &mut Vec<u8>, tag: &[u8], value: &OsStr) {
        bytes.extend_from_slice(tag);
        bytes.push(0);
        bytes.extend_from_slice(&(value.as_encoded_bytes().len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_encoded_bytes());
    }

    fn read_all(file: &File) -> Result<Vec<u8>, ToolError> {
        let mut file = file.try_clone().map_err(super::io_error)?;
        file.seek(SeekFrom::Start(0)).map_err(super::io_error)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(super::io_error)?;
        Ok(bytes)
    }

    fn directory_entries(file: &File) -> Result<Vec<(String, bool)>, ToolError> {
        let mut directory = Dir::read_from(file).map_err(super::io_error)?;
        let mut entries = Vec::new();
        for entry in &mut directory {
            let entry = entry.map_err(super::io_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), "." | "..") {
                continue;
            }
            entries.push((name, entry.file_type() == FileType::Directory));
        }
        entries.sort();
        Ok(entries)
    }

    fn directory_digest(file: &File) -> Result<Sha256Digest, ToolError> {
        serde_json::to_vec(&directory_entries(file)?)
            .map(|bytes| Sha256Digest::of_bytes(&bytes))
            .map_err(super::io_error)
    }

    fn publish_missing_subtree(
        anchor: &File,
        missing: &[OsString],
        bytes: &[u8],
        before_publish: impl FnOnce(),
    ) -> Result<(), ToolError> {
        let temporary = OsString::from(format!(
            ".cookie-agent-subtree-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        rustix::fs::mkdirat(anchor, &temporary, Mode::from_raw_mode(0o700))
            .map_err(super::io_error)?;
        let cleanup_path = PathBuf::from(format!(
            "/proc/self/fd/{}/{}",
            anchor.as_raw_fd(),
            temporary.to_string_lossy()
        ));
        let result = (|| {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let mut current = File::from(
                rustix::fs::openat(anchor, &temporary, flags, Mode::empty())
                    .map_err(super::io_error)?,
            );
            for component in &missing[1..missing.len() - 1] {
                rustix::fs::mkdirat(&current, component, Mode::from_raw_mode(0o700))
                    .map_err(super::io_error)?;
                let next = File::from(
                    rustix::fs::openat(&current, component, flags, Mode::empty())
                        .map_err(super::io_error)?,
                );
                rustix::fs::fsync(&current).map_err(super::io_error)?;
                current = next;
            }
            let leaf = missing
                .last()
                .ok_or_else(|| ToolError::execution("missing subtree has no leaf"))?;
            let fd = rustix::fs::openat(
                &current,
                leaf,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(super::io_error)?;
            let mut file = File::from(fd);
            file.write_all(bytes).map_err(super::io_error)?;
            file.sync_all().map_err(super::io_error)?;
            rustix::fs::fsync(&current).map_err(super::io_error)?;
            before_publish();
            rename_noreplace(anchor, &temporary, &missing[0])
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&cleanup_path);
        }
        result
    }

    fn stage(parent: &File, bytes: &[u8], mode: u32) -> Result<OsString, ToolError> {
        let name = OsString::from(format!(
            ".cookie-agent-stage-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let fd = rustix::fs::openat(
            parent,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            permission_mode(mode),
        )
        .map_err(super::io_error)?;
        let mut file = File::from(fd);
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = rustix::fs::unlinkat(parent, &name, AtFlags::empty());
            return Err(super::io_error(error));
        }
        Ok(name)
    }

    #[cfg(target_os = "linux")]
    fn rename_exchange(parent: &File, old: &OsStr, new: &OsStr) -> Result<(), ToolError> {
        rustix::fs::renameat_with(parent, old, parent, new, rustix::fs::RenameFlags::EXCHANGE)
            .map_err(super::io_error)
    }

    #[cfg(not(target_os = "linux"))]
    fn rename_exchange(_: &File, _: &OsStr, _: &OsStr) -> Result<(), ToolError> {
        Err(ToolError::unsupported_platform(
            "atomic expected-target exchange is unsupported on this Unix platform",
        ))
    }

    #[cfg(target_os = "linux")]
    fn rename_noreplace(parent: &File, old: &OsStr, new: &OsStr) -> Result<(), ToolError> {
        rustix::fs::renameat_with(parent, old, parent, new, rustix::fs::RenameFlags::NOREPLACE)
            .map_err(|error| {
                if error == rustix::io::Errno::EXIST {
                    ToolError::operation_changed("prepared absent path was inserted before commit")
                } else {
                    super::io_error(error)
                }
            })
    }

    #[cfg(not(target_os = "linux"))]
    fn rename_noreplace(_: &File, _: &OsStr, _: &OsStr) -> Result<(), ToolError> {
        Err(ToolError::unsupported_platform(
            "atomic no-replace is unsupported on this Unix platform",
        ))
    }

    #[cfg(test)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum ExchangeFailurePoint {
        OpenDisplaced,
        StatDisplaced,
        ReadDisplaced,
        IdentityMismatch,
        DigestMismatch,
        CommitFsync,
        CleanupUnlink,
        CleanupFsync,
    }

    #[cfg(not(test))]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ExchangeFailurePoint {
        OpenDisplaced,
        StatDisplaced,
        ReadDisplaced,
        IdentityMismatch,
        DigestMismatch,
        CommitFsync,
        CleanupUnlink,
        CleanupFsync,
    }

    #[cfg(test)]
    std::thread_local! {
        static EXCHANGE_FAILURE: std::cell::Cell<Option<ExchangeFailurePoint>> = const { std::cell::Cell::new(None) };
    }

    #[cfg(test)]
    pub(super) fn inject_exchange_failure(point: ExchangeFailurePoint) {
        EXCHANGE_FAILURE.set(Some(point));
    }

    #[cfg(test)]
    fn injected_exchange_failure(point: ExchangeFailurePoint) -> Result<(), ToolError> {
        let injected = EXCHANGE_FAILURE.get() == Some(point);
        if injected {
            EXCHANGE_FAILURE.set(None);
            if matches!(
                point,
                ExchangeFailurePoint::IdentityMismatch | ExchangeFailurePoint::DigestMismatch
            ) {
                Err(ToolError::operation_changed(format!("injected {point:?}")))
            } else {
                Err(ToolError::execution(format!("injected {point:?}")))
            }
        } else {
            Ok(())
        }
    }

    #[cfg(not(test))]
    fn injected_exchange_failure(_: ExchangeFailurePoint) -> Result<(), ToolError> {
        Ok(())
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows {
    //! Windows path-based filesystem capabilities.
    //!
    //! Documented gaps versus the Unix descriptor backend: Windows validation
    //! cannot provide TOCTOU immunity between revalidation and mutation, and
    //! Windows has no atomic two-file exchange for expected-target rollback.

    use std::{
        fs,
        io::Write,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt as _, io::AsRawHandle as _},
        path::{Component, Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use cookie_agent_engine::ToolError;
    use cookie_agent_protocol::Sha256Digest;
    use windows_sys::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            MoveFileExW,
        },
    };

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ObjectIdentity {
        pub device: u64,
        pub inode: u64,
        pub mode: u32,
        pub size: u64,
    }
    impl ObjectIdentity {
        pub fn canonical_bytes(&self) -> Vec<u8> {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&self.device.to_be_bytes());
            bytes.extend_from_slice(&self.inode.to_be_bytes());
            bytes.extend_from_slice(&self.mode.to_be_bytes());
            bytes.extend_from_slice(&self.size.to_be_bytes());
            bytes
        }
    }
    pub struct PreparedExisting {
        pub display_path: PathBuf,
        pub identity: ObjectIdentity,
        pub content_digest: Sha256Digest,
        pub directory: bool,
        original_path: PathBuf,
        canonical_path: PathBuf,
        sandbox_root: PathBuf,
    }
    pub struct PreparedAbsent {
        pub display_path: PathBuf,
        original_path: PathBuf,
        first_missing: PathBuf,
        sandbox_root: PathBuf,
    }
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct AtomicWriteOutcome {
        pub cleanup_warning: Option<String>,
    }
    pub enum PreparedTarget {
        Existing(PreparedExisting),
        Absent(PreparedAbsent),
    }
    impl PreparedTarget {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            match self {
                Self::Existing(target) => target.revalidate(),
                Self::Absent(target) => target.revalidate(),
            }
        }
        pub fn serialization_bytes(&self) -> Result<Vec<u8>, ToolError> {
            self.manifest_bytes()
        }
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            match self {
                Self::Existing(target) => target.manifest_bytes(),
                Self::Absent(target) => target.manifest_bytes(),
            }
        }
    }
    impl PreparedExisting {
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            let mut bytes = b"windows-existing\0".to_vec();
            append_path(&mut bytes, &self.sandbox_root);
            append_path(&mut bytes, &self.canonical_path);
            bytes.extend_from_slice(&self.identity.canonical_bytes());
            bytes.extend_from_slice(self.content_digest.as_str().as_bytes());
            Ok(bytes)
        }
        pub fn read_bytes(&self) -> Result<Vec<u8>, ToolError> {
            if self.directory {
                return Err(ToolError::execution("prepared target is a directory"));
            }
            fs::read(&self.canonical_path).map_err(super::io_error)
        }
        pub fn verified_bytes(&self) -> Result<Vec<u8>, ToolError> {
            self.revalidate()?;
            let bytes = self.read_bytes()?;
            if Sha256Digest::of_bytes(&bytes) != self.content_digest {
                return Err(ToolError::operation_changed(
                    "prepared target content changed",
                ));
            }
            Ok(bytes)
        }
        pub fn directory_entries(&self) -> Result<Vec<(String, bool)>, ToolError> {
            if !self.directory {
                return Err(ToolError::execution("prepared target is not a directory"));
            }
            directory_entries(&self.canonical_path)
        }
        pub fn revalidate(&self) -> Result<(), ToolError> {
            validate_path_chain(&self.original_path, false)?;
            let canonical = self.original_path.canonicalize().map_err(super::io_error)?;
            if !paths_equal(&canonical, &self.canonical_path) {
                return Err(ToolError::operation_changed(
                    "prepared target resolved to another path",
                ));
            }
            validate_contained_path(&self.sandbox_root, &self.canonical_path, false)?;
            let file = open_for_identity(&self.canonical_path)?;
            let identity = identity(&file)?;
            if identity.device != self.identity.device
                || identity.inode != self.identity.inode
                || identity.mode != self.identity.mode
            {
                return Err(ToolError::operation_changed(
                    "prepared target identity changed",
                ));
            }
            let digest = content_digest(&self.canonical_path, self.directory)?;
            if digest != self.content_digest {
                return Err(ToolError::operation_changed(
                    "prepared target content changed",
                ));
            }
            Ok(())
        }
        pub fn replace_atomically(&self, bytes: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            if self.directory {
                return Err(ToolError::unsupported_security(
                    "cannot replace a directory with file content",
                ));
            }
            self.revalidate()?;
            let temporary = stage_sibling(&self.canonical_path, bytes)?;
            if let Err(error) = move_file(&temporary, &self.canonical_path, true) {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(AtomicWriteOutcome::default())
        }
        pub fn proc_fd_path(&self) -> PathBuf {
            self.canonical_path.clone()
        }
    }
    impl PreparedAbsent {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            validate_path_chain(&self.original_path, true)?;
            match fs::symlink_metadata(&self.first_missing) {
                Ok(_) => {
                    return Err(ToolError::operation_changed(
                        "an originally missing path component was inserted",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(super::io_error(error)),
            }
            let canonical = canonicalize_target(&self.original_path)?;
            if !paths_equal(&canonical, &self.display_path) {
                return Err(ToolError::operation_changed(
                    "prepared absent target resolved to another path",
                ));
            }
            validate_contained_path(&self.sandbox_root, &canonical, true)?;
            if self.display_path.exists() {
                return Err(ToolError::operation_changed(
                    "a prepared absent path was inserted",
                ));
            }
            Ok(())
        }
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            let mut bytes = b"windows-absent\0".to_vec();
            append_path(&mut bytes, &self.sandbox_root);
            append_path(&mut bytes, &self.display_path);
            append_path(&mut bytes, &self.first_missing);
            Ok(bytes)
        }
        pub fn create_atomically(&self, bytes: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            self.revalidate()?;
            let parent = self
                .display_path
                .parent()
                .ok_or_else(|| ToolError::unsupported_security("target has no parent"))?;
            create_private_parents(&self.sandbox_root, parent)?;
            let temporary = stage_sibling(&self.display_path, bytes)?;
            if let Err(error) = move_file(&temporary, &self.display_path, false) {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(AtomicWriteOutcome::default())
        }
    }
    pub fn cwd_context_bytes(cwd: &Path) -> Result<Vec<u8>, ToolError> {
        validate_path_chain(cwd, false)?;
        let canonical = cwd.canonicalize().map_err(super::io_error)?;
        validate_no_reparse(&canonical)?;
        Ok(identity(&open_for_identity(&canonical)?)?.canonical_bytes())
    }
    pub fn ensure_atomic_write_supported() -> Result<(), ToolError> {
        Ok(())
    }
    pub fn prepare_existing(cwd: &Path, requested: &Path) -> Result<PreparedExisting, ToolError> {
        match prepare_target(cwd, requested)? {
            PreparedTarget::Existing(existing) => Ok(existing),
            PreparedTarget::Absent(_) => {
                Err(ToolError::execution("prepared target does not exist"))
            }
        }
    }
    pub fn prepare_target(cwd: &Path, requested: &Path) -> Result<PreparedTarget, ToolError> {
        let original_path = absolute_requested(cwd, requested)?;
        let sandbox_root = sandbox_root(cwd, &original_path, requested.is_absolute())?;
        validate_path_chain(&original_path, true)?;
        let requested_absolute = canonicalize_target(&original_path)?;
        let target_exists = requested_absolute.exists();
        validate_contained_path(&sandbox_root, &requested_absolute, !target_exists)?;
        if target_exists {
            let canonical_path = requested_absolute;
            let file = open_for_identity(&canonical_path)?;
            let identity = identity(&file)?;
            let directory = file.metadata().map_err(super::io_error)?.is_dir();
            let content_digest = content_digest(&canonical_path, directory)?;
            Ok(PreparedTarget::Existing(PreparedExisting {
                display_path: canonical_path.clone(),
                identity,
                content_digest,
                directory,
                original_path,
                canonical_path,
                sandbox_root,
            }))
        } else {
            let first_missing = first_missing_component(&original_path)?;
            Ok(PreparedTarget::Absent(PreparedAbsent {
                display_path: requested_absolute,
                original_path,
                first_missing,
                sandbox_root,
            }))
        }
    }

    fn first_missing_component(path: &Path) -> Result<PathBuf, ToolError> {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::Normal(_)) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
                Err(error) => return Err(super::io_error(error)),
            }
        }
        Err(ToolError::execution(
            "prepared absent path has no missing component",
        ))
    }

    fn canonicalize_target(path: &Path) -> Result<PathBuf, ToolError> {
        if path.exists() {
            return path.canonicalize().map_err(super::io_error);
        }
        let mut existing = path.to_owned();
        let mut missing = Vec::new();
        while !existing.exists() {
            let name = existing
                .file_name()
                .ok_or_else(|| ToolError::unsupported_security("target has no existing anchor"))?;
            missing.push(name.to_owned());
            existing = existing
                .parent()
                .ok_or_else(|| ToolError::unsupported_security("target has no existing anchor"))?
                .to_owned();
        }
        let mut canonical = existing.canonicalize().map_err(super::io_error)?;
        for component in missing.into_iter().rev() {
            canonical.push(component);
        }
        Ok(canonical)
    }

    fn absolute_requested(cwd: &Path, requested: &Path) -> Result<PathBuf, ToolError> {
        let path = if requested.is_absolute() {
            requested.to_owned()
        } else {
            cwd.join(requested)
        };
        normalize_absolute(&path)
    }

    fn sandbox_root(
        cwd: &Path,
        requested: &Path,
        absolute_request: bool,
    ) -> Result<PathBuf, ToolError> {
        if cwd == Path::new("/") || absolute_request {
            let mut root = PathBuf::new();
            for component in requested.components() {
                match component {
                    Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                    Component::RootDir => {
                        root.push(Path::new("\\"));
                        break;
                    }
                    _ => break,
                }
            }
            validate_path_chain(&root, false)?;
            return root.canonicalize().map_err(super::io_error);
        }
        validate_path_chain(cwd, false)?;
        cwd.canonicalize().map_err(super::io_error)
    }

    fn normalize_absolute(path: &Path) -> Result<PathBuf, ToolError> {
        if !path.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem path is not absolute",
            ));
        }
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => normalized.push(Path::new("\\")),
                Component::CurDir => {}
                Component::Normal(name) => {
                    validate_windows_component(name)?;
                    normalized.push(name);
                }
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(ToolError::unsupported_security(
                            "filesystem path traverses above its root",
                        ));
                    }
                }
            }
        }
        Ok(normalized)
    }

    fn validate_windows_component(name: &std::ffi::OsStr) -> Result<(), ToolError> {
        let name = name.to_string_lossy();
        if name.is_empty()
            || name.ends_with('.')
            || name.ends_with(' ')
            || name.chars().any(|character| {
                matches!(
                    character,
                    '\0' | '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
                )
            })
        {
            Err(ToolError::unsupported_security(
                "filesystem path contains an unsafe Windows component",
            ))
        } else {
            Ok(())
        }
    }

    fn validate_contained_path(
        root: &Path,
        path: &Path,
        allow_missing: bool,
    ) -> Result<(), ToolError> {
        if !components_start_with(path, root) {
            return Err(ToolError::unsupported_security(
                "filesystem target escapes the prepared sandbox",
            ));
        }
        validate_path_chain(path, allow_missing)
    }

    fn validate_path_chain(path: &Path, allow_missing: bool) -> Result<(), ToolError> {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::Normal(_)) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(_) => {
                    if opened_path_is_reparse(&current)? {
                        return Err(ToolError::unsupported_security(
                            "prepared path contains a symlink or junction",
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolError::operation_changed(
                        "prepared filesystem object disappeared",
                    ));
                }
                Err(error) => {
                    return Err(super::io_error(format!(
                        "symlink_metadata failed for {}: {error}",
                        current.display()
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_no_reparse(path: &Path) -> Result<(), ToolError> {
        validate_path_chain(path, false)
    }

    pub(super) fn components_start_with(path: &Path, root: &Path) -> bool {
        let mut path = path.components();
        for root_component in root.components() {
            let Some(path_component) = path.next() else {
                return false;
            };
            if !component_equal(path_component.as_os_str(), root_component.as_os_str()) {
                return false;
            }
        }
        true
    }

    fn component_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
        let normalize = |value: &std::ffi::OsStr| {
            value
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_ascii_lowercase()
        };
        normalize(left) == normalize(right)
    }

    pub(super) fn paths_equal(left: &Path, right: &Path) -> bool {
        let left = left.components().collect::<Vec<_>>();
        let right = right.components().collect::<Vec<_>>();
        left.len() == right.len()
            && left
                .iter()
                .zip(&right)
                .all(|(left, right)| component_equal(left.as_os_str(), right.as_os_str()))
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, ToolError> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(ToolError::unsupported_security(
                "filesystem path contains an invalid character",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    fn opened_path_is_reparse(path: &Path) -> Result<bool, ToolError> {
        let file = open_for_identity(path)?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let handle = file.as_raw_handle() as HANDLE;
        if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
            return Err(super::io_error(format!(
                "GetFileInformationByHandle failed during reparse check for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }

    fn open_for_identity(path: &Path) -> Result<fs::File, ToolError> {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| {
                super::io_error(format!(
                    "CreateFileW failed for {}: {error}",
                    path.display()
                ))
            })
    }

    fn identity(file: &fs::File) -> Result<ObjectIdentity, ToolError> {
        let metadata = file.metadata().map_err(super::io_error)?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let handle = file.as_raw_handle() as HANDLE;
        if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
            return Err(super::io_error(std::io::Error::last_os_error()));
        }
        Ok(ObjectIdentity {
            device: u64::from(information.dwVolumeSerialNumber),
            inode: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
            mode: if metadata.is_dir() {
                0o040755
            } else {
                0o100755
            },
            size: metadata.len(),
        })
    }

    fn content_digest(path: &Path, directory: bool) -> Result<Sha256Digest, ToolError> {
        if directory {
            serde_json::to_vec(&directory_entries(path)?)
                .map(|bytes| Sha256Digest::of_bytes(&bytes))
                .map_err(super::io_error)
        } else {
            fs::read(path)
                .map(|bytes| Sha256Digest::of_bytes(&bytes))
                .map_err(super::io_error)
        }
    }

    fn directory_entries(path: &Path) -> Result<Vec<(String, bool)>, ToolError> {
        let mut entries = fs::read_dir(path)
            .map_err(super::io_error)?
            .map(|entry| {
                let entry = entry.map_err(super::io_error)?;
                let file_type = entry.file_type().map_err(super::io_error)?;
                Ok((
                    entry.file_name().to_string_lossy().into_owned(),
                    file_type.is_dir(),
                ))
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        entries.sort();
        Ok(entries)
    }

    fn create_private_parents(root: &Path, parent: &Path) -> Result<(), ToolError> {
        if !components_start_with(parent, root) {
            return Err(ToolError::unsupported_security(
                "filesystem target escapes the prepared sandbox",
            ));
        }
        let mut current = PathBuf::new();
        for component in parent.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::Normal(_)) {
                continue;
            }
            if !current.exists() {
                fs::create_dir(&current).map_err(|error| {
                    super::io_error(format!(
                        "create_dir failed for {}: {error}",
                        current.display()
                    ))
                })?;
            }
            if opened_path_is_reparse(&current)? {
                return Err(ToolError::unsupported_security(
                    "prepared path contains a symlink or junction",
                ));
            }
        }
        Ok(())
    }

    fn stage_sibling(target: &Path, bytes: &[u8]) -> Result<PathBuf, ToolError> {
        let parent = target
            .parent()
            .ok_or_else(|| ToolError::unsupported_security("target has no parent"))?;
        let name = target
            .file_name()
            .ok_or_else(|| ToolError::unsupported_security("target has no basename"))?;
        let temporary = parent.join(format!(
            ".{}.cookie-stage-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                super::io_error(format!(
                    "CreateFileW failed for staging file {}: {error}",
                    temporary.display()
                ))
            })?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(super::io_error(format!(
                "write or flush failed for staging file {}: {error}",
                temporary.display()
            )));
        }
        Ok(temporary)
    }

    fn move_file(source: &Path, target: &Path, replace: bool) -> Result<(), ToolError> {
        let source_wide = wide_path(source)?;
        let target_wide = wide_path(target)?;
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        if unsafe { MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), flags) } == 0 {
            Err(ToolError::operation_changed(format!(
                "MoveFileExW failed from {} to {}: {}",
                source.display(),
                target.display(),
                std::io::Error::last_os_error()
            )))
        } else {
            Ok(())
        }
    }

    fn append_path(bytes: &mut Vec<u8>, path: &Path) {
        let encoded = path.as_os_str().to_string_lossy();
        bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(encoded.as_bytes());
    }
}

#[cfg(windows)]
pub use windows::*;

fn io_error(error: impl std::fmt::Display) -> cookie_agent_engine::ToolError {
    cookie_agent_engine::ToolError::execution(error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use std::{ffi::OsString, fs, os::unix::fs::symlink, sync::atomic::AtomicUsize};

    use cookie_agent_engine::ToolError;
    use cookie_agent_protocol::Sha256Digest;

    use super::unix::{
        ChainNode, ExchangeFailurePoint, ObjectIdentity, existing_manifest_bytes,
        inject_exchange_failure, reserve_weight,
    };
    use super::{PreparedTarget, prepare_existing, prepare_target};

    #[test]
    fn read_capability_rejects_in_place_leaf_and_ancestor_changes() {
        let root = tempfile::tempdir().expect("tempdir");
        let directory = root.path().join("tree");
        fs::create_dir(&directory).expect("directory");
        let path = directory.join("value.txt");
        fs::write(&path, "alpha").expect("fixture");

        let in_place =
            prepare_existing(root.path(), std::path::Path::new("tree/value.txt")).expect("prepare");
        fs::write(&path, "bravo").expect("change content");
        assert!(matches!(
            in_place.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));

        fs::write(&path, "alpha").expect("restore");
        let leaf = prepare_existing(root.path(), std::path::Path::new("tree/value.txt"))
            .expect("prepare leaf");
        fs::rename(&path, directory.join("old.txt")).expect("rename leaf");
        fs::write(&path, "alpha").expect("replacement leaf");
        assert!(matches!(
            leaf.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));

        fs::remove_file(&path).expect("remove replacement");
        fs::rename(directory.join("old.txt"), &path).expect("restore leaf");
        let ancestor = prepare_existing(root.path(), std::path::Path::new("tree/value.txt"))
            .expect("prepare ancestor");
        fs::rename(&directory, root.path().join("old-tree")).expect("rename ancestor");
        fs::create_dir(&directory).expect("replacement ancestor");
        fs::write(&path, "alpha").expect("replacement content");
        assert!(matches!(
            ancestor.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));
    }

    #[test]
    fn write_capability_rejects_symlink_swaps_and_absent_target_creation() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("target"), "old").expect("fixture");
        let PreparedTarget::Existing(existing) =
            prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
        else {
            panic!("existing target")
        };
        fs::rename(root.path().join("target"), root.path().join("saved")).expect("rename");
        symlink("saved", root.path().join("target")).expect("symlink");
        assert!(matches!(
            existing.replace_atomically(b"new"),
            Err(ToolError::OperationChanged(_))
        ));
        assert_eq!(
            fs::read_to_string(root.path().join("saved")).expect("saved"),
            "old"
        );

        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("new-file")).expect("prepare absent")
        else {
            panic!("absent target")
        };
        fs::write(root.path().join("new-file"), "attacker").expect("create target");
        assert!(matches!(
            absent.create_atomically(b"new"),
            Err(ToolError::OperationChanged(_))
        ));
        assert_eq!(
            fs::read_to_string(root.path().join("new-file")).expect("attacker file"),
            "attacker"
        );
        assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".cookie-agent-stage-")
        }));
    }

    #[test]
    fn atomic_replace_and_no_replace_publish_complete_content() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("existing"), "old").expect("fixture");
        let PreparedTarget::Existing(existing) =
            prepare_target(root.path(), std::path::Path::new("existing")).expect("prepare")
        else {
            panic!("existing")
        };
        existing.replace_atomically(b"new-value").expect("replace");
        assert_eq!(
            fs::read(root.path().join("existing")).expect("read"),
            b"new-value"
        );

        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("absent")).expect("prepare")
        else {
            panic!("absent")
        };
        absent.create_atomically(b"complete").expect("create");
        assert_eq!(
            fs::read(root.path().join("absent")).expect("read"),
            b"complete"
        );
    }

    #[test]
    fn existing_manifest_encoding_is_golden() {
        let root = tempfile::tempdir().expect("root");
        let parent_file = fs::File::open(root.path()).expect("parent");
        let chain = vec![ChainNode {
            parent: parent_file,
            name: OsString::from("ancestor"),
            identity: ObjectIdentity {
                device: 1,
                inode: 2,
                mode: 0o040700,
                size: 3,
            },
        }];
        let bytes = existing_manifest_bytes(
            &chain,
            &ObjectIdentity {
                device: 4,
                inode: 5,
                mode: 0o040700,
                size: 6,
            },
            std::ffi::OsStr::new("leaf"),
            &ObjectIdentity {
                device: 7,
                inode: 8,
                mode: 0o100600,
                size: 9,
            },
            false,
            &Sha256Digest::of_bytes(b"content"),
        );
        assert_eq!(
            Sha256Digest::of_bytes(&bytes).as_str(),
            "d926a6519d1bdf345feac9d76c8d7b9eaee841e843e6bffcc480ee1ec29ae6de"
        );
    }

    #[test]
    fn unsupported_paths_and_capability_drops_do_not_leak_descriptors() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("file"), "value").expect("fixture");
        symlink("file", root.path().join("link")).expect("symlink");
        assert!(matches!(
            prepare_existing(root.path(), std::path::Path::new("link")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
        let PreparedTarget::Absent(missing) =
            prepare_target(root.path(), std::path::Path::new("missing/subtree/file"))
                .expect("prepare missing subtree")
        else {
            panic!("missing subtree target")
        };
        missing
            .create_atomically(b"published")
            .expect("publish subtree");
        assert_eq!(
            fs::read(root.path().join("missing/subtree/file")).expect("published file"),
            b"published"
        );
        let descriptors_for_root = || {
            fs::read_dir("/proc/self/fd")
                .expect("fd directory")
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(root.path()))
                .count()
        };
        let before = descriptors_for_root();
        for _ in 0..128 {
            drop(prepare_existing(root.path(), std::path::Path::new("file")).expect("prepare"));
        }
        let after = descriptors_for_root();
        assert_eq!(
            after, before,
            "prepared descriptors leaked for fixture root"
        );
    }

    #[test]
    fn missing_subtree_fails_if_any_component_is_inserted() {
        let root = tempfile::tempdir().expect("tempdir");
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("one/two/file")).expect("prepare")
        else {
            panic!("absent target")
        };
        fs::create_dir(root.path().join("one")).expect("attacker insertion");
        assert!(matches!(
            absent.create_atomically(b"content"),
            Err(ToolError::OperationChanged(_))
        ));
        assert!(!root.path().join("one/two/file").exists());
    }

    #[test]
    fn write_manifests_distinguish_state_parent_basename_and_preimage() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("a"), "same").expect("a");
        fs::write(root.path().join("b"), "same").expect("b");
        let a = prepare_target(root.path(), std::path::Path::new("a")).expect("prepare a");
        let b = prepare_target(root.path(), std::path::Path::new("b")).expect("prepare b");
        let absent = prepare_target(root.path(), std::path::Path::new("c")).expect("prepare c");
        assert_ne!(
            a.manifest_bytes().expect("manifest"),
            b.manifest_bytes().expect("manifest")
        );
        assert_ne!(
            a.manifest_bytes().expect("manifest"),
            absent.manifest_bytes().expect("manifest")
        );
    }

    #[test]
    fn aggregate_budget_exhaustion_is_reported_without_opening_descriptors() {
        let counter = AtomicUsize::new(7);
        assert!(matches!(
            reserve_weight(&counter, 8, 2),
            Err(ToolError::ResourceLimit(_))
        ));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 7);
    }

    #[test]
    fn exchange_rollback_preserves_racing_symlink_and_cleans_stage() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("target"), "original").expect("target");
        fs::write(root.path().join("attacker"), "attacker").expect("attacker");
        let PreparedTarget::Existing(existing) =
            prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
        else {
            panic!("existing target")
        };
        let target = root.path().join("target");
        let saved = root.path().join("saved");
        let result = existing.replace_atomically_inner(b"new", || {
            fs::rename(&target, &saved).expect("move target");
            symlink("attacker", &target).expect("racing symlink");
        });
        assert!(matches!(result, Err(ToolError::OperationChanged(_))));
        assert_eq!(
            fs::read_link(&target).expect("symlink retained"),
            std::path::PathBuf::from("attacker")
        );
        assert_eq!(
            fs::read_to_string(&saved).expect("original retained"),
            "original"
        );
        assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".cookie-agent-stage-")
        }));
    }

    #[test]
    fn every_exchange_precommit_failure_rolls_back_and_cleans_stage() {
        for point in [
            ExchangeFailurePoint::OpenDisplaced,
            ExchangeFailurePoint::StatDisplaced,
            ExchangeFailurePoint::ReadDisplaced,
            ExchangeFailurePoint::IdentityMismatch,
            ExchangeFailurePoint::DigestMismatch,
            ExchangeFailurePoint::CommitFsync,
        ] {
            let root = tempfile::tempdir().expect("root");
            fs::write(root.path().join("target"), "original").expect("fixture");
            let PreparedTarget::Existing(existing) =
                prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
            else {
                panic!("existing target")
            };
            inject_exchange_failure(point);
            assert!(
                existing.replace_atomically(b"replacement").is_err(),
                "{point:?}"
            );
            assert_eq!(
                fs::read_to_string(root.path().join("target")).expect("target"),
                "original",
                "{point:?}"
            );
            assert!(fs::read_dir(root.path()).expect("directory").all(|entry| {
                !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cookie-agent-stage-")
            }));
        }
    }

    #[test]
    fn postcommit_cleanup_failures_report_committed_success() {
        for point in [
            ExchangeFailurePoint::CleanupUnlink,
            ExchangeFailurePoint::CleanupFsync,
        ] {
            let root = tempfile::tempdir().expect("root");
            fs::write(root.path().join("target"), "original").expect("fixture");
            let PreparedTarget::Existing(existing) =
                prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
            else {
                panic!("existing target")
            };
            inject_exchange_failure(point);
            let outcome = existing
                .replace_atomically(b"replacement")
                .expect("committed success");
            assert!(outcome.cleanup_warning.is_some(), "{point:?}");
            assert_eq!(
                fs::read_to_string(root.path().join("target")).expect("target"),
                "replacement",
                "{point:?}"
            );
        }
    }

    #[test]
    fn absent_leaf_insertion_at_publish_barrier_is_operation_changed() {
        let root = tempfile::tempdir().expect("root");
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("target")).expect("prepare")
        else {
            panic!("absent target")
        };
        let target = root.path().join("target");
        let result = absent.create_atomically_inner(b"new", || {
            fs::write(&target, "attacker").expect("insert target");
        });
        assert!(matches!(result, Err(ToolError::OperationChanged(_))));
        assert_eq!(fs::read_to_string(target).expect("target"), "attacker");
    }

    #[test]
    fn missing_subtree_insertion_at_publish_barrier_is_operation_changed() {
        let root = tempfile::tempdir().expect("root");
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("one/two/target")).expect("prepare")
        else {
            panic!("absent target")
        };
        let inserted = root.path().join("one");
        let result = absent.create_atomically_inner(b"new", || {
            fs::create_dir(&inserted).expect("insert subtree");
            fs::write(inserted.join("attacker"), "attacker").expect("attacker");
        });
        assert!(matches!(result, Err(ToolError::OperationChanged(_))));
        assert_eq!(
            fs::read_to_string(inserted.join("attacker")).expect("attacker"),
            "attacker"
        );
        assert!(!root.path().join("one/two/target").exists());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs;

    use cookie_agent_engine::ToolError;

    use super::{
        PreparedTarget, components_start_with, paths_equal, prepare_existing, prepare_target,
    };

    #[test]
    fn windows_path_comparisons_are_case_insensitive_and_component_bounded() {
        let root = std::path::Path::new(r"c:\users\runneradmin\work");
        let target = std::path::Path::new(r"\\?\C:\Users\RUNNERADMIN\Work\nested\file.txt");
        assert!(components_start_with(target, root));
        assert!(paths_equal(
            std::path::Path::new(r"C:\Users\RunnerAdmin\Work"),
            root
        ));
        assert!(!components_start_with(
            std::path::Path::new(r"C:\Users\runneradmin-other\file.txt"),
            std::path::Path::new(r"C:\Users\runneradmin")
        ));
    }

    #[test]
    fn windows_capability_enforces_prefix_and_publishes_atomically() {
        let root = tempfile::tempdir().expect("sandbox");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(root.path().join("existing.txt"), "old").expect("existing");
        let existing = prepare_existing(root.path(), std::path::Path::new("existing.txt"))
            .expect("prepare existing");
        existing.replace_atomically(b"new").expect("replace");
        assert_eq!(fs::read(root.path().join("existing.txt")).unwrap(), b"new");

        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("nested/new.txt"))
                .expect("prepare absent")
        else {
            panic!("absent target")
        };
        absent.create_atomically(b"created").expect("create");
        assert_eq!(
            fs::read(root.path().join("nested/new.txt")).unwrap(),
            b"created"
        );

        assert!(matches!(
            prepare_target(root.path(), &outside.path().join("escape.txt")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }

    #[test]
    fn windows_capability_rejects_reparse_components() {
        let root = tempfile::tempdir().expect("sandbox");
        let target = root.path().join("target");
        fs::create_dir(&target).expect("target");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir(&target, &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_target(root.path(), std::path::Path::new("link/file.txt")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }

    #[test]
    fn windows_absent_leaf_insertion_is_operation_changed() {
        let root = tempfile::tempdir().expect("sandbox");
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("target.txt")).expect("prepare")
        else {
            panic!("absent target")
        };
        fs::write(root.path().join("target.txt"), "attacker").expect("insert target");
        assert!(matches!(
            absent.create_atomically(b"new"),
            Err(ToolError::OperationChanged(_))
        ));
        assert_eq!(
            fs::read_to_string(root.path().join("target.txt")).unwrap(),
            "attacker"
        );
    }

    #[test]
    fn windows_missing_subtree_insertion_is_operation_changed() {
        let root = tempfile::tempdir().expect("sandbox");
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("one/two/target.txt"))
                .expect("prepare")
        else {
            panic!("absent target")
        };
        fs::create_dir(root.path().join("one")).expect("insert subtree");
        fs::write(root.path().join("one/attacker"), "attacker").expect("attacker");
        assert!(matches!(
            absent.create_atomically(b"new"),
            Err(ToolError::OperationChanged(_))
        ));
        assert!(!root.path().join("one/two/target.txt").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("one/attacker")).unwrap(),
            "attacker"
        );
    }
}
