use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    mem::{offset_of, size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, RawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::null_mut,
    time::{Duration, Instant},
};

use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION,
        ERROR_NOT_SUPPORTED, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    },
    Security::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        AddAccessAllowedAceEx,
        Authorization::{GetNamedSecurityInfoW, GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, GetTokenInformation, INHERITED_ACE, InitializeAcl,
        InitializeSecurityDescriptor, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, FILE_RENAME_INFO, FILE_RENAME_INFO_0, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfoEx, FlushFileBuffers,
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, READ_CONTROL,
        SetFileInformationByHandle, UnlockFileEx, WRITE_DAC, WRITE_OWNER,
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
            // SAFETY: the security-info APIs allocate this descriptor with LocalAlloc.
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

fn with_private_acl<T>(
    directory: bool,
    operation: impl FnOnce(PSID, *mut ACL) -> io::Result<T>,
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
    operation(sid.as_sid(), acl)
}

fn with_private_security_attributes<T>(
    directory: bool,
    operation: impl FnOnce(*const SECURITY_ATTRIBUTES) -> io::Result<T>,
) -> io::Result<T> {
    with_private_acl(directory, |owner, acl| {
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        // SAFETY: descriptor, SID, and ACL remain live through the creation call.
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, 1) } == 0
            || unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner, 0) } == 0
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
    })
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
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secure storage path contains an invalid character",
        ));
    }
    // Native APIs need an extended path even when std::fs accepts the same
    // long path. Normalize first: verbatim paths do not resolve `/` or `..`.
    // Unlike canonicalize, absolute also works for files not yet created.
    let path = std::path::absolute(path)?;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => {
                wide.splice(..0, r"\\?\".encode_utf16());
            }
            Prefix::UNC(..) => {
                wide.splice(..2, r"\\?\UNC\".encode_utf16());
            }
            _ => {} // Already verbatim or a device path.
        },
        _ => unreachable!("absolute Windows path has a prefix"),
    }
    wide.push(0);
    Ok(wide)
}

/// `FILE_RENAME_INFO::Flags` bits, which `windows-sys` does not export.
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

