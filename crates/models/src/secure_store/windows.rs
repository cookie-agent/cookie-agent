use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, RawHandle},
    },
    path::{Component, Path, PathBuf},
    ptr::{null, null_mut},
};

use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessAllowedAceEx,
        Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW},
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, GetTokenInformation, INHERITED_ACE, InitializeAcl,
        InitializeSecurityDescriptor, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileAttributesW, GetFileInformationByHandle,
        LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        MoveFileExW, UnlockFileEx,
    },
    System::{
        IO::OVERLAPPED,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

use super::{SecureDirectory, SecureDirectoryLock, SecureStoreError, validate_name};

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn into_raw(mut self) -> HANDLE {
        let handle = self.0;
        self.0 = null_mut();
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this type exclusively owns the valid handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct LocalSecurityDescriptor(*mut core::ffi::c_void);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetNamedSecurityInfoW allocates this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct SidBuffer(Vec<usize>);

impl SidBuffer {
    fn as_sid(&self) -> PSID {
        // TOKEN_USER is the first object in this suitably aligned allocation.
        unsafe { (*(self.0.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }
}

fn with_private_security_attributes<T>(
    directory: bool,
    operation: impl FnOnce(*const SECURITY_ATTRIBUTES) -> io::Result<T>,
) -> io::Result<T> {
    let sid = current_user_sid()?;
    let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid.as_sid()) } as usize;
    if sid_length == 0 {
        return Err(io::Error::last_os_error());
    }
    let acl_bytes =
        size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_length;
    let mut acl_storage = vec![0u32; acl_bytes.div_ceil(size_of::<u32>())];
    let acl = acl_storage.as_mut_ptr().cast::<ACL>();
    let inheritance = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    // SAFETY: all buffers are aligned, initialized, and live through operation.
    if unsafe { InitializeAcl(acl, acl_bytes as u32, ACL_REVISION) } == 0
        || unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                inheritance,
                FILE_ALL_ACCESS,
                sid.as_sid(),
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor_ptr = (&raw mut descriptor).cast();
    // SAFETY: descriptor, SID, and ACL remain live through the creation call.
    if unsafe { InitializeSecurityDescriptor(descriptor_ptr, 1) } == 0
        || unsafe { SetSecurityDescriptorOwner(descriptor_ptr, sid.as_sid(), 0) } == 0
        || unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl, 0) } == 0
        || unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr,
        bInheritHandle: 0,
    };
    operation(&raw const attributes)
}

fn current_user_sid() -> io::Result<SidBuffer> {
    let mut token = null_mut();
    // SAFETY: token points to writable storage and GetCurrentProcess is always valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);
    let mut required = 0;
    // SAFETY: the first call intentionally queries the required length.
    unsafe {
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut required);
    }
    if required < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::last_os_error());
    }
    let words = (required as usize).div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    // SAFETY: storage is aligned and has at least `required` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(SidBuffer(storage))
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure storage path contains an invalid character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// Applies and verifies a protected DACL granting only the current user full control.
pub fn protect_path(path: &Path) -> io::Result<()> {
    if path_is_reparse(path)? {
        return Err(unsafe_path_error());
    }
    let sid = current_user_sid()?;
    let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid.as_sid()) } as usize;
    if sid_length == 0 {
        return Err(io::Error::last_os_error());
    }
    let acl_bytes =
        size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_length;
    let mut acl_storage = vec![0u32; acl_bytes.div_ceil(size_of::<u32>())];
    let acl = acl_storage.as_mut_ptr().cast::<ACL>();
    let inheritance = if fs::metadata(path)?.is_dir() {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    // SAFETY: acl_storage is aligned and sized for the ACL and its single SID-bearing ACE.
    if unsafe { InitializeAcl(acl, acl_bytes as u32, ACL_REVISION) } == 0
        || unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                inheritance,
                FILE_ALL_ACCESS,
                sid.as_sid(),
            )
        } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let wide = wide_path(path)?;
    // SAFETY: wide is NUL-terminated and acl remains alive for the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    validate_path_acl(path)
}

