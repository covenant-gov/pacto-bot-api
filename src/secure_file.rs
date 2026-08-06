//! Cross-platform owner-only file creation for sensitive runtime artifacts.
//!
//! Files containing secrets, encryption keys, or diagnostics must never pass
//! through a window where they carry the process's default (umask- or
//! inherited-ACL-derived) permissions. `create_restricted_file` creates the
//! file with owner-only access already in place — `0o600` on Unix, an
//! owner-only protected DACL on Windows built from the current process
//! token — rather than creating it permissively and tightening afterward.
//! See `docs/solutions/best-practices/secure-file-creation.md`.

use std::fs;
use std::path::Path;

/// Create `path` for writing with owner-only access from the moment it is
/// created. Truncates an existing file at `path`.
#[cfg(unix)]
pub fn create_restricted_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Create `path` for writing with an owner-only access from the moment it is
/// created.
///
/// On Windows, the file is created with a protected DACL that grants full
/// control only to the current process owner, so there is no window where
/// the file inherits a permissive default ACL.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn create_restricted_file(path: &Path) -> std::io::Result<fs::File> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use winapi::um::fileapi::{CREATE_ALWAYS, CreateFileW};
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::securitybaseapi::{
        AddAccessAllowedAce, GetSidLengthRequired, GetSidSubAuthorityCount, GetTokenInformation,
        InitializeAcl, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl,
    };
    use winapi::um::winnt::{
        ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        GENERIC_WRITE, HANDLE, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR,
        SECURITY_DESCRIPTOR_REVISION, TOKEN_OWNER, TOKEN_QUERY, TokenOwner,
    };

    // Get the owner SID from the current process token.
    let mut token: HANDLE = ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut size_needed: u32 = 0;
    unsafe {
        GetTokenInformation(token, TokenOwner, ptr::null_mut(), 0, &mut size_needed);
    }
    let mut owner_buffer = vec![0u8; size_needed as usize];
    let owner_info = owner_buffer.as_mut_ptr() as *mut TOKEN_OWNER;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenOwner,
            owner_buffer.as_mut_ptr() as *mut _,
            size_needed,
            &mut size_needed,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let owner_sid: PSID = unsafe { (*owner_info).Owner };

    // Build an owner-only DACL.
    let sub_authority_count = unsafe { *GetSidSubAuthorityCount(owner_sid) };
    let sid_length = unsafe { GetSidLengthRequired(sub_authority_count) };
    let acl_size = std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        - std::mem::size_of::<u32>()
        + sid_length as usize;
    let mut acl_buffer = vec![0u8; acl_size];
    let acl = acl_buffer.as_mut_ptr() as *mut ACL;
    let ok = unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION.into()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ok = unsafe { AddAccessAllowedAce(acl, ACL_REVISION.into(), FILE_ALL_ACCESS, owner_sid) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Build a security descriptor with the owner-only DACL.
    let mut sd: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        InitializeSecurityDescriptor(
            &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR,
            SECURITY_DESCRIPTOR_REVISION,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ok = unsafe {
        SetSecurityDescriptorDacl(
            &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR,
            1,
            acl,
            0,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ok = unsafe {
        SetSecurityDescriptorControl(
            &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR,
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Create the file with the restrictive DACL already in place.
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.lpSecurityDescriptor = &mut sd as *mut SECURITY_DESCRIPTOR as *mut _;
    sa.bInheritHandle = 0;

    let path_wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &mut sa,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: handle is valid and ownership is transferred to File.
    let file = unsafe { fs::File::from_raw_handle(handle as *mut _) };
    Ok(file)
}

/// Fallback for platforms without an explicit owner-only permission model.
#[cfg(not(any(unix, windows)))]
pub fn create_restricted_file(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn creates_file_with_owner_only_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.txt");
        let mut file = create_restricted_file(&path).expect("create restricted file");
        file.write_all(b"hello").expect("write");
        drop(file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600, got {mode:03o}");
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn truncates_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.txt");
        {
            let mut file = create_restricted_file(&path).expect("create");
            file.write_all(b"first-value-that-is-longer")
                .expect("write");
        }
        {
            let mut file = create_restricted_file(&path).expect("recreate");
            file.write_all(b"short").expect("write");
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "short");
    }
}
