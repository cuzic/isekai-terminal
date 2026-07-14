//! Windows ACL enforcement backing `check_not_world_writable` /
//! `set_private_file_permissions` / `ensure_private_dir`'s directory-creation
//! branch (`lib.rs`). Unix uses the classic owner/group/other mode bits;
//! Windows has no equivalent bit vector, so this module works directly with
//! the file/directory's DACL (Discretionary Access Control List) via the
//! `windows` crate's raw Win32 bindings.
//!
//! Policy (deliberately *stricter* than the Unix side, see `lib.rs` module
//! docs): any `ACCESS_ALLOWED` grant of write-ish rights to a principal
//! other than the current process's user (or one of a small set of
//! well-known trusted principals, `TRUSTED_WELL_KNOWN_SIDS` — see its docs
//! for why) is treated as insecure — this is a new design for Windows
//! support, not a port of the Unix `0o002` (others-writable-only) check,
//! which is more permissive (it lets a shared group write). The path's
//! *owner* is checked the same way: an object's owner can always rewrite
//! its own DACL (no `WRITE_DAC` grant needed), so an untrusted owner makes
//! the DACL check alone meaningless regardless of what it currently says.
//!
//! Verified against real Windows CI (`test-windows`, `windows-latest`) as
//! of the Windows official-support work landing — see this crate's
//! `#[cfg(windows)]` unit tests below and `git log` for confirmation this
//! actually runs (this development *environment* itself is still
//! Linux-only; `cargo check --target x86_64-pc-windows-gnu` is what's run
//! locally here).

use std::ffi::c_void;
use std::path::Path;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    GetExplicitEntriesFromAclW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetTokenInformation, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY,
    TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_GENERIC_WRITE, DELETE, WRITE_DAC, WRITE_OWNER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::FsGuardError;

/// Any of these bits present on a non-trusted `ACCESS_ALLOWED` grant is
/// treated as "world-writable" (mirrors the intent of Unix's `0o002` check,
/// not its exact bit layout — DACL entries always carry object-specific
/// rights, never raw `GENERIC_*` bits, so `FILE_GENERIC_WRITE` alone already
/// overlaps with a `FILE_ALL_ACCESS`/"Full control" grant). `WRITE_DAC`/
/// `WRITE_OWNER`/`DELETE` are included even though they aren't *data*-write
/// rights: any of them lets the grantee rewrite the ACL (or delete and
/// recreate the object) to grant itself real write access later, so a DACL
/// that omits `FILE_GENERIC_WRITE` but grants one of these to an untrusted
/// principal would otherwise be misjudged as safe.
const WRITE_LIKE_RIGHTS: u32 = FILE_GENERIC_WRITE.0 | WRITE_DAC.0 | WRITE_OWNER.0 | DELETE.0;

/// Principals besides the current user that are trusted with write-ish
/// rights without tripping `InsecureAcl`: `NT AUTHORITY\SYSTEM` and
/// `BUILTIN\Administrators`. Both are standard, expected owners/grantees on
/// a stock Windows install (elevated processes, services, and Windows
/// itself routinely create files owned by one of these) — rejecting them
/// outright would make this check fail closed on ordinary, unmodified
/// Windows ACLs far more often than the Unix `0o002` check ever does on
/// ordinary Unix permissions. Resolved once per call via `CreateWellKnownSid`
/// rather than hardcoded, since well-known SIDs still vary by machine
/// (the exact bytes depend on the local machine/domain for some well-known
/// types, though not these particular two).
fn trusted_well_known_sids() -> Vec<Vec<u8>> {
    [WinLocalSystemSid, WinBuiltinAdministratorsSid].into_iter().filter_map(well_known_sid).collect()
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Option<Vec<u8>> {
    // SECURITY_MAX_SID_SIZE (68 bytes) comfortably fits any well-known SID.
    let mut buf = vec![0u8; 68];
    let mut len = buf.len() as u32;
    unsafe {
        CreateWellKnownSid(kind, PSID::default(), PSID(buf.as_mut_ptr() as *mut c_void), &mut len).ok()?;
    }
    buf.truncate(len as usize);
    Some(buf)
}

fn is_trusted_principal(sid: PSID, current_user_sid: PSID, trusted_well_known: &[Vec<u8>]) -> bool {
    unsafe {
        if EqualSid(sid, current_user_sid).is_ok() {
            return true;
        }
        trusted_well_known.iter().any(|buf| EqualSid(sid, PSID(buf.as_ptr() as *mut c_void)).is_ok())
    }
}

/// Frees a `LocalAlloc`-backed pointer (the allocation convention every
/// Win32 ACL API used here follows) when dropped, so early-return paths
/// can't leak it.
struct LocalAllocGuard(*mut c_void);

impl Drop for LocalAllocGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }
}

