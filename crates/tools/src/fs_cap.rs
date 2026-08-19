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
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};

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
            Mode::from_raw_mode(mode & 0o777),
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

#[cfg(not(unix))]
mod unsupported {
    use std::path::{Path, PathBuf};

    use cookie_agent_engine::ToolError;
    use cookie_agent_protocol::Sha256Digest;

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ObjectIdentity {
        pub device: u64,
        pub inode: u64,
        pub mode: u32,
        pub size: u64,
    }
    impl ObjectIdentity {
        pub fn canonical_bytes(&self) -> Vec<u8> {
            Vec::new()
        }
    }
    pub struct PreparedExisting {
        pub display_path: PathBuf,
        pub identity: ObjectIdentity,
        pub content_digest: Sha256Digest,
        pub directory: bool,
    }
    pub struct PreparedAbsent {
        pub display_path: PathBuf,
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
            unsupported()
        }
        pub fn serialization_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
    }
    impl PreparedExisting {
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
        pub fn read_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
        pub fn verified_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
        pub fn directory_entries(&self) -> Result<Vec<(String, bool)>, ToolError> {
            unsupported()
        }
        pub fn revalidate(&self) -> Result<(), ToolError> {
            unsupported()
        }
        pub fn replace_atomically(&self, _: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            unsupported()
        }
        pub fn proc_fd_path(&self) -> PathBuf {
            PathBuf::new()
        }
    }
    impl PreparedAbsent {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            unsupported()
        }
        pub fn manifest_bytes(&self) -> Result<Vec<u8>, ToolError> {
            unsupported()
        }
        pub fn create_atomically(&self, _: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            unsupported()
        }
    }
    pub fn cwd_context_bytes(_: &Path) -> Result<Vec<u8>, ToolError> {
        unsupported()
    }
    pub fn ensure_atomic_write_supported() -> Result<(), ToolError> {
        unsupported()
    }
    pub fn prepare_existing(_: &Path, _: &Path) -> Result<PreparedExisting, ToolError> {
        unsupported()
    }
    pub fn prepare_target(_: &Path, _: &Path) -> Result<PreparedTarget, ToolError> {
        unsupported()
    }
    fn unsupported<T>() -> Result<T, ToolError> {
        Err(ToolError::unsupported_platform(
            "prepared filesystem security is unsupported on non-Unix platforms",
        ))
    }
}

#[cfg(not(unix))]
pub use unsupported::*;

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
