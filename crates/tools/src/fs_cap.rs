//! Filesystem targets resolved once at preparation and used at execution.
//!
//! Preparation resolves the requested path and keeps the target's parent
//! directory open; execution works relative to that handle. A symlink swapped
//! into the route while an approval is pending therefore cannot redirect the
//! operation to a path other than the one that was approved. The target's
//! content is deliberately not pinned: execution sees, and replaces, whatever
//! the file holds at that moment.

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{OsStr, OsString},
        fs::File,
        io::Write,
        os::fd::AsRawFd,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        path::{Component, Path, PathBuf},
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use cookie_agent_runtime_api::ToolError;
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RawMode};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    static PREPARED_WEIGHT: AtomicUsize = AtomicUsize::new(0);
    const PREPARED_WEIGHT_LIMIT: usize = 16_384;
    /// Descriptors a prepared target holds open until it is dropped.
    const PREPARED_DESCRIPTORS: usize = 2;
    /// Permission bits for files and directories the write tool creates.
    const CREATED_FILE_MODE: u32 = 0o600;
    const CREATED_DIRECTORY_MODE: u32 = 0o700;

    struct BudgetReservation;

    impl BudgetReservation {
        fn reserve() -> Result<Self, ToolError> {
            reserve_weight(
                &PREPARED_WEIGHT,
                PREPARED_WEIGHT_LIMIT,
                PREPARED_DESCRIPTORS,
            )?;
            Ok(Self)
        }
    }

    impl Drop for BudgetReservation {
        fn drop(&mut self) {
            PREPARED_WEIGHT.fetch_sub(PREPARED_DESCRIPTORS, Ordering::AcqRel);
        }
    }

    pub(super) fn reserve_weight(
        counter: &AtomicUsize,
        limit: usize,
        weight: usize,
    ) -> Result<(), ToolError> {
        // A compare-exchange loop rather than `fetch_update`, which Rust 1.99
        // deprecates in favour of a `try_update` the 1.88 MSRV lacks.
        let mut used = counter.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(weight).filter(|next| *next <= limit) else {
                return Err(ToolError::resource_limit(format!(
                    "aggregate prepared-object weight would exceed {limit}"
                )));
            };
            match counter.compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(()),
                Err(actual) => used = actual,
            }
        }
    }

    #[allow(clippy::unnecessary_cast)]
    fn permission_mode(mode: u32) -> Mode {
        // RawMode is u32 on Linux but Darwin's libc mode_t is u16.
        Mode::from_raw_mode((mode & 0o7777) as RawMode)
    }

    /// An existing file or directory.
    pub struct PreparedExisting {
        parent: File,
        basename: OsString,
        /// The object resolved at preparation. Only bash uses it, to exec the
        /// prepared shell and enter the prepared working directory.
        object: File,
        /// The resolved location, with every symlink expanded.
        resolved: PathBuf,
        pub display_path: PathBuf,
        pub directory: bool,
        pub mode: u32,
        _budget: BudgetReservation,
    }

    /// A path whose leaf, and possibly some ancestors, did not exist.
    pub struct PreparedAbsent {
        /// The deepest existing directory on the route.
        parent: File,
        /// The components below `parent`, the last being the file itself.
        missing: Vec<OsString>,
        resolved: PathBuf,
        pub display_path: PathBuf,
        _budget: BudgetReservation,
    }

    pub enum PreparedTarget {
        Existing(PreparedExisting),
        Absent(PreparedAbsent),
    }

    impl PreparedTarget {
        pub fn display_path(&self) -> &Path {
            match self {
                Self::Existing(target) => &target.display_path,
                Self::Absent(target) => &target.display_path,
            }
        }

        /// Identifies the resolved location for approval bindings.
        pub fn binding_bytes(&self) -> Vec<u8> {
            match self {
                Self::Existing(target) => target.binding_bytes(),
                Self::Absent(target) => location_bytes(b"absent\0", &target.resolved),
            }
        }

        /// Serializes mutations of one resolved location.
        pub fn serialization_key(&self) -> Vec<u8> {
            match self {
                Self::Existing(target) => target.serialization_key(),
                Self::Absent(target) => target.resolved.as_os_str().as_bytes().to_vec(),
            }
        }

        /// Writes `bytes` to the target: replaces the file if it exists by
        /// now, otherwise creates it and any missing parent directories.
        pub fn write(&self, bytes: &[u8]) -> Result<(), ToolError> {
            match self {
                Self::Existing(target) => target.replace(bytes),
                Self::Absent(target) => target.create(bytes),
            }
        }
    }

    impl PreparedExisting {
        pub fn binding_bytes(&self) -> Vec<u8> {
            location_bytes(
                if self.directory {
                    b"directory\0"
                } else {
                    b"file\0"
                },
                &self.resolved,
            )
        }

        pub fn serialization_key(&self) -> Vec<u8> {
            self.resolved.as_os_str().as_bytes().to_vec()
        }

        pub fn proc_fd_path(&self) -> PathBuf {
            PathBuf::from(format!("/proc/self/fd/{}", self.object.as_raw_fd()))
        }

        /// Opens the regular file now at the prepared location for reading.
        pub fn open_file(&self) -> Result<File, ToolError> {
            let file = self.open(OFlags::empty())?;
            if !file.metadata().map_err(super::io_error)?.is_file() {
                return Err(ToolError::operation_changed(
                    "prepared target is no longer a regular file",
                ));
            }
            Ok(file)
        }

        /// Lists the directory now at the prepared location, sorted by name.
        pub fn directory_entries(&self) -> Result<Vec<(String, bool)>, ToolError> {
            let directory = self.open(OFlags::DIRECTORY)?;
            let mut entries = Vec::new();
            for entry in Dir::read_from(&directory).map_err(super::io_error)? {
                let entry = entry.map_err(super::io_error)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if matches!(name.as_str(), "." | "..") {
                    continue;
                }
                entries.push((name, entry.file_type() == FileType::Directory));
            }
            entries.sort_unstable();
            Ok(entries)
        }

        /// Atomically replaces the file at the prepared location.
        pub fn replace(&self, bytes: &[u8]) -> Result<(), ToolError> {
            publish(&self.parent, &self.basename, bytes, self.mode)
        }

        fn open(&self, flags: OFlags) -> Result<File, ToolError> {
            rustix::fs::openat(
                &self.parent,
                &self.basename,
                flags | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|error| match error {
                rustix::io::Errno::NOENT => {
                    ToolError::execution("prepared target no longer exists")
                }
                rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
                    ToolError::operation_changed("prepared target was replaced by another type")
                }
                error => super::io_error(error),
            })
        }
    }

    impl PreparedAbsent {
        fn create(&self, bytes: &[u8]) -> Result<(), ToolError> {
            let (leaf, directories) = self
                .missing
                .split_last()
                .ok_or_else(|| ToolError::execution("missing path is empty"))?;
            let mut created = None;
            for name in directories {
                let parent = created.as_ref().unwrap_or(&self.parent);
                match rustix::fs::mkdirat(parent, name, permission_mode(CREATED_DIRECTORY_MODE)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(super::io_error(error)),
                }
                // NOFOLLOW: a symlink planted here since preparation must not
                // redirect the write.
                let next = rustix::fs::openat(
                    parent,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| match error {
                    rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
                        ToolError::operation_changed(
                            "a prepared missing directory was replaced by another type",
                        )
                    }
                    error => super::io_error(error),
                })?;
                created = Some(File::from(next));
            }
            publish(
                created.as_ref().unwrap_or(&self.parent),
                leaf,
                bytes,
                CREATED_FILE_MODE,
            )
        }
    }

    /// Writes `bytes` to a sibling temporary and renames it over `name`: one
    /// file sync before the rename and one directory sync after it.
    fn publish(parent: &File, name: &OsStr, bytes: &[u8], mode: u32) -> Result<(), ToolError> {
        let mode = match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    return Err(ToolError::execution("write target is a directory"));
                }
                #[allow(clippy::unnecessary_cast)]
                FileType::RegularFile => stat.st_mode as u32,
                _ => mode,
            },
            Err(_) => mode,
        };
        let temporary = OsString::from(format!(
            ".cookie-agent-stage-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = File::from(
            rustix::fs::openat(
                parent,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                permission_mode(mode),
            )
            .map_err(super::io_error)?,
        );
        let staged = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(super::io_error)
            .and_then(|()| {
                rustix::fs::renameat(parent, &temporary, parent, name).map_err(super::io_error)
            });
        if let Err(error) = staged {
            let _ = rustix::fs::unlinkat(parent, &temporary, AtFlags::empty());
            return Err(error);
        }
        rustix::fs::fsync(parent).map_err(super::io_error)
    }

    fn location_bytes(kind: &[u8], resolved: &Path) -> Vec<u8> {
        let mut bytes = kind.to_vec();
        bytes.extend_from_slice(resolved.as_os_str().as_bytes());
        bytes
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
        let budget = BudgetReservation::reserve()?;
        let ResolvedPath {
            parent,
            parent_path,
            basename,
            missing,
        } = resolve_path(&absolute)?;
        if !missing.is_empty() {
            let mut resolved = parent_path;
            resolved.extend(&missing);
            return Ok(PreparedTarget::Absent(PreparedAbsent {
                parent,
                missing,
                resolved,
                display_path: absolute,
                _budget: budget,
            }));
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        match rustix::fs::openat(&parent, &basename, flags, Mode::empty()) {
            Ok(fd) => {
                let object = File::from(fd);
                let metadata = object.metadata().map_err(super::io_error)?;
                if !metadata.is_file() && !metadata.is_dir() {
                    return Err(ToolError::unsupported_security(
                        "prepared filesystem object is not a regular file or directory",
                    ));
                }
                let mut resolved = parent_path;
                if basename != "." {
                    resolved.push(&basename);
                }
                Ok(PreparedTarget::Existing(PreparedExisting {
                    parent,
                    basename,
                    object,
                    resolved,
                    display_path: absolute,
                    directory: metadata.is_dir(),
                    mode: metadata.mode(),
                    _budget: budget,
                }))
            }
            Err(rustix::io::Errno::NOENT) => {
                let mut resolved = parent_path;
                resolved.push(&basename);
                Ok(PreparedTarget::Absent(PreparedAbsent {
                    parent,
                    missing: vec![basename],
                    resolved,
                    display_path: absolute,
                    _budget: budget,
                }))
            }
            Err(rustix::io::Errno::LOOP) => Err(ToolError::operation_changed(
                "prepared target became a symlink during resolution",
            )),
            Err(error) => Err(super::io_error(error)),
        }
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
        /// `parent`'s location with every symlink expanded.
        parent_path: PathBuf,
        basename: OsString,
        missing: Vec<OsString>,
    }

    /// Walks `path` one component at a time from `/`, expanding symlinks in
    /// place, so `..` is always evaluated from the directory actually reached.
    fn resolve_path(path: &Path) -> Result<ResolvedPath, ToolError> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let open_root = || {
            rustix::fs::open("/", flags, Mode::empty())
                .map(File::from)
                .map_err(super::io_error)
        };
        let mut current = open_root()?;
        let mut current_path = PathBuf::from("/");
        let mut pending = path
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect::<std::collections::VecDeque<_>>();
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
                current = open_root()?;
                current_path = PathBuf::from("/");
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
                    let basename = missing.last().expect("missing leaf").clone();
                    return Ok(ResolvedPath {
                        parent: current,
                        parent_path: current_path,
                        basename,
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
                let target =
                    rustix::fs::readlinkat(&current, &name, Vec::new()).map_err(super::io_error)?;
                let target = OsStr::from_bytes(target.as_bytes()).to_owned();
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
                    parent_path: current_path,
                    basename: name,
                    missing: Vec::new(),
                });
            }
            current = File::from(
                rustix::fs::openat(&current, &name, flags, Mode::empty())
                    .map_err(super::io_error)?,
            );
            if name == ".." {
                current_path.pop();
            } else {
                current_path.push(&name);
            }
        }
        Ok(ResolvedPath {
            parent: current,
            parent_path: current_path,
            basename: OsString::from("."),
            missing: Vec::new(),
        })
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows {
    //! Windows path-based filesystem targets.
    //!
    //! Unlike the Unix backend, Windows cannot hold the route open: execution
    //! resolves the path again and refuses to proceed if it now leads
    //! elsewhere, which narrows but cannot close the window for a racing link.

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

    use cookie_agent_runtime_api::ToolError;
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
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

    /// An existing file or directory.
    pub struct PreparedExisting {
        pub display_path: PathBuf,
        pub directory: bool,
        pub mode: u32,
        execution_path: PathBuf,
        sandbox_root: PathBuf,
    }

    /// A path whose leaf, and possibly some ancestors, did not exist.
    pub struct PreparedAbsent {
        pub display_path: PathBuf,
        execution_path: PathBuf,
        sandbox_root: PathBuf,
    }

    struct ResolvedPath {
        execution_path: PathBuf,
        first_missing: Option<PathBuf>,
    }

    enum PendingComponent {
        Normal(OsString),
        Parent,
        RequireDirectory,
    }

    pub enum PreparedTarget {
        Existing(PreparedExisting),
        Absent(PreparedAbsent),
    }

    impl PreparedTarget {
        pub fn display_path(&self) -> &Path {
            match self {
                Self::Existing(target) => &target.display_path,
                Self::Absent(target) => &target.display_path,
            }
        }

        /// Identifies the resolved location for approval bindings.
        pub fn binding_bytes(&self) -> Vec<u8> {
            match self {
                Self::Existing(target) => target.binding_bytes(),
                Self::Absent(target) => location_bytes(b"absent\0", &target.execution_path),
            }
        }

        /// Serializes mutations of one resolved location.
        pub fn serialization_key(&self) -> Vec<u8> {
            match self {
                Self::Existing(target) => target.serialization_key(),
                Self::Absent(target) => serialization_key(&target.execution_path),
            }
        }

        /// Writes `bytes` to the target: replaces the file if it exists by
        /// now, otherwise creates it and any missing parent directories.
        pub fn write(&self, bytes: &[u8]) -> Result<(), ToolError> {
            match self {
                Self::Existing(target) => target.replace(bytes),
                Self::Absent(target) => target.create(bytes),
            }
        }
    }

    impl PreparedExisting {
        pub fn binding_bytes(&self) -> Vec<u8> {
            location_bytes(
                if self.directory {
                    b"directory\0"
                } else {
                    b"file\0"
                },
                &self.execution_path,
            )
        }

        pub fn serialization_key(&self) -> Vec<u8> {
            serialization_key(&self.execution_path)
        }

        pub fn proc_fd_path(&self) -> PathBuf {
            self.execution_path.clone()
        }

        /// Opens the regular file now at the prepared location for reading.
        pub fn open_file(&self) -> Result<fs::File, ToolError> {
            self.verify_route()?;
            let file = fs::File::open(&self.execution_path).map_err(super::io_error)?;
            if !file.metadata().map_err(super::io_error)?.is_file() {
                return Err(ToolError::operation_changed(
                    "prepared target is no longer a regular file",
                ));
            }
            Ok(file)
        }

        /// Lists the directory now at the prepared location, sorted by name.
        pub fn directory_entries(&self) -> Result<Vec<(String, bool)>, ToolError> {
            self.verify_route()?;
            let mut entries = fs::read_dir(&self.execution_path)
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
            entries.sort_unstable();
            Ok(entries)
        }

        /// Atomically replaces the file at the prepared location.
        pub fn replace(&self, bytes: &[u8]) -> Result<(), ToolError> {
            if self.directory {
                return Err(ToolError::unsupported_security(
                    "cannot replace a directory with file content",
                ));
            }
            self.verify_route()?;
            publish(&self.execution_path, bytes)
        }

        fn verify_route(&self) -> Result<(), ToolError> {
            if verify_route(&self.display_path, &self.execution_path, &self.sandbox_root)? {
                return Err(ToolError::execution("prepared target no longer exists"));
            }
            Ok(())
        }
    }

    impl PreparedAbsent {
        fn create(&self, bytes: &[u8]) -> Result<(), ToolError> {
            verify_route(&self.display_path, &self.execution_path, &self.sandbox_root)?;
            let parent = self
                .execution_path
                .parent()
                .ok_or_else(|| ToolError::unsupported_security("target has no parent"))?;
            create_private_parents(&self.sandbox_root, parent)?;
            publish(&self.execution_path, bytes)
        }
    }

    /// Windows cannot operate relative to an open directory handle, so the
    /// path is resolved again right before use: a link planted on the route
    /// since preparation must not redirect the operation. Returns whether the
    /// target is now missing.
    fn verify_route(
        display_path: &Path,
        execution_path: &Path,
        sandbox_root: &Path,
    ) -> Result<bool, ToolError> {
        let resolved = resolve_path(display_path, true)?;
        if !paths_equal(&resolved.execution_path, execution_path) {
            return Err(ToolError::operation_changed(
                "prepared target now resolves to another path",
            ));
        }
        validate_contained_path(sandbox_root, &resolved.execution_path)?;
        Ok(resolved.first_missing.is_some())
    }

    fn publish(target: &Path, bytes: &[u8]) -> Result<(), ToolError> {
        let temporary = stage_sibling(target, bytes)?;
        if let Err(error) = move_file(&temporary, target) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    fn location_bytes(kind: &[u8], path: &Path) -> Vec<u8> {
        let mut bytes = kind.to_vec();
        bytes.extend_from_slice(path.to_string_lossy().as_bytes());
        bytes
    }

    fn serialization_key(path: &Path) -> Vec<u8> {
        path.to_string_lossy().to_lowercase().into_bytes()
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
        if resolved.first_missing.is_some() {
            return Ok(PreparedTarget::Absent(PreparedAbsent {
                display_path,
                execution_path: resolved.execution_path,
                sandbox_root,
            }));
        }
        let directory = open_for_identity(&resolved.execution_path)?
            .metadata()
            .map_err(super::io_error)?
            .is_dir();
        Ok(PreparedTarget::Existing(PreparedExisting {
            display_path,
            directory,
            mode: if directory { 0o040755 } else { 0o100755 },
            execution_path: resolved.execution_path,
            sandbox_root,
        }))
    }

    fn resolve_path(path: &Path, allow_missing: bool) -> Result<ResolvedPath, ToolError> {
        let path = normalize_absolute(path)?;
        let (mut current, names) = root_and_names(&path)?;
        let mut pending = names
            .into_iter()
            .map(PendingComponent::Normal)
            .collect::<VecDeque<_>>();
        let mut visited = 1;
        let mut links = 0;
        ensure_resolution_bound(visited, pending.len())?;
        open_for_identity(&current)?;

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
                    visited += 1;
                    ensure_resolution_bound(visited, pending.len())?;
                    let object = open_for_identity(&candidate)?;
                    if opened_file_is_reparse(&object)? {
                        let target = read_supported_link(&candidate)?;
                        links += 1;
                        if links > MAX_LINK_TRAVERSALS {
                            return Err(ToolError::unsupported_security(
                                "filesystem path exceeds the Windows link traversal limit",
                            ));
                        }
                        let (target_root, target_components) =
                            link_target_components(&current, &target)?;
                        ensure_resolution_bound(
                            visited + usize::from(target_root.is_some()),
                            pending.len().saturating_add(target_components.len()),
                        )?;
                        if let Some(target_root) = target_root {
                            visited += 1;
                            open_for_identity(&target_root)?;
                            current = target_root;
                        }
                        for component in target_components.into_iter().rev() {
                            pending.push_front(component);
                        }
                    } else {
                        if !pending.is_empty()
                            && !object.metadata().map_err(super::io_error)?.is_dir()
                        {
                            return Err(ToolError::execution(format!(
                                "filesystem path component is not a directory: {}",
                                candidate.display()
                            )));
                        }
                        current = candidate;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
                    ensure_resolution_bound(visited, pending.len().saturating_add(1))?;
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
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolError::execution(format!(
                        "filesystem path does not exist: {}",
                        candidate.display()
                    )));
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

    fn move_file(source: &Path, target: &Path) -> Result<(), ToolError> {
        let target_wide = wide_path(target)?;
        // Prefer the superseding POSIX rename: classic MoveFileExW unlinks the
        // target before linking the source, so a reader that opens the path by
        // name can find it absent mid-write, and it rejects an open destination
        // even when that shares DELETE. The classic call remains the fallback
        // for filesystems without POSIX rename semantics.
        if rename_over_open_target(source, &target_wide).is_ok() {
            return Ok(());
        }
        let source_wide = wide_path(source)?;
        let flags = MOVEFILE_WRITE_THROUGH | MOVEFILE_REPLACE_EXISTING;
        if unsafe { MoveFileExW(source_wide.as_ptr(), target_wide.as_ptr(), flags) } == 0 {
            Err(super::io_error(format!(
                "atomic rename failed from {} to {}: {}",
                source.display(),
                target.display(),
                std::io::Error::last_os_error()
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
}

#[cfg(windows)]
pub use windows::*;

/// Identifies the session working directory for operation fingerprints.
fn io_error(error: impl std::fmt::Display) -> cookie_agent_runtime_api::ToolError {
    cookie_agent_runtime_api::ToolError::execution(error.to_string())
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{fs, path::Path, process::Command};

    use cookie_agent_runtime_api::ToolError;

    use super::{
        PreparedExisting, PreparedTarget, components_start_with, ensure_resolution_bound,
        paths_equal, prepare_existing, prepare_target,
    };

    fn read(target: &PreparedExisting) -> Vec<u8> {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut target.open_file().expect("open"), &mut bytes)
            .expect("read destination");
        bytes
    }

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
        existing.replace(b"new").expect("replace");
        assert_eq!(fs::read(root.path().join("existing.txt")).unwrap(), b"new");

        let absent = prepare_target(root.path(), std::path::Path::new("nested/new.txt"))
            .expect("prepare absent");
        assert!(matches!(absent, PreparedTarget::Absent(_)));
        absent.write(b"created").expect("create");
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
        assert_eq!(
            outside_from_root.binding_bytes(),
            outside_direct.binding_bytes()
        );
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
            .replace(b"replacement")
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
            target.replace(b"forbidden"),
            Err(ToolError::Failed(_))
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
        assert_eq!(read(&target), b"inside");
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
        assert_eq!(read(&target), b"value");
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
        let target =
            prepare_target(root.path(), Path::new("link/nested/file.txt")).expect("prepare");
        assert!(matches!(target, PreparedTarget::Absent(_)));
        assert!(paths_equal(
            target.display_path(),
            &root.path().join("link/nested/file.txt")
        ));
        target.write(b"created").expect("create");
        assert_eq!(
            fs::read(root.path().join("destination/nested/file.txt")).expect("destination"),
            b"created"
        );
        let slash_link = root.path().join("slash-link");
        std::os::windows::fs::symlink_dir("other\\", &slash_link).expect("directory suffix");
        let target = prepare_target(root.path(), Path::new("slash-link/nested/file.txt"))
            .expect("prepare child");
        assert!(matches!(target, PreparedTarget::Absent(_)));
        target.write(b"child").expect("create child");
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
            target.replace(b"redirected"),
            Err(ToolError::OperationChanged(_))
        ));
        assert_eq!(
            fs::read(root.path().join("first/file.txt")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(root.path().join("second/file.txt")).unwrap(),
            b"second"
        );
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
        assert_eq!(read(&target), b"junction");
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
    fn windows_write_replaces_a_file_created_after_preparation() {
        let root = tempfile::tempdir().expect("sandbox");
        let absent =
            prepare_target(root.path(), std::path::Path::new("target.txt")).expect("prepare");
        assert!(matches!(absent, PreparedTarget::Absent(_)));
        fs::write(root.path().join("target.txt"), "concurrent").expect("insert target");
        absent.write(b"new").expect("write");
        assert_eq!(
            fs::read_to_string(root.path().join("target.txt")).unwrap(),
            "new"
        );
    }
}