struct TokenGuard(HANDLE);

impl Drop for TokenGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn win32_io_error(code: u32) -> std::io::Error {
    std::io::Error::from_raw_os_error(code as i32)
}

/// Returns the raw `TOKEN_USER` buffer for the current process's token — the
/// `PSID` inside it (`sid_in_token_buf`) is only valid for as long as this
/// buffer lives.
fn current_user_token_buf() -> Result<Vec<u8>, FsGuardError> {
    unsafe {
        let mut token = HANDLE(std::ptr::null_mut());
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| FsGuardError::Stat(win32_io_error(e.code().0 as u32)))?;
        let _guard = TokenGuard(token);

        let mut needed: u32 = 0;
        // Expected to fail with ERROR_INSUFFICIENT_BUFFER; `needed` is set
        // regardless (standard Win32 "query the size first" two-call
        // pattern).
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(FsGuardError::Stat(std::io::Error::other("GetTokenInformation returned no size")));
        }
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), needed, &mut needed)
            .map_err(|e| FsGuardError::Stat(win32_io_error(e.code().0 as u32)))?;
        Ok(buf)
    }
}

fn sid_in_token_buf(buf: &[u8]) -> PSID {
    // SAFETY: `buf` was sized and filled by `GetTokenInformation(TokenUser,
    // ...)`, which guarantees a valid `TOKEN_USER` at its start.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}

/// Best-effort human-readable form of a `PSID`, for error messages only —
/// never fails the caller if the conversion itself fails.
fn sid_to_string(sid: PSID) -> String {
    unsafe {
        let mut raw = PWSTR::null();
        if ConvertSidToStringSidW(sid, &mut raw).is_err() || raw.is_null() {
            return "<unknown principal>".to_string();
        }
        let _guard = LocalAllocGuard(raw.0 as *mut c_void);
        raw.to_string().unwrap_or_else(|_| "<unknown principal>".to_string())
    }
}

pub(crate) fn check_not_world_writable(path: &Path) -> Result<(), FsGuardError> {
    let wide = path_to_wide(path);
    let current_user_buf = current_user_token_buf()?;
    let current_user_sid = sid_in_token_buf(&current_user_buf);
    let trusted_well_known = trusted_well_known_sids();

    unsafe {
        let mut owner = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let result = GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if result.0 != 0 {
            return Err(FsGuardError::Stat(win32_io_error(result.0)));
        }
        let _sd_guard = LocalAllocGuard(sd.0);

        // The object's *owner* can always rewrite its own DACL (implicitly
        // has `WRITE_DAC`/`WRITE_OWNER`-equivalent power over it, with no
        // ACE required) — so an untrusted owner makes every check below
        // meaningless regardless of what the current DACL says.
        if !owner.is_invalid() && !is_trusted_principal(owner, current_user_sid, &trusted_well_known) {
            return Err(FsGuardError::InsecureAcl {
                principal: sid_to_string(owner),
                rights: "owns this path (can rewrite its ACL at will, regardless of current grants)".to_string(),
            });
        }

        if dacl.is_null() {
            // A NULL DACL grants everyone full access — the Windows
            // equivalent of a Unix `0o777` file, and the most permissive
            // state possible.
            return Err(FsGuardError::InsecureAcl {
                principal: "Everyone".to_string(),
                rights: "no DACL present (unrestricted access)".to_string(),
            });
        }

        let mut count: u32 = 0;
        let mut entries: *mut EXPLICIT_ACCESS_W = std::ptr::null_mut();
        let result = GetExplicitEntriesFromAclW(dacl, &mut count, &mut entries);
        if result.0 != 0 {
            return Err(FsGuardError::Stat(win32_io_error(result.0)));
        }
        let _entries_guard = LocalAllocGuard(entries as *mut c_void);

        let entries = if entries.is_null() || count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(entries, count as usize)
        };

        for entry in entries {
            if entry.grfAccessMode != GRANT_ACCESS {
                continue;
            }
            if entry.Trustee.TrusteeForm != TRUSTEE_IS_SID {
                // `ptstrName` isn't a `PSID` for any other trustee form
                // (e.g. `TRUSTEE_IS_NAME`) — casting it to one anyway and
                // handing it to `EqualSid`/`ConvertSidToStringSidW` would be
                // reading garbage through an invalid pointer. `GetExplicitEntriesFromAclW`
                // is documented to always return SID-form trustees in
                // practice, but fail closed here instead of relying on
                // that: any write-ish grant to a non-SID-form trustee is
                // treated as insecure without ever touching `ptstrName` as
                // a SID.
                if (entry.grfAccessPermissions & WRITE_LIKE_RIGHTS) != 0 {
                    return Err(FsGuardError::InsecureAcl {
                        principal: "<non-SID trustee>".to_string(),
                        rights: format!("{:#x}", entry.grfAccessPermissions),
                    });
                }
                continue;
            }
            let entry_sid = PSID(entry.Trustee.ptstrName.0 as *mut c_void);
            let is_trusted = is_trusted_principal(entry_sid, current_user_sid, &trusted_well_known);
            if !is_trusted && (entry.grfAccessPermissions & WRITE_LIKE_RIGHTS) != 0 {
                return Err(FsGuardError::InsecureAcl {
                    principal: sid_to_string(entry_sid),
                    rights: format!("{:#x}", entry.grfAccessPermissions),
                });
            }
        }
        Ok(())
    }
}