/// Atomically replaces one Windows path with another file from the same volume.
pub fn replace_path(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide_path(source)?;
    let target = wide_path(target)?;
    // SAFETY: both paths are NUL-terminated for the duration of the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Verifies that a path has exactly the current-user protected full-control DACL.
pub fn validate_path_acl(path: &Path) -> io::Result<()> {
    let sid = current_user_sid()?;
    let wide = wide_path(path)?;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: output pointers are writable and the path is NUL-terminated.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = LocalSecurityDescriptor(descriptor);
    if owner.is_null() || dacl.is_null() || unsafe { EqualSid(owner, sid.as_sid()) } == 0 {
        return Err(unsafe_path_error());
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: descriptor is valid until the guard drops.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(unsafe_path_error());
    }
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl is part of descriptor and info has the required size.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount != 1
    {
        return Err(unsafe_path_error());
    }
    let mut ace = null_mut();
    // SAFETY: the ACL reports one ACE and ace points to writable pointer storage.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(unsafe_path_error());
    }
    let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
    let expected_inheritance = if fs::metadata(path)?.is_dir() {
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
    } else {
        0
    };
    // ACCESS_ALLOWED_ACE_TYPE is zero. SidStart is the first byte of the variable SID.
    let valid = unsafe {
        (*ace).Header.AceType == 0
            && u32::from((*ace).Header.AceFlags) & INHERITED_ACE == 0
            && (*ace).Header.AceFlags == expected_inheritance
            && (*ace).Mask == FILE_ALL_ACCESS
            && EqualSid((&raw const (*ace).SidStart).cast_mut().cast(), sid.as_sid()) != 0
    };
    if valid {
        Ok(())
    } else {
        Err(unsafe_path_error())
    }
}

fn unsafe_path_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "secure storage path is unsafe",
    )
}

fn path_is_reparse(path: &Path) -> io::Result<bool> {
    let wide = wide_path(path)?;
    // SAFETY: wide is NUL-terminated.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn private_components(path: &Path) -> Result<Vec<&OsStr>, SecureStoreError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                validate_component(name)?;
                components.push(name);
            }
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SecureStoreError::UnsafePath);
            }
        }
    }
    if components.is_empty() {
        return Err(SecureStoreError::UnsafePath);
    }
    Ok(components)
}

fn validate_component(name: &OsStr) -> Result<(), SecureStoreError> {
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
        Err(SecureStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

fn open_directory_handle(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let wide = wide_path(path)?;
    with_private_security_attributes(true, |attributes| {
        // SAFETY: path and security attributes remain valid for the call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), attributes) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Creates missing directory components with final ACLs and validates existing ones.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(unsafe_path_error());
    }
    let mut existing = path.to_owned();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .ok_or_else(unsafe_path_error)?
                .to_owned(),
        );
        existing = existing.parent().ok_or_else(unsafe_path_error)?.to_owned();
    }
    if path_is_reparse(&existing)? {
        return Err(unsafe_path_error());
    }
    let mut current = existing;
    for component in missing.into_iter().rev() {
        validate_component(&component).map_err(|_| unsafe_path_error())?;
        current.push(component);
        create_private_directory(&current)?;
        validate_path_acl(&current)?;
    }
    if current == path {
        validate_path_acl(path)
    } else {
        Ok(())
    }
}

pub(super) fn open_private(
    anchor: &Path,
    relative: &Path,
) -> Result<SecureDirectory, SecureStoreError> {
    let mut current = anchor.canonicalize().map_err(SecureStoreError::Io)?;
    if path_is_reparse(&current).map_err(SecureStoreError::Io)? {
        return Err(SecureStoreError::UnsafePath);
    }
    for component in private_components(relative)? {
        current.push(component);
        let created = match create_private_directory(&current) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => false,
            Err(error) => return Err(SecureStoreError::Io(error)),
        };
        let metadata = fs::symlink_metadata(&current).map_err(SecureStoreError::Io)?;
        if !metadata.is_dir() || path_is_reparse(&current).map_err(SecureStoreError::Io)? {
            return Err(SecureStoreError::UnsafePath);
        }
        if !created {
            validate_path_acl(&current).map_err(SecureStoreError::Io)?;
        }
    }
    let directory = open_directory_handle(&current).map_err(SecureStoreError::Io)?;
    validate_directory_handle(&directory)?;
    validate_path_acl(&current).map_err(SecureStoreError::Io)?;
    Ok(SecureDirectory {
        directory,
        path: current,
    })
}

