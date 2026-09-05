//! Unix descriptor-anchored filesystem capabilities used by prepared tools.

#[cfg(unix)]
mod unix {
    use std::{
        collections::VecDeque,
        ffi::{OsStr, OsString},
        fs::{self, File},
        io::{Read, Seek, SeekFrom, Write},
        os::fd::AsRawFd,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
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

        fn grow(&mut self, weight: usize) -> Result<(), ToolError> {
            reserve_weight(
                &PREPARED_WEIGHT,
                PREPARED_WEIGHT_LIMIT.load(Ordering::Acquire),
                weight,
            )?;
            self.weight += weight;
            Ok(())
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
        // Keep links and exited directories alive so their inodes cannot be reused.
        pub(super) _object: File,
        pub(super) link_target: Option<OsString>,
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
        let mut budget = BudgetReservation::reserve(4)?;
        let resolved = resolve_path(&normalize_absolute(cwd.to_owned())?, &mut budget)?;
        if !resolved.missing.is_empty() {
            return Err(ToolError::execution("prepared directory does not exist"));
        }
        let directory = File::from(
            rustix::fs::openat(
                &resolved.parent,
                &resolved.basename,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(super::io_error)?,
        );
        revalidate_chain(&resolved.chain)?;
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
        if absolute.file_name().is_none() {
            return Err(ToolError::unsupported_security("target has no basename"));
        }
        let mut budget = BudgetReservation::reserve(4)?;
        let ResolvedPath {
            parent,
            basename,
            chain,
            missing,
        } = resolve_path(&absolute, &mut budget)?;
        if !missing.is_empty() {
            revalidate_chain(&chain)?;
            return Ok(PreparedTarget::Absent(PreparedAbsent {
                parent,
                basename,
                display_path: absolute,
                missing,
                chain,
                _budget: budget,
            }));
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
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
                revalidate_chain(&chain)?;
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
            Err(rustix::io::Errno::LOOP) => Err(ToolError::operation_changed(
                "prepared target became a symlink during resolution",
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
            if let Err(error) = revalidate_chain(&self.chain) {
                let _ = rustix::fs::unlinkat(&self.parent, &temporary, AtFlags::empty());
                return Err(error);
            }
            if let Err(error) = rename_exchange(&self.parent, &temporary, &self.basename) {
                let _ = rustix::fs::unlinkat(&self.parent, &temporary, AtFlags::empty());
                return Err(ToolError::operation_changed(format!(
                    "target changed during atomic replacement: {}",
                    error.message()
                )));
            }
            let verification = (|| {
                revalidate_chain(&self.chain)?;
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
                append_link_manifest(&mut bytes, node);
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
            let before_publish = || {
                before_publish();
                self.revalidate()
            };
            if self.missing.len() == 1 {
                let temporary = stage(&self.parent, bytes, 0o100600)?;
                if let Err(error) = before_publish()
                    .and_then(|()| rename_noreplace(&self.parent, &temporary, first))
                {
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

    struct ResolvedPath {
        parent: File,
        basename: OsString,
        chain: Vec<ChainNode>,
        missing: Vec<OsString>,
    }

    fn resolve_path(
        path: &Path,
        budget: &mut BudgetReservation,
    ) -> Result<ResolvedPath, ToolError> {
        if !path.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem anchor is not absolute",
            ));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut current =
            File::from(rustix::fs::open("/", flags, Mode::empty()).map_err(super::io_error)?);
        let mut chain = Vec::new();
        let mut pending = path
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect::<VecDeque<_>>();
        let mut links = 0;
        let mut steps = 0;
        while let Some(name) = pending.pop_front() {
            steps += 1;
            if steps > 4096 || pending.len() > 4096 {
                return Err(ToolError::resource_limit(
                    "prepared path resolution is too deep",
                ));
            }
            if name == "/" {
                current = File::from(
                    rustix::fs::open("/", flags, Mode::empty()).map_err(super::io_error)?,
                );
                continue;
            }
            if name == "." {
                continue;
            }
            let stat = match rustix::fs::statat(&current, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => {
                    if pending.back().is_some_and(|part| part == ".") {
                        return Err(ToolError::execution(
                            "symlink destination requires a directory",
                        ));
                    }
                    let mut missing = vec![name];
                    for part in pending {
                        if part == ".." {
                            return Err(ToolError::execution(
                                "symlink target traverses a missing directory",
                            ));
                        }
                        if part != "." {
                            missing.push(part);
                        }
                    }
                    budget.grow(missing.len())?;
                    let basename = missing.last().expect("missing leaf").clone();
                    return Ok(ResolvedPath {
                        parent: current,
                        basename,
                        chain,
                        missing,
                    });
                }
                Err(error) => return Err(super::io_error(error)),
            };
            if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
                links += 1;
                if links > 40 {
                    return Err(ToolError::unsupported_security(
                        "too many symlinks in prepared path",
                    ));
                }
                budget.grow(2)?;
                let object = open_link(&current, &name)?;
                let identity =
                    ObjectIdentity::from_metadata(&object.metadata().map_err(super::io_error)?);
                if stat_identity(&stat) != (identity.device, identity.inode) {
                    return Err(ToolError::operation_changed(
                        "symlink changed during resolution",
                    ));
                }
                let target =
                    rustix::fs::readlinkat(&current, &name, Vec::new()).map_err(super::io_error)?;
                let target = OsStr::from_bytes(target.as_bytes()).to_owned();
                chain.push(ChainNode {
                    parent: current.try_clone().map_err(super::io_error)?,
                    name,
                    identity,
                    _object: object,
                    link_target: Some(target.clone()),
                });
                // Expand target components in place: `..` is evaluated from the
                // resolved directory, never lexically across another symlink.
                if target.as_bytes().ends_with(b"/") || target.as_bytes().ends_with(b"/.") {
                    pending.push_front(OsString::from("."));
                }
                for part in Path::new(&target).components().rev() {
                    pending.push_front(part.as_os_str().to_owned());
                }
                continue;
            }
            if pending.is_empty() && name != ".." {
                return Ok(ResolvedPath {
                    parent: current,
                    basename: name,
                    chain,
                    missing: Vec::new(),
                });
            }
            budget.grow(2)?;
            let next = File::from(
                rustix::fs::openat(&current, &name, flags, Mode::empty())
                    .map_err(super::io_error)?,
            );
            let identity =
                ObjectIdentity::from_metadata(&next.metadata().map_err(super::io_error)?);
            if stat_identity(&stat) != (identity.device, identity.inode) {
                return Err(ToolError::operation_changed(
                    "ancestor changed during resolution",
                ));
            }
            chain.push(ChainNode {
                parent: current,
                name,
                identity,
                _object: next.try_clone().map_err(super::io_error)?,
                link_target: None,
            });
            current = next;
        }
        Ok(ResolvedPath {
            parent: current,
            basename: OsString::from("."),
            chain,
            missing: Vec::new(),
        })
    }

    fn open_link(parent: &File, name: &OsStr) -> Result<File, ToolError> {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        #[cfg(target_vendor = "apple")]
        let flags =
            OFlags::from_bits_retain(libc::O_SYMLINK as _) | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_vendor = "apple"
        ))]
        {
            rustix::fs::openat(parent, name, flags, Mode::empty())
                .map(File::from)
                .map_err(super::io_error)
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_vendor = "apple"
        )))]
        {
            let _ = (parent, name);
            Err(ToolError::unsupported_platform(
                "pinning symlink objects is unsupported on this Unix platform",
            ))
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
            if let Some(target) = &node.link_target {
                let actual = rustix::fs::readlinkat(&node.parent, &node.name, Vec::new())
                    .map_err(|_| ToolError::operation_changed("prepared symlink changed"))?;
                if actual.as_bytes() != target.as_encoded_bytes() {
                    return Err(ToolError::operation_changed(
                        "prepared symlink target changed",
                    ));
                }
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
            append_link_manifest(&mut bytes, node);
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

    fn append_link_manifest(bytes: &mut Vec<u8>, node: &ChainNode) {
        if let Some(target) = &node.link_target {
            append_tagged_os_string(bytes, b"symlink-target", target);
        }
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
        before_publish: impl FnOnce() -> Result<(), ToolError>,
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
            before_publish()?;
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
        collections::VecDeque,
        ffi::OsString,
        fs,
        io::Write,
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt as _,
            io::AsRawHandle as _,
        },
        path::{Component, Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use cookie_agent_engine::ToolError;
    use cookie_agent_protocol::Sha256Digest;
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
            FILE_RENAME_INFO, FILE_RENAME_INFO_0, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FileRenameInfoEx, FindClose, FindFirstFileW,
            GetFileInformationByHandle, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            MoveFileExW, SetFileInformationByHandle, WIN32_FIND_DATAW,
        },
    };

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    const MAX_LINK_TRAVERSALS: usize = 40;
    const MAX_ROUTE_COMPONENTS: usize = 4096;

    pub(crate) fn lexical_path_spelling(path: &Path) -> PathBuf {
        if !path.is_absolute() || !path.as_os_str().as_encoded_bytes().contains(&b'~') {
            return path.to_owned();
        }
        let mut result = PathBuf::new();
        let mut components = path.components();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                result.push(component.as_os_str());
                continue;
            };
            let candidate = result.join(name);
            let found = validate_windows_component(name)
                .and_then(|()| wide_path(&candidate))
                .ok()
                .and_then(|wide| {
                    let mut data = WIN32_FIND_DATAW::default();
                    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
                    if handle == INVALID_HANDLE_VALUE {
                        return None;
                    }
                    unsafe { FindClose(handle) };
                    Some(data)
                });
            let Some(data) = found else {
                result.push(name);
                result.extend(components.map(|part| part.as_os_str()));
                break;
            };
            let length = data
                .cFileName
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(data.cFileName.len());
            result.push(OsString::from_wide(&data.cFileName[..length]));
            // FindFirstFile reports the link's own entry name. Never inspect its
            // descendants for spelling: permission labels must retain the alias.
            if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                result.extend(components.map(|part| part.as_os_str()));
                break;
            }
        }
        result
    }

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
        execution_path: PathBuf,
        sandbox_root: PathBuf,
        route: Vec<RouteNode>,
    }
    pub struct PreparedAbsent {
        pub display_path: PathBuf,
        execution_path: PathBuf,
        first_missing: PathBuf,
        sandbox_root: PathBuf,
        route: Vec<RouteNode>,
    }
    struct RouteNode {
        path: PathBuf,
        identity: ObjectIdentity,
        link_target: Option<PathBuf>,
        _object: fs::File,
    }
    struct ResolvedPath {
        execution_path: PathBuf,
        first_missing: Option<PathBuf>,
        route: Vec<RouteNode>,
    }
    enum PendingComponent {
        Normal(OsString),
        Parent,
        RequireDirectory,
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
            append_path(&mut bytes, &self.display_path);
            append_path(&mut bytes, &self.execution_path);
            append_route(&mut bytes, &self.route);
            bytes.extend_from_slice(&self.identity.canonical_bytes());
            bytes.extend_from_slice(self.content_digest.as_str().as_bytes());
            Ok(bytes)
        }
        pub fn read_bytes(&self) -> Result<Vec<u8>, ToolError> {
            if self.directory {
                return Err(ToolError::execution("prepared target is a directory"));
            }
            fs::read(&self.execution_path).map_err(super::io_error)
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
            directory_entries(&self.execution_path)
        }
        pub fn revalidate(&self) -> Result<(), ToolError> {
            revalidate_route(&self.route)?;
            let resolved = resolve_path(&self.display_path, false).map_err(|error| {
                ToolError::operation_changed(format!("prepared target route changed: {error}"))
            })?;
            if !paths_equal(&resolved.execution_path, &self.execution_path)
                || resolved.first_missing.is_some()
                || !routes_equal(&resolved.route, &self.route)
            {
                return Err(ToolError::operation_changed(
                    "prepared target resolved to another path",
                ));
            }
            validate_contained_path(&self.sandbox_root, &resolved.execution_path).map_err(
                |error| {
                    ToolError::operation_changed(format!(
                        "prepared target containment changed: {error}"
                    ))
                },
            )?;
            let file = open_for_identity(&self.execution_path).map_err(|error| {
                ToolError::operation_changed(format!("prepared target changed: {error}"))
            })?;
            let identity = identity(&file).map_err(|error| {
                ToolError::operation_changed(format!("prepared target changed: {error}"))
            })?;
            if identity.device != self.identity.device
                || identity.inode != self.identity.inode
                || identity.mode != self.identity.mode
            {
                return Err(ToolError::operation_changed(
                    "prepared target identity changed",
                ));
            }
            let digest = content_digest(&self.execution_path, self.directory).map_err(|error| {
                ToolError::operation_changed(format!("prepared target changed: {error}"))
            })?;
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
            let temporary = stage_sibling(&self.execution_path, bytes)?;
            if let Err(error) = move_file(&temporary, &self.execution_path, true) {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(AtomicWriteOutcome::default())
        }
        pub fn proc_fd_path(&self) -> PathBuf {
            self.execution_path.clone()
        }
    }
    impl PreparedAbsent {
        pub fn revalidate(&self) -> Result<(), ToolError> {
            revalidate_route(&self.route)?;
            match fs::symlink_metadata(&self.first_missing) {
                Ok(_) => {
                    return Err(ToolError::operation_changed(
                        "an originally missing path component was inserted",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ToolError::operation_changed(format!(
                        "prepared missing route changed: {error}"
                    )));
                }
            }
            let resolved = resolve_path(&self.display_path, true).map_err(|error| {
                ToolError::operation_changed(format!("prepared target route changed: {error}"))
            })?;
            if !paths_equal(&resolved.execution_path, &self.execution_path)
                || resolved.first_missing.as_ref() != Some(&self.first_missing)
                || !routes_equal(&resolved.route, &self.route)
            {
                return Err(ToolError::operation_changed(
                    "prepared absent target resolved to another path",
                ));
            }
            validate_contained_path(&self.sandbox_root, &resolved.execution_path).map_err(
                |error| {
                    ToolError::operation_changed(format!(
                        "prepared target containment changed: {error}"
                    ))
                },
            )?;
            if self.execution_path.exists() {
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
            append_path(&mut bytes, &self.execution_path);
            append_path(&mut bytes, &self.first_missing);
            append_route(&mut bytes, &self.route);
            Ok(bytes)
        }
        pub fn create_atomically(&self, bytes: &[u8]) -> Result<AtomicWriteOutcome, ToolError> {
            self.revalidate()?;
            let parent = self
                .execution_path
                .parent()
                .ok_or_else(|| ToolError::unsupported_security("target has no parent"))?;
            create_private_parents(&self.sandbox_root, parent)?;
            let temporary = stage_sibling(&self.execution_path, bytes)?;
            if let Err(error) = move_file(&temporary, &self.execution_path, false) {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(AtomicWriteOutcome::default())
        }
    }
    pub fn cwd_context_bytes(cwd: &Path) -> Result<Vec<u8>, ToolError> {
        let cwd = normalize_absolute(cwd)?;
        let resolved = resolve_path(&cwd, false)?;
        Ok(identity(&open_for_identity(&resolved.execution_path)?)?.canonical_bytes())
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
        let display_path = absolute_requested(cwd, requested)?;
        let sandbox_root = sandbox_root(cwd, &display_path, requested.is_absolute())?;
        let resolved = resolve_path(&display_path, true)?;
        validate_contained_path(&sandbox_root, &resolved.execution_path)?;
        match resolved.first_missing {
            None => {
                let file = open_for_identity(&resolved.execution_path)?;
                let identity = identity(&file)?;
                let directory = file.metadata().map_err(super::io_error)?.is_dir();
                let content_digest = content_digest(&resolved.execution_path, directory)?;
                revalidate_route(&resolved.route)?;
                Ok(PreparedTarget::Existing(PreparedExisting {
                    display_path,
                    identity,
                    content_digest,
                    directory,
                    execution_path: resolved.execution_path,
                    sandbox_root,
                    route: resolved.route,
                }))
            }
            Some(first_missing) => {
                revalidate_route(&resolved.route)?;
                match fs::symlink_metadata(&first_missing) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(ToolError::operation_changed(
                            "missing route component was inserted during preparation",
                        ));
                    }
                    Err(error) => return Err(super::io_error(error)),
                }
                Ok(PreparedTarget::Absent(PreparedAbsent {
                    display_path,
                    execution_path: resolved.execution_path,
                    first_missing,
                    sandbox_root,
                    route: resolved.route,
                }))
            }
        }
    }

    fn resolve_path(path: &Path, allow_missing: bool) -> Result<ResolvedPath, ToolError> {
        let path = normalize_absolute(path)?;
        let (mut current, names) = root_and_names(&path)?;
        let mut pending = names
            .into_iter()
            .map(PendingComponent::Normal)
            .collect::<VecDeque<_>>();
        let mut route = Vec::new();
        let mut links = 0;
        ensure_resolution_bound(1, pending.len())?;
        pin_route_node(&mut route, current.clone(), None)?;

        while let Some(component) = pending.pop_front() {
            let name = match component {
                PendingComponent::Normal(name) => name,
                PendingComponent::Parent => {
                    if current.file_name().is_none() || !current.pop() {
                        return Err(ToolError::unsupported_security(
                            "link target traverses above its native root",
                        ));
                    }
                    continue;
                }
                PendingComponent::RequireDirectory => {
                    if !fs::metadata(&current).map_err(super::io_error)?.is_dir() {
                        return Err(ToolError::execution(
                            "symlink destination requires a directory",
                        ));
                    }
                    continue;
                }
            };
            let candidate = current.join(&name);
            match fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    ensure_resolution_bound(route.len() + 1, pending.len())?;
                    let object = open_for_identity(&candidate)?;
                    let is_reparse = opened_file_is_reparse(&object)?;
                    let directory = object.metadata().map_err(super::io_error)?.is_dir();
                    let link_target = if is_reparse {
                        Some(read_supported_link(&candidate)?)
                    } else {
                        None
                    };
                    pin_open_route_node(
                        &mut route,
                        candidate.clone(),
                        link_target.clone(),
                        object,
                    )?;
                    if let Some(target) = link_target {
                        links += 1;
                        if links > MAX_LINK_TRAVERSALS {
                            return Err(ToolError::unsupported_security(
                                "filesystem path exceeds the Windows link traversal limit",
                            ));
                        }
                        let (target_root, target_components) =
                            link_target_components(&current, &target)?;
                        ensure_resolution_bound(
                            route.len() + usize::from(target_root.is_some()),
                            pending.len().saturating_add(target_components.len()),
                        )?;
                        if let Some(target_root) = target_root {
                            current = target_root;
                            pin_route_node(&mut route, current.clone(), None)?;
                        }
                        for component in target_components.into_iter().rev() {
                            pending.push_front(component);
                        }
                    } else {
                        if !pending.is_empty() && !directory {
                            return Err(ToolError::execution(format!(
                                "filesystem path component is not a directory: {}",
                                candidate.display()
                            )));
                        }
                        current = candidate;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                    ensure_resolution_bound(route.len(), pending.len().saturating_add(1))?;
                    if matches!(pending.back(), Some(PendingComponent::RequireDirectory)) {
                        return Err(ToolError::execution(
                            "symlink destination requires a directory",
                        ));
                    }
                    let canonical_parent = current.canonicalize().map_err(super::io_error)?;
                    let first_missing = canonical_parent.join(&name);
                    current = first_missing.clone();
                    for component in pending {
                        match component {
                            PendingComponent::Normal(component) => current.push(component),
                            PendingComponent::Parent => {
                                return Err(ToolError::execution(
                                    "link target traverses a missing directory",
                                ));
                            }
                            PendingComponent::RequireDirectory => {}
                        }
                    }
                    return Ok(ResolvedPath {
                        execution_path: current,
                        first_missing: Some(first_missing),
                        route,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolError::operation_changed(
                        "prepared filesystem object disappeared",
                    ));
                }
                Err(error) => {
                    return Err(super::io_error(format!(
                        "symlink_metadata failed for {}: {error}",
                        candidate.display()
                    )));
                }
            }
        }
        Ok(ResolvedPath {
            execution_path: current.canonicalize().map_err(super::io_error)?,
            first_missing: None,
            route,
        })
    }

    pub(super) fn ensure_resolution_bound(route: usize, pending: usize) -> Result<(), ToolError> {
        if route
            .checked_add(pending)
            .is_none_or(|total| total > MAX_ROUTE_COMPONENTS)
        {
            Err(ToolError::resource_limit(format!(
                "Windows filesystem route exceeds {MAX_ROUTE_COMPONENTS} components"
            )))
        } else {
            Ok(())
        }
    }

    fn link_target_components(
        current: &Path,
        target: &Path,
    ) -> Result<(Option<PathBuf>, Vec<PendingComponent>), ToolError> {
        let rooted = target.has_root();
        let root = if target.is_absolute() {
            Some(native_root(target)?)
        } else if rooted {
            Some(native_root(current)?)
        } else {
            None
        };
        let mut components = Vec::new();
        for component in target.components() {
            match component {
                Component::Normal(name) => {
                    validate_windows_component(name)?;
                    components.push(PendingComponent::Normal(name.to_owned()));
                }
                Component::ParentDir => components.push(PendingComponent::Parent),
                Component::CurDir | Component::RootDir if rooted => {}
                Component::CurDir => {}
                Component::Prefix(_) if target.is_absolute() => {}
                Component::Prefix(_) => {
                    return Err(ToolError::unsupported_security(
                        "drive-relative link targets are unsupported",
                    ));
                }
                Component::RootDir => {
                    return Err(ToolError::unsupported_security(
                        "link target has an unsupported root",
                    ));
                }
            }
        }
        if link_target_requires_directory(target) {
            components.push(PendingComponent::RequireDirectory);
        }
        Ok((root, components))
    }

    fn link_target_requires_directory(target: &Path) -> bool {
        let target = target.as_os_str().encode_wide().collect::<Vec<_>>();
        match target.as_slice() {
            [.., character] if *character == u16::from(b'/') || *character == u16::from(b'\\') => {
                true
            }
            [.., prior, character]
                if *character == u16::from(b'.')
                    && (*prior == u16::from(b'/') || *prior == u16::from(b'\\')) =>
            {
                true
            }
            _ => false,
        }
    }

    fn native_root(path: &Path) -> Result<PathBuf, ToolError> {
        let mut root = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                Component::RootDir => {
                    root.push(Path::new("\\"));
                    break;
                }
                _ => break,
            }
        }
        if !root.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem path has no absolute Windows root",
            ));
        }
        Ok(root)
    }

    fn root_and_names(path: &Path) -> Result<(PathBuf, Vec<OsString>), ToolError> {
        let mut root = PathBuf::new();
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => root.push(prefix.as_os_str()),
                Component::RootDir => root.push(Path::new("\\")),
                Component::Normal(name) => names.push(name.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(ToolError::unsupported_security(
                        "filesystem path is not lexically normalized",
                    ));
                }
            }
        }
        if !root.is_absolute() {
            return Err(ToolError::unsupported_security(
                "filesystem path has no absolute Windows root",
            ));
        }
        Ok((root, names))
    }

    fn pin_route_node(
        route: &mut Vec<RouteNode>,
        path: PathBuf,
        link_target: Option<PathBuf>,
    ) -> Result<(), ToolError> {
        ensure_resolution_bound(route.len() + 1, 0)?;
        let object = open_for_identity(&path)?;
        pin_open_route_node(route, path, link_target, object)
    }

    fn pin_open_route_node(
        route: &mut Vec<RouteNode>,
        path: PathBuf,
        link_target: Option<PathBuf>,
        object: fs::File,
    ) -> Result<(), ToolError> {
        let identity = identity(&object)?;
        route.push(RouteNode {
            path,
            identity,
            link_target,
            _object: object,
        });
        Ok(())
    }

    fn routes_equal(left: &[RouteNode], right: &[RouteNode]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                paths_equal(&left.path, &right.path)
                    && same_route_identity(&left.identity, &right.identity)
                    && left.link_target == right.link_target
            })
    }

    fn same_route_identity(left: &ObjectIdentity, right: &ObjectIdentity) -> bool {
        left.device == right.device && left.inode == right.inode && left.mode == right.mode
    }

    fn revalidate_route(route: &[RouteNode]) -> Result<(), ToolError> {
        for node in route {
            let file = open_for_identity(&node.path).map_err(|_| {
                ToolError::operation_changed("prepared route component disappeared")
            })?;
            let found = identity(&file)
                .map_err(|_| ToolError::operation_changed("prepared route component changed"))?;
            if !same_route_identity(&found, &node.identity) {
                return Err(ToolError::operation_changed(
                    "prepared route component identity changed",
                ));
            }
            let is_reparse = opened_file_is_reparse(&file).map_err(|error| {
                ToolError::operation_changed(format!("prepared route component changed: {error}"))
            })?;
            if is_reparse != node.link_target.is_some() {
                return Err(ToolError::operation_changed(
                    "prepared route component type changed",
                ));
            }
            if let Some(expected) = &node.link_target {
                let found = fs::read_link(&node.path)
                    .map_err(|_| ToolError::operation_changed("prepared link target changed"))?;
                if found != *expected {
                    return Err(ToolError::operation_changed("prepared link target changed"));
                }
            }
        }
        Ok(())
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
        if absolute_request {
            return native_root(requested);
        }
        let cwd = normalize_absolute(cwd)?;
        let resolved = resolve_path(&cwd, false)?;
        if !fs::metadata(&resolved.execution_path)
            .map_err(super::io_error)?
            .is_dir()
        {
            return Err(ToolError::unsupported_security(
                "filesystem sandbox root is not a directory",
            ));
        }
        Ok(resolved.execution_path)
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
                    if normalized.file_name().is_none() || !normalized.pop() {
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

    fn validate_contained_path(root: &Path, path: &Path) -> Result<(), ToolError> {
        if components_start_with(path, root) {
            Ok(())
        } else {
            Err(ToolError::unsupported_security(
                "filesystem target escapes the prepared sandbox",
            ))
        }
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
            let value = value.to_string_lossy();
            if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{unc}").to_ascii_lowercase()
            } else {
                value
                    .strip_prefix(r"\\?\")
                    .unwrap_or(&value)
                    .to_ascii_lowercase()
            }
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
        opened_file_is_reparse(&file)
    }

    fn opened_file_is_reparse(file: &fs::File) -> Result<bool, ToolError> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        let handle = file.as_raw_handle() as HANDLE;
        if unsafe { GetFileInformationByHandle(handle, &mut information) } == 0 {
            return Err(super::io_error(std::io::Error::last_os_error()));
        }
        Ok(information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
    }

    fn read_supported_link(path: &Path) -> Result<PathBuf, ToolError> {
        // Windows std accepts symbolic-link and mount-point (junction) tags here
        // and rejects every other reparse tag as unsupported.
        fs::read_link(path).map_err(|error| {
            ToolError::unsupported_security(format!(
                "unsupported reparse point at {}: {error}",
                path.display()
            ))
        })
    }

    fn open_for_identity(path: &Path) -> Result<fs::File, ToolError> {
        fs::OpenOptions::new()
            .access_mode(0)
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
        validate_contained_path(root, parent)?;
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
            let mut error = std::io::Error::last_os_error();
            if replace && error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                // NTFS classic rename-over rejects an open destination even when
                // it shares DELETE. Keep the pins and use a write-through POSIX
                // rename, rather than weakening identity or durability checks.
                match rename_over_open_target(source, &target_wide) {
                    Ok(()) => return Ok(()),
                    Err(rename_error) => error = rename_error,
                }
            }
            Err(ToolError::operation_changed(format!(
                "atomic rename failed from {} to {}: {}",
                source.display(),
                target.display(),
                error
            )))
        } else {
            Ok(())
        }
    }

    fn rename_over_open_target(source: &Path, target: &[u16]) -> std::io::Result<()> {
        const REPLACE_IF_EXISTS: u32 = 0x1;
        const POSIX_SEMANTICS: u32 = 0x2;

        let source = fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
            )
            .open(source)?;
        let filename_bytes = u32::try_from(std::mem::size_of_val(&target[..target.len() - 1]))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let buffer_bytes =
            std::mem::offset_of!(FILE_RENAME_INFO, FileName) + std::mem::size_of_val(target);
        let buffer_length = u32::try_from(buffer_bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let mut buffer = vec![
            FILE_RENAME_INFO::default();
            buffer_bytes.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>())
        ];
        let info = buffer.as_mut_ptr();
        // The header array provides FILE_RENAME_INFO alignment and enough storage
        // for its variable-length UTF-16 tail, including the terminator.
        unsafe {
            (*info).Anonymous = FILE_RENAME_INFO_0 {
                Flags: REPLACE_IF_EXISTS | POSIX_SEMANTICS,
            };
            (*info).RootDirectory = std::ptr::null_mut();
            (*info).FileNameLength = filename_bytes;
            std::ptr::copy_nonoverlapping(
                target.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                target.len(),
            );
            if SetFileInformationByHandle(
                source.as_raw_handle() as HANDLE,
                FileRenameInfoEx,
                info.cast(),
                buffer_length,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn append_path(bytes: &mut Vec<u8>, path: &Path) {
        let encoded = path.as_os_str().to_string_lossy();
        bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(encoded.as_bytes());
    }

    fn append_route(bytes: &mut Vec<u8>, route: &[RouteNode]) {
        bytes.extend_from_slice(&(route.len() as u64).to_be_bytes());
        for node in route {
            append_path(bytes, &node.path);
            bytes.extend_from_slice(&node.identity.device.to_be_bytes());
            bytes.extend_from_slice(&node.identity.inode.to_be_bytes());
            bytes.extend_from_slice(&node.identity.mode.to_be_bytes());
            match &node.link_target {
                Some(target) => {
                    bytes.push(1);
                    append_path(bytes, target);
                }
                None => bytes.push(0),
            }
        }
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
    fn symlink_targets_resolve_parent_components_after_following_links() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("actual/nested")).unwrap();
        fs::write(root.path().join("actual/value"), "resolved").unwrap();
        fs::write(root.path().join("value"), "lexical").unwrap();
        symlink("actual/nested", root.path().join("directory")).unwrap();
        symlink("directory/../value", root.path().join("alias")).unwrap();
        let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
        assert_eq!(target.verified_bytes().unwrap(), b"resolved");
        target.replace_atomically(b"updated").unwrap();
        assert_eq!(
            fs::read(root.path().join("actual/value")).unwrap(),
            b"updated"
        );
        assert_eq!(fs::read(root.path().join("value")).unwrap(), b"lexical");
        assert_eq!(
            fs::read_link(root.path().join("alias")).unwrap(),
            std::path::Path::new("directory/../value")
        );

        symlink(
            "directory/../missing/deep/value",
            root.path().join("dangling"),
        )
        .unwrap();
        let PreparedTarget::Absent(absent) =
            prepare_target(root.path(), std::path::Path::new("dangling")).unwrap()
        else {
            panic!("absent")
        };
        absent.create_atomically(b"created").unwrap();
        assert_eq!(
            fs::read(root.path().join("actual/missing/deep/value")).unwrap(),
            b"created"
        );
        assert!(
            fs::symlink_metadata(root.path().join("dangling"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn traversed_but_exited_ancestors_and_chained_links_remain_bound() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("actual/nested")).unwrap();
        fs::write(root.path().join("actual/value"), "value").unwrap();
        symlink("actual/nested", root.path().join("directory")).unwrap();
        symlink("directory/../value", root.path().join("alias")).unwrap();
        let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
        fs::rename(root.path().join("actual/nested"), root.path().join("saved")).unwrap();
        fs::create_dir(root.path().join("actual/nested")).unwrap();
        assert!(matches!(
            target.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));

        let target = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
        let binding = target.manifest_bytes().unwrap();
        fs::rename(
            root.path().join("directory"),
            root.path().join("saved-link"),
        )
        .unwrap();
        symlink("actual/nested", root.path().join("directory")).unwrap();
        assert!(matches!(
            target.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));
        let replacement = prepare_existing(root.path(), std::path::Path::new("alias")).unwrap();
        assert_ne!(binding, replacement.manifest_bytes().unwrap());
        assert_eq!(target.identity, replacement.identity);
    }

    #[test]
    fn alias_retargeting_at_commit_barriers_preserves_destinations() {
        for missing in [None, Some("new"), Some("subtree/deep/new")] {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("existing"), "original").unwrap();
            fs::write(root.path().join("other"), "other").unwrap();
            let destination = missing.unwrap_or("existing");
            let alias = root.path().join("alias");
            symlink(destination, &alias).unwrap();
            let target = prepare_target(root.path(), std::path::Path::new("alias")).unwrap();
            let retarget = || {
                fs::rename(&alias, root.path().join("saved-alias")).unwrap();
                symlink("other", &alias).unwrap();
            };
            let result = match target {
                PreparedTarget::Existing(target) => {
                    target.replace_atomically_inner(b"new", retarget)
                }
                PreparedTarget::Absent(target) => target.create_atomically_inner(b"new", retarget),
            };
            assert!(matches!(result, Err(ToolError::OperationChanged(_))));
            assert_eq!(fs::read(root.path().join("existing")).unwrap(), b"original");
            assert_eq!(fs::read(root.path().join("other")).unwrap(), b"other");
            if let Some(missing) = missing {
                assert!(!root.path().join(missing).exists());
            }
            assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cookie-agent-")
            }));
        }
    }

    #[test]
    fn symlink_cycles_and_missing_parent_traversal_fail_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        symlink("two", root.path().join("one")).unwrap();
        symlink("one", root.path().join("two")).unwrap();
        symlink("missing/../value", root.path().join("dangling")).unwrap();
        for path in ["one", "one/child", "dangling"] {
            assert!(prepare_target(root.path(), std::path::Path::new(path)).is_err());
        }
        assert!(!root.path().join("missing").exists());
        assert!(!root.path().join("value").exists());
    }

    #[test]
    fn link_target_directory_suffix_cannot_be_written_as_a_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), "value").unwrap();
        for (index, destination) in ["file/", "file/.", "missing/", "missing/."]
            .into_iter()
            .enumerate()
        {
            let alias = format!("alias-{index}");
            symlink(destination, root.path().join(&alias)).unwrap();
            assert!(prepare_target(root.path(), std::path::Path::new(&alias)).is_err());
        }
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"value");
        assert!(!root.path().join("missing").exists());
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
            _object: fs::File::open(root.path()).expect("ancestor"),
            link_target: None,
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
        drop(prepare_existing(root.path(), std::path::Path::new("link")).expect("symlink"));
        symlink("loop", root.path().join("loop")).expect("loop");
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
            drop(
                prepare_existing(root.path(), std::path::Path::new("link")).expect("prepare link"),
            );
            assert!(matches!(
                prepare_existing(root.path(), std::path::Path::new("loop")),
                Err(ToolError::UnsupportedSecurity(_))
            ));
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
    use std::{fs, path::Path, process::Command};

    use cookie_agent_engine::ToolError;

    use super::{
        PreparedTarget, components_start_with, ensure_resolution_bound, paths_equal,
        prepare_existing, prepare_target,
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
        assert!(components_start_with(
            std::path::Path::new(r"\\?\UNC\server\share\root\file.txt"),
            std::path::Path::new(r"\\server\share\root")
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

        let outside_path = outside.path().join("external.txt");
        fs::write(&outside_path, "external").expect("outside fixture");
        let outside_from_root =
            prepare_existing(root.path(), &outside_path).expect("prepare absolute outside target");
        let outside_direct = prepare_existing(outside.path(), std::path::Path::new("external.txt"))
            .expect("prepare direct outside target");
        assert!(paths_equal(&outside_from_root.display_path, &outside_path));
        assert_eq!(outside_from_root.identity, outside_direct.identity);
        assert!(!components_start_with(
            &outside_from_root.display_path,
            &root.path().canonicalize().expect("canonical sandbox")
        ));

        let relative_escape = Path::new("..").join(
            outside
                .path()
                .file_name()
                .expect("outside directory basename"),
        );
        assert!(matches!(
            prepare_target(root.path(), &relative_escape.join("external.txt")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }

    #[test]
    fn windows_replacement_preserves_open_preimage_and_delete_sharing() {
        use std::{io::Read as _, os::windows::fs::OpenOptionsExt as _};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let root = tempfile::tempdir().expect("sandbox");
        let path = root.path().join("target.txt");
        fs::write(&path, "original").expect("target");
        let mut held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .expect("held destination");
        let target = prepare_existing(root.path(), Path::new("target.txt")).expect("prepare");
        target
            .replace_atomically(b"replacement")
            .expect("replace open destination");
        let mut preimage = String::new();
        held.read_to_string(&mut preimage)
            .expect("read held preimage");
        assert_eq!(preimage, "original");
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        let target = prepare_existing(root.path(), Path::new("target.txt")).expect("prepare again");
        let _deny_delete = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .expect("deny-delete holder");
        assert!(matches!(
            target.replace_atomically(b"forbidden"),
            Err(ToolError::OperationChanged(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("cookie-stage")
        }));
    }

    #[test]
    fn windows_capability_resolves_links_but_keeps_a_lexical_display_path() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir(root.path().join("target")).expect("target directory");
        fs::write(root.path().join("target/file.txt"), "inside").expect("target");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir("target", &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        let target = prepare_existing(root.path(), Path::new(r".\link\file.txt"))
            .expect("prepare through link");
        assert!(paths_equal(
            &target.display_path,
            &root.path().join("link/file.txt")
        ));
        assert_eq!(
            target.verified_bytes().expect("read destination"),
            b"inside"
        );
    }

    #[test]
    fn windows_relative_link_parent_components_resolve_from_the_link_parent() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir_all(root.path().join("real/directory")).expect("directory");
        fs::create_dir(root.path().join("real/value")).expect("value directory");
        fs::write(root.path().join("real/value/file.txt"), "value").expect("target");
        let directory = root.path().join("directory");
        if let Err(error) =
            std::os::windows::fs::symlink_dir(Path::new(r"real\directory"), &directory)
        {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create directory symlink: {error}");
        }
        let link = root.path().join("link");
        if let Err(error) =
            std::os::windows::fs::symlink_dir(Path::new(r"directory\..\value"), &link)
        {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        let target = prepare_existing(root.path(), Path::new("link/file.txt"))
            .expect("prepare through relative target");
        assert_eq!(target.verified_bytes().expect("read destination"), b"value");
    }

    #[test]
    fn windows_link_parent_cannot_traverse_through_a_regular_file() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::write(root.path().join("file"), "not a directory").expect("file");
        fs::create_dir(root.path().join("value")).expect("value directory");
        fs::write(root.path().join("value/result.txt"), "wrong").expect("result");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir(Path::new(r"file\..\value"), &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_existing(root.path(), Path::new("link/result.txt")),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn windows_link_target_dot_suffix_requires_an_existing_directory() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::write(root.path().join("file"), "not a directory").expect("file");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_file(Path::new(r"file\."), &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_existing(root.path(), Path::new("link")),
            Err(ToolError::Failed(_))
        ));
    }

    #[test]
    fn windows_dangling_link_trailing_separator_cannot_create_a_regular_file() {
        let root = tempfile::tempdir().expect("sandbox");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir(Path::new(r"missing\"), &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_target(root.path(), Path::new("link")),
            Err(ToolError::Failed(_))
        ));
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn windows_resolution_component_budget_is_bounded() {
        assert!(ensure_resolution_bound(4095, 1).is_ok());
        assert!(matches!(
            ensure_resolution_bound(4096, 1),
            Err(ToolError::ResourceLimit(_))
        ));
        assert!(matches!(
            ensure_resolution_bound(usize::MAX, 1),
            Err(ToolError::ResourceLimit(_))
        ));
    }

    #[test]
    fn windows_relative_link_destination_must_remain_in_the_resolved_sandbox() {
        let root = tempfile::tempdir().expect("sandbox");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("file.txt"), "outside").expect("target");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_existing(root.path(), Path::new("link/file.txt")),
            Err(ToolError::UnsupportedSecurity(_))
        ));
    }

    #[test]
    fn windows_dangling_directory_link_supports_atomic_destination_creation() {
        let root = tempfile::tempdir().expect("sandbox");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir("destination", &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        let PreparedTarget::Absent(target) =
            prepare_target(root.path(), Path::new("link/nested/file.txt")).expect("prepare")
        else {
            panic!("absent destination")
        };
        assert!(paths_equal(
            &target.display_path,
            &root.path().join("link/nested/file.txt")
        ));
        target.create_atomically(b"created").expect("create");
        assert_eq!(
            fs::read(root.path().join("destination/nested/file.txt")).expect("destination"),
            b"created"
        );
        let slash_link = root.path().join("slash-link");
        std::os::windows::fs::symlink_dir("other\\", &slash_link).expect("directory suffix");
        let PreparedTarget::Absent(target) =
            prepare_target(root.path(), Path::new("slash-link/nested/file.txt"))
                .expect("prepare child")
        else {
            panic!("absent child destination")
        };
        target.create_atomically(b"child").expect("create child");
        assert_eq!(
            fs::read(root.path().join("other/nested/file.txt")).unwrap(),
            b"child"
        );
        assert!(fs::read_link(slash_link).is_ok());
    }

    #[test]
    fn windows_link_target_change_is_operation_changed() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir(root.path().join("first")).expect("first");
        fs::create_dir(root.path().join("second")).expect("second");
        fs::write(root.path().join("first/file.txt"), "first").expect("first file");
        fs::write(root.path().join("second/file.txt"), "second").expect("second file");
        let link = root.path().join("link");
        if let Err(error) = std::os::windows::fs::symlink_dir("first", &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        let target =
            prepare_existing(root.path(), Path::new("link/file.txt")).expect("prepare target");
        fs::remove_dir(&link).expect("remove old link");
        std::os::windows::fs::symlink_dir("second", &link).expect("replace link");
        assert!(matches!(
            target.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));
    }

    #[test]
    fn windows_route_ancestor_replacement_is_operation_changed() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir(root.path().join("ancestor")).expect("ancestor");
        fs::write(root.path().join("ancestor/file.txt"), "same").expect("file");
        let target =
            prepare_existing(root.path(), Path::new("ancestor/file.txt")).expect("prepare target");
        // NTFS forbids renaming a directory with open descendants. Move the
        // pinned leaf out first, then put that same object under the new ancestor.
        fs::rename(
            root.path().join("ancestor/file.txt"),
            root.path().join("held-file.txt"),
        )
        .expect("move pinned leaf out");
        fs::rename(root.path().join("ancestor"), root.path().join("displaced"))
            .expect("displace ancestor");
        fs::create_dir(root.path().join("ancestor")).expect("replacement ancestor");
        fs::rename(
            root.path().join("held-file.txt"),
            root.path().join("ancestor/file.txt"),
        )
        .expect("restore original leaf");
        let replacement = prepare_existing(root.path(), Path::new("ancestor/file.txt"))
            .expect("prepare replacement route");
        assert_eq!(target.identity, replacement.identity);
        assert!(matches!(
            target.revalidate(),
            Err(ToolError::OperationChanged(_))
        ));
    }

    #[test]
    fn windows_route_identity_ignores_mutable_directory_size() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir(root.path().join("ancestor")).expect("ancestor");
        fs::write(root.path().join("ancestor/file.txt"), "value").expect("file");
        let target =
            prepare_existing(root.path(), Path::new("ancestor/file.txt")).expect("prepare target");
        fs::write(root.path().join("ancestor/sibling.txt"), "sibling").expect("sibling");
        target.revalidate().expect("stable route identity");
    }

    #[test]
    fn windows_junctions_are_supported_route_components() {
        let root = tempfile::tempdir().expect("sandbox");
        fs::create_dir(root.path().join("target")).expect("target");
        fs::write(root.path().join("target/file.txt"), "junction").expect("file");
        let junction = root.path().join("junction");
        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(root.path().join("target"))
            .output()
            .expect("run mklink");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let target = prepare_existing(root.path(), Path::new("junction/file.txt"))
            .expect("prepare through junction");
        assert_eq!(
            target.verified_bytes().expect("read destination"),
            b"junction"
        );
    }

    #[test]
    fn windows_link_loops_are_bounded() {
        let root = tempfile::tempdir().expect("sandbox");
        let link = root.path().join("loop");
        if let Err(error) = std::os::windows::fs::symlink_dir("loop", &link) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create symlink: {error}");
        }
        assert!(matches!(
            prepare_target(root.path(), Path::new("loop/file.txt")),
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