/// Atomically replaces one Windows path with another file from the same volume.
///
/// Windows 10 1607+ on NTFS supersedes the target through
/// `SetFileInformationByHandle(FileRenameInfoEx)` with POSIX semantics, and that
/// keeps the target *name* resolvable for the whole operation: a concurrent
/// open-by-name observes the old file or the new one, never a gap. Classic
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` instead unlinks the target and then
/// links the source, so a by-name read racing a replacement can fail with
/// `ERROR_FILE_NOT_FOUND`. `std::fs::rename` does not help: it reaches for the
/// same POSIX rename only after `MoveFileExW` returns `ERROR_ACCESS_DENIED`.
/// The classic call therefore stays only as the fallback for kernels and
/// filesystems that reject the newer information class.
pub fn replace_path(source: &Path, target: &Path) -> io::Result<()> {
    let source_wide = wide_path(source)?;
    let target_wide = wide_path(target)?;
    match posix_replace_path(&source_wide, &target_wide) {
        // The POSIX rename carries no write-through flag, so the durability
        // MOVEFILE_WRITE_THROUGH used to provide comes from flushing the
        // target's parent directory, mirroring the Unix `fsync` after `rename`.
        Ok(()) => flush_parent_directory(target),
        Err(error) if unsupported_rename_information(&error) => {
            // SAFETY: both paths are NUL-terminated for the duration of the call.
            if unsafe {
                MoveFileExW(
                    source_wide.as_ptr(),
                    target_wide.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

/// Pre-1607 kernels and filesystems without `FileRenameInfoEx` reject the class
/// itself rather than the request, so only these two codes fall back.
fn unsupported_rename_information(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_INVALID_PARAMETER as i32 || code == ERROR_NOT_SUPPORTED as i32
    )
}

/// Supersedes `target` with `source` in one metadata transaction. Both paths are
/// NUL-terminated wide strings in the verbatim form [`wide_path`] produces.
fn posix_replace_path(source: &[u16], target: &[u16]) -> io::Result<()> {
    // DELETE is the right the rename consumes; sharing everything keeps readers
    // and the target's own openers from turning the replacement into a sharing
    // violation. Callers replace files, so no FILE_FLAG_BACKUP_SEMANTICS.
    // SAFETY: the path is NUL-terminated and the handle is closed by OwnedHandle.
    let handle = unsafe {
        CreateFileW(
            source.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);

    // FILE_RENAME_INFO ends in a variable-length UTF-16 name. Allocating whole
    // structs gives the buffer the struct's alignment and enough storage for the
    // name plus its terminator; FileNameLength excludes the terminator.
    let name_length = u32::try_from(size_of_val(&target[..target.len() - 1]))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let buffer_bytes = offset_of!(FILE_RENAME_INFO, FileName) + size_of_val(target);
    let buffer_length = u32::try_from(buffer_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut buffer =
        vec![FILE_RENAME_INFO::default(); buffer_bytes.div_ceil(size_of::<FILE_RENAME_INFO>())];
    let info = buffer.as_mut_ptr();
    // SAFETY: the vector is FILE_RENAME_INFO-aligned and holds at least
    // `buffer_bytes` writable bytes, which covers the name and its terminator.
    unsafe {
        (*info).Anonymous = FILE_RENAME_INFO_0 {
            Flags: FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
        };
        (*info).RootDirectory = null_mut();
        (*info).FileNameLength = name_length;
        std::ptr::copy_nonoverlapping(
            target.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            target.len(),
        );
        if SetFileInformationByHandle(handle.0, FileRenameInfoEx, info.cast(), buffer_length) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Commits the replaced directory entry, the Windows counterpart of the parent
/// `fsync` the Unix path performs after `rename`. Windows does not consistently
/// permit flushing a directory handle across filesystems and host policies, so
/// a refusal leaves the replacement as durable as the volume allows instead of
/// failing an operation that already committed.
fn flush_parent_directory(target: &Path) -> io::Result<()> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    let wide = wide_path(parent)?;
    // SAFETY: the path is NUL-terminated and the handle is closed by OwnedHandle.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return tolerate_refused_directory_flush(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);
    // SAFETY: the handle is a valid open directory handle.
    if unsafe { FlushFileBuffers(handle.0) } == 0 {
        return tolerate_refused_directory_flush(io::Error::last_os_error());
    }
    Ok(())
}

fn tolerate_refused_directory_flush(error: io::Error) -> io::Result<()> {
    // ERROR_INVALID_FUNCTION, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, and
    // ERROR_NOT_SUPPORTED all mean "this volume will not flush a directory".
    match error.raw_os_error() {
        Some(1 | 5 | 6 | 50) => Ok(()),
        _ => Err(error),
    }
}

/// Test/diagnostic inspection for creation-time owner-only DACLs.
pub fn verify_private_creation(path: &Path) -> io::Result<()> {
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
    verify_private_security_descriptor(
        &sid,
        owner,
        dacl,
        descriptor.0,
        fs::metadata(path)?.is_dir(),
    )
}

/// Verifies owner-only security on the exact file object referenced by a handle.
pub fn verify_private_file_handle(file: &fs::File) -> io::Result<()> {
    let sid = current_user_sid()?;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: the handle is valid and output pointers are writable.
    let status = unsafe {
        GetSecurityInfo(
            handle(file),
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
    verify_private_security_descriptor(&sid, owner, dacl, descriptor.0, file.metadata()?.is_dir())
}

fn verify_private_security_descriptor(
    sid: &SidBuffer,
    owner: PSID,
    dacl: *mut ACL,
    descriptor: *mut core::ffi::c_void,
    directory: bool,
) -> io::Result<()> {
    if owner.is_null() || dacl.is_null() || unsafe { EqualSid(owner, sid.as_sid()) } == 0 {
        return Err(unsafe_path_error());
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: descriptor is valid until the guard drops.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
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
    let expected_inheritance = if directory {
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

/// Replaces the owner and DACL on the exact file object referenced by a handle.
pub fn repair_private_file_handle_acl(file: &fs::File) -> io::Result<()> {
    with_private_acl(file.metadata()?.is_dir(), |owner, dacl| {
        // SAFETY: the handle is valid and the SID and ACL remain live for the call.
        let status = unsafe {
            SetSecurityInfo(
                handle(file),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner,
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    })
}

fn unsafe_path_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid secure storage path")
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

/// Creates missing directory components with final owner-only ACLs.
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
    let mut current = existing;
    for component in missing.into_iter().rev() {
        validate_component(&component).map_err(|_| unsafe_path_error())?;
        current.push(component);
        create_private_directory(&current)?;
    }
    Ok(())
}

pub(super) fn open_private(
    anchor: &Path,
    relative: &Path,
) -> Result<SecureDirectory, SecureStoreError> {
    let mut current = anchor.canonicalize().map_err(SecureStoreError::Io)?;
    for component in private_components(relative)? {
        current.push(component);
        match create_private_directory(&current) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) => {}
            Err(error) => return Err(SecureStoreError::Io(error)),
        }
    }
    Ok(SecureDirectory { path: current })
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

fn open_existing(path: &Path, write: bool) -> Result<Option<fs::File>, SecureStoreError> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(write)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SecureStoreError::Io(error)),
    }
}

fn create_file_with_access_and_share(
    path: &Path,
    access_mode: u32,
    share_mode: u32,
) -> Result<fs::File, SecureStoreError> {
    let wide = wide_path(path).map_err(SecureStoreError::Io)?;
    let handle = with_private_security_attributes(false, |attributes| {
        // SAFETY: path and security attributes remain valid for the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access_mode,
                share_mode,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
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
    Ok(file)
}

fn create_file(path: &Path) -> Result<fs::File, SecureStoreError> {
    create_file_with_access_and_share(
        path,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
}

/// Atomically creates a new private file with its final owner and protected DACL.
pub fn create_private_file(path: &Path) -> io::Result<fs::File> {
    create_file(path).map_err(private_file_error)
}

/// Creates a private lock file whose open handle prevents rename or replacement.
pub fn create_private_lock_file(path: &Path) -> io::Result<fs::File> {
    create_file_with_access_and_share(
        path,
        private_lock_access_mode(),
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
    .map_err(private_file_error)
}

/// Opens an existing private lock with the rights needed for handle-based ACL repair.
pub fn open_private_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(private_lock_access_mode())
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

fn private_lock_access_mode() -> u32 {
    // FILE_GENERIC_READ includes READ_CONTROL, required by GetSecurityInfo. SetSecurityInfo
    // additionally requires WRITE_DAC for the DACL and WRITE_OWNER for the owner SID.
    const _: () = assert!(FILE_GENERIC_READ & READ_CONTROL == READ_CONTROL);
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | WRITE_DAC | WRITE_OWNER
}

fn private_file_error(error: SecureStoreError) -> io::Error {
    match error {
        SecureStoreError::Io(error) => error,
        SecureStoreError::UnsafePath => {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid private file path")
        }
        SecureStoreError::HomeUnavailable | SecureStoreError::TooLarge => {
            io::Error::other("private file creation failed")
        }
        SecureStoreError::LockContention { .. } => {
            io::Error::new(io::ErrorKind::WouldBlock, "private file lock contention")
        }
    }
}

fn open_or_create(path: &Path) -> Result<fs::File, SecureStoreError> {
    if let Some(file) = open_existing(path, true)? {
        return Ok(file);
    }
    match create_file(path) {
        Ok(file) => Ok(file),
        Err(SecureStoreError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_existing(path, true)?.ok_or_else(|| {
                SecureStoreError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "storage file disappeared during creation",
                ))
            })
        }
        Err(error) => Err(error),
    }
}

pub(super) fn read_file(
    directory: &SecureDirectory,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, SecureStoreError> {
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

pub(super) fn lock_within<'a>(
    directory: &'a SecureDirectory,
    name: &str,
    budget: Duration,
) -> Result<SecureDirectoryLock<'a>, SecureStoreError> {
    let path = directory.path.join(name);
    let file = open_or_create(&path)?;
    let started = Instant::now();
    if !super::lock_within_file(&file, budget).map_err(SecureStoreError::Io)? {
        return Err(super::lock_contention(path, started.elapsed()));
    }
    Ok(SecureDirectoryLock {
        directory,
        lock_name: name.to_owned(),
        _lock: file,
    })
}

pub(super) fn try_lock<'a>(
    directory: &'a SecureDirectory,
    name: &str,
) -> Result<Option<SecureDirectoryLock<'a>>, SecureStoreError> {
    let path = directory.path.join(name);
    let file = open_or_create(&path)?;
    if super::try_lock_once(&file).map_err(SecureStoreError::Io)? {
        Ok(Some(SecureDirectoryLock {
            directory,
            lock_name: name.to_owned(),
            _lock: file,
        }))
    } else {
        Ok(None)
    }
}

pub(super) fn try_lock_once(file: &fs::File) -> io::Result<bool> {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the synchronous file handle and OVERLAPPED are valid for the call.
    if unsafe {
        LockFileEx(
            handle(file),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    } != 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(error)
    }
}

pub(super) fn unlock(file: &fs::File) -> io::Result<()> {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: this unlocks the same whole-file range locked by `lock_within`.
    if unsafe { UnlockFileEx(handle(file), 0, u32::MAX, u32::MAX, &mut overlapped) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn read_journal(
    lock: &SecureDirectoryLock<'_>,
    limit: u64,
) -> Result<Vec<u8>, SecureStoreError> {
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
    Ok(bytes)
}

pub(super) fn append_journal(
    lock: &SecureDirectoryLock<'_>,
    bytes: &[u8],
    limit: u64,
) -> Result<(), SecureStoreError> {
    let current = lock._lock.metadata().map_err(SecureStoreError::Io)?.len();
    if current.saturating_add(bytes.len() as u64) > limit {
        return Err(SecureStoreError::TooLarge);
    }
    let mut file = lock._lock.try_clone().map_err(SecureStoreError::Io)?;
    file.seek(SeekFrom::End(0)).map_err(SecureStoreError::Io)?;
    file.write_all(bytes).map_err(SecureStoreError::Io)?;
    file.sync_all().map_err(SecureStoreError::Io)?;
    Ok(())
}

pub(super) fn clear_journal(lock: &SecureDirectoryLock<'_>) -> Result<(), SecureStoreError> {
    lock._lock.set_len(0).map_err(SecureStoreError::Io)?;
    lock._lock.sync_all().map_err(SecureStoreError::Io)?;
    Ok(())
}

pub(super) fn atomic_replace(
    directory: &SecureDirectory,
    name: &str,
    bytes: &[u8],
) -> Result<(), SecureStoreError> {
    let temporary_name = format!(".{name}.tmp-{}", Uuid::now_v7());
    let temporary = directory.path.join(&temporary_name);
    let target = directory.path.join(name);
    let mut file = create_file(&temporary)?;
    let result = (|| {
        file.write_all(bytes).map_err(SecureStoreError::Io)?;
        file.sync_all().map_err(SecureStoreError::Io)?;
        replace_path(&temporary, &target).map_err(SecureStoreError::Io)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(super) fn remove(lock: &SecureDirectoryLock<'_>, name: &str) -> Result<(), SecureStoreError> {
    let path = lock.directory.path.join(name);
    if open_existing(&path, false)?.is_some() {
        fs::remove_file(path).map_err(SecureStoreError::Io)?;
    }
    Ok(())
}

pub(super) fn validate_leaf_name(name: &str) -> Result<(), SecureStoreError> {
    validate_name(name)?;
    validate_component(OsStr::new(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_paths_normalize_before_becoming_verbatim() {
        for (input, expected) in [
            (r"C:/store/unused/../state", r"\\?\C:\store\state"),
            (
                r"\\server\share\unused\..\state",
                r"\\?\UNC\server\share\state",
            ),
            (r"\\?\C:\store\state", r"\\?\C:\store\state"),
            (r"\\?\UNC\server\share\state", r"\\?\UNC\server\share\state"),
        ] {
            let wide = wide_path(Path::new(input)).expect("native path");
            assert_eq!(wide, expected.encode_utf16().chain([0]).collect::<Vec<_>>());
        }
        assert!(wide_path(Path::new("invalid\0path")).is_err());
    }
}