pub(super) fn open_absolute_private(path: &Path) -> Result<SecureDirectory, SecureStoreError> {
    if !path.is_absolute() {
        return Err(SecureStoreError::UnsafePath);
    }
    let mut anchor = path
        .parent()
        .ok_or(SecureStoreError::UnsafePath)?
        .to_owned();
    let mut missing = vec![
        path.file_name()
            .ok_or(SecureStoreError::UnsafePath)?
            .to_owned(),
    ];
    while !anchor.exists() {
        missing.push(
            anchor
                .file_name()
                .ok_or(SecureStoreError::UnsafePath)?
                .to_owned(),
        );
        anchor = anchor
            .parent()
            .ok_or(SecureStoreError::UnsafePath)?
            .to_owned();
    }
    let mut relative = PathBuf::new();
    for component in missing.into_iter().rev() {
        relative.push(component);
    }
    open_private(&anchor, &relative)
}

fn handle(file: &fs::File) -> HANDLE {
    file.as_raw_handle() as RawHandle as HANDLE
}

fn file_information(file: &fs::File) -> Result<BY_HANDLE_FILE_INFORMATION, SecureStoreError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: handle is valid and information is writable.
    if unsafe { GetFileInformationByHandle(handle(file), &mut information) } == 0 {
        return Err(SecureStoreError::Io(io::Error::last_os_error()));
    }
    Ok(information)
}