/// Replaces `path`'s DACL with a single entry granting the current user
/// full control, with inheritance disabled, and explicitly sets the owner
/// to the current user too (defense in depth for `check_not_world_writable`'s
/// owner check — deterministic regardless of any unusual
/// impersonation/token context, rather than relying on whatever owner
/// `CreateFile`/`CreateDirectory` assigned implicitly). Used for both newly
/// created files (`set_private_file_permissions`) and newly created
/// directories (`ensure_private_dir`).
pub(crate) fn set_private_acl(path: &Path) -> Result<(), FsGuardError> {
    let wide = path_to_wide(path);
    let current_user_buf = current_user_token_buf()?;
    let current_user_sid = sid_in_token_buf(&current_user_buf);

    unsafe {
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS.0,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR(current_user_sid.0 as *mut u16),
            },
        };

        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let result = SetEntriesInAclW(Some(&[entry]), None, &mut new_acl);
        if result.0 != 0 {
            return Err(FsGuardError::SetPermissions(win32_io_error(result.0)));
        }
        let _acl_guard = LocalAllocGuard(new_acl as *mut c_void);

        let result = SetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            current_user_sid,
            PSID::default(),
            Some(new_acl as *const ACL),
            None,
        );
        if result.0 != 0 {
            return Err(FsGuardError::SetPermissions(win32_io_error(result.0)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_private_acl_then_check_not_world_writable_accepts_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "").unwrap();

        set_private_acl(&path).unwrap();
        check_not_world_writable(&path).unwrap();
    }

    #[test]
    fn check_not_world_writable_rejects_a_grant_to_everyone() {
        use windows::Win32::Security::WinWorldSid;
        use windows::Win32::Security::CreateWellKnownSid;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "").unwrap();
        set_private_acl(&path).unwrap();

        // Widen the DACL to also grant the well-known "Everyone" SID write
        // access, mirroring what `rejects_world_writable_file`'s Unix
        // sibling does by chmod-ing to `0o666`.
        let mut everyone_buf = [0u8; 64];
        let mut everyone_len = everyone_buf.len() as u32;
        unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                PSID::default(),
                PSID(everyone_buf.as_mut_ptr() as *mut c_void),
                &mut everyone_len,
            )
            .unwrap();
        }
        let everyone_sid = PSID(everyone_buf.as_mut_ptr() as *mut c_void);

        let wide = path_to_wide(&path);
        unsafe {
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_GENERIC_WRITE.0,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: PWSTR(everyone_sid.0 as *mut u16),
                },
            };
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let result = GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            );
            assert_eq!(result.0, 0);
            let _sd_guard = LocalAllocGuard(sd.0);

            let mut new_acl: *mut ACL = std::ptr::null_mut();
            let result = SetEntriesInAclW(Some(&[entry]), Some(dacl as *const ACL), &mut new_acl);
            assert_eq!(result.0, 0);
            let _acl_guard = LocalAllocGuard(new_acl as *mut c_void);

            let result = SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(new_acl as *const ACL),
                None,
            );
            assert_eq!(result.0, 0);
        }

        let err = check_not_world_writable(&path).unwrap_err();
        assert!(matches!(err, FsGuardError::InsecureAcl { .. }));
    }
}