fn validate_directory_handle(file: &fs::File) -> Result<(), SecureStoreError> {
    let information = file_information(file)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || information.dwFileAttributes
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
            == 0
    {
        Err(SecureStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

fn validate_file_handle(file: &fs::File) -> Result<(), SecureStoreError> {
    let information = file_information(file)?;
    if information.dwFileAttributes
        & (FILE_ATTRIBUTE_REPARSE_POINT
            | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY)
        != 0
        || information.nNumberOfLinks != 1
    {
        Err(SecureStoreError::UnsafePath)
    } else {
        Ok(())
    }
}

fn file_identity(file: &fs::File) -> Result<(u32, u64), SecureStoreError> {
    let information = file_information(file)?;
    Ok((
        information.dwVolumeSerialNumber,
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

fn open_existing(path: &Path, write: bool) -> Result<Option<fs::File>, SecureStoreError> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(write)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    match options.open(path) {
        Ok(file) => {
            validate_file_handle(&file)?;
            validate_path_acl(path).map_err(SecureStoreError::Io)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SecureStoreError::Io(error)),
    }
}

fn create_file(path: &Path) -> Result<fs::File, SecureStoreError> {
    let wide = wide_path(path).map_err(SecureStoreError::Io)?;
    let handle = with_private_security_attributes(false, |attributes| {
        // SAFETY: path and security attributes remain valid for the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(OwnedHandle(handle))
        }
    })
    .map_err(SecureStoreError::Io)?;
    // SAFETY: ownership is transferred from OwnedHandle to File exactly once.
    let file = unsafe { fs::File::from_raw_handle(handle.into_raw() as RawHandle) };
    validate_file_handle(&file)?;
    validate_path_acl(path).map_err(SecureStoreError::Io)?;
    Ok(file)
}

/// Atomically creates a new private file with its final owner and protected DACL.
pub fn create_private_file(path: &Path) -> io::Result<fs::File> {
    create_file(path).map_err(|error| match error {
        SecureStoreError::Io(error) => error,
        SecureStoreError::UnsafePath => unsafe_path_error(),
        SecureStoreError::HomeUnavailable | SecureStoreError::TooLarge => {
            io::Error::other("private file creation failed")
        }
    })
}

fn open_or_create(path: &Path) -> Result<fs::File, SecureStoreError> {
    if let Some(file) = open_existing(path, true)? {
        return Ok(file);
    }
    match create_file(path) {
        Ok(file) => Ok(file),
        Err(SecureStoreError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing(path, true)?.ok_or(SecureStoreError::UnsafePath)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn read_file(
    directory: &SecureDirectory,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, SecureStoreError> {
    validate_directory_handle(&directory.directory)?;
    let path = directory.path.join(name);
    let Some(file) = open_existing(&path, false)? else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(SecureStoreError::Io)?;
    if metadata.len() > limit {
        return Err(SecureStoreError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| SecureStoreError::TooLarge)?,
    );
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(SecureStoreError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(SecureStoreError::TooLarge);
    }
    Ok(Some(bytes))
}

pub(super) fn lock<'a>(
    directory: &'a SecureDirectory,
    name: &str,
) -> Result<SecureDirectoryLock<'a>, SecureStoreError> {
    let path = directory.path.join(name);
    let file = open_or_create(&path)?;
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the synchronous file handle and OVERLAPPED are valid for the blocking call.
    if unsafe {
        LockFileEx(
            handle(&file),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } == 0
    {
        return Err(SecureStoreError::Io(io::Error::last_os_error()));
    }
    verify_current_entry(directory, name, &file)?;
    Ok(SecureDirectoryLock {
        directory,
        lock_name: name.to_owned(),
        _lock: file,
    })
}

pub(super) fn unlock(file: &fs::File) {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: this unlocks the same whole-file range locked by `lock`.
    unsafe {
        UnlockFileEx(handle(file), 0, u32::MAX, u32::MAX, &mut overlapped);
    }
}

pub(super) fn verify_current_entry(
    directory: &SecureDirectory,
    name: &str,
    held: &fs::File,
) -> Result<(), SecureStoreError> {
    let current =
        open_existing(&directory.path.join(name), false)?.ok_or(SecureStoreError::UnsafePath)?;
    if file_identity(held)? == file_identity(&current)? {
        Ok(())
    } else {
        Err(SecureStoreError::UnsafePath)
    }
}

pub(super) fn read_journal(
    lock: &SecureDirectoryLock<'_>,
    limit: u64,
) -> Result<Vec<u8>, SecureStoreError> {
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
    let metadata = lock._lock.metadata().map_err(SecureStoreError::Io)?;
    if metadata.len() > limit {
        return Err(SecureStoreError::TooLarge);
    }
    let mut file = lock._lock.try_clone().map_err(SecureStoreError::Io)?;
    file.seek(SeekFrom::Start(0))
        .map_err(SecureStoreError::Io)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(SecureStoreError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(SecureStoreError::TooLarge);
    }
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
    Ok(bytes)
}

pub(super) fn append_journal(
    lock: &SecureDirectoryLock<'_>,
    bytes: &[u8],
    limit: u64,
) -> Result<(), SecureStoreError> {
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
    let current = lock._lock.metadata().map_err(SecureStoreError::Io)?.len();
    if current.saturating_add(bytes.len() as u64) > limit {
        return Err(SecureStoreError::TooLarge);
    }
    let mut file = lock._lock.try_clone().map_err(SecureStoreError::Io)?;
    file.seek(SeekFrom::End(0)).map_err(SecureStoreError::Io)?;
    file.write_all(bytes).map_err(SecureStoreError::Io)?;
    file.sync_all().map_err(SecureStoreError::Io)?;
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)
}

pub(super) fn clear_journal(lock: &SecureDirectoryLock<'_>) -> Result<(), SecureStoreError> {
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
    lock._lock.set_len(0).map_err(SecureStoreError::Io)?;
    lock._lock.sync_all().map_err(SecureStoreError::Io)?;
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)
}

pub(super) fn atomic_replace(
    lock: &SecureDirectoryLock<'_>,
    name: &str,
    bytes: &[u8],
) -> Result<(), SecureStoreError> {
    if let Some(existing) = open_existing(&lock.directory.path.join(name), false)? {
        verify_current_entry(lock.directory, name, &existing)?;
    }
    let temporary_name = format!(".{name}.tmp-{}", Uuid::now_v7());
    let temporary = lock.directory.path.join(&temporary_name);
    let target = lock.directory.path.join(name);
    let mut file = create_file(&temporary)?;
    let result = (|| {
        file.write_all(bytes).map_err(SecureStoreError::Io)?;
        file.sync_all().map_err(SecureStoreError::Io)?;
        verify_current_entry(lock.directory, &temporary_name, &file)?;
        verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
        replace_path(&temporary, &target).map_err(SecureStoreError::Io)?;
        let installed = open_existing(&target, false)?.ok_or(SecureStoreError::UnsafePath)?;
        verify_current_entry(lock.directory, name, &installed)?;
        verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(super) fn remove(lock: &SecureDirectoryLock<'_>, name: &str) -> Result<(), SecureStoreError> {
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)?;
    let path = lock.directory.path.join(name);
    if let Some(file) = open_existing(&path, false)? {
        verify_current_entry(lock.directory, name, &file)?;
        fs::remove_file(path).map_err(SecureStoreError::Io)?;
    }
    verify_current_entry(lock.directory, &lock.lock_name, &lock._lock)
}

pub(super) fn validate_leaf_name(name: &str) -> Result<(), SecureStoreError> {
    validate_name(name)?;
    validate_component(OsStr::new(name))
}
