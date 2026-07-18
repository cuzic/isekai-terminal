//! The real Windows implementation of [`crate::ExclusiveChannel`], backed by
//! `tokio::net::windows::named_pipe`.
//!
//! **Runtime-untested in this repo.** The named-pipe syscalls only exist on
//! Windows, and this project's development/CI-for-agents environment is Linux
//! — exactly like `isekai-ssh`'s own Windows-only modules
//! (`native::agent_auth::connect_agent`). This module is therefore verified
//! only via `cargo check`/`cargo clippy --target x86_64-pc-windows-gnu`; a
//! real Windows machine must confirm it actually works before it is relied on.
//! Everything that *can* be tested without Windows — the error-classification
//! and retry decisions — lives as pure functions in [`crate::pipe_classify`]
//! with host-independent unit tests.
//!
//! Design:
//! - `try_claim` uses `ServerOptions::first_pipe_instance(true)`; the
//!   `ERROR_ACCESS_DENIED`/[`io::ErrorKind::PermissionDenied`] it returns when
//!   the pipe already exists is mapped to [`crate::ClaimError::AlreadyClaimed`]
//!   (see [`crate::pipe_classify::is_already_claimed`]).
//! - `accept` follows the canonical tokio multi-client server loop (see
//!   `tokio::net::windows::named_pipe`'s crate docs): it always keeps one
//!   server instance waiting, `connect()`s it, then immediately creates the
//!   *next* instance so a client is never turned away with `NotFound` merely
//!   because the owner is between accepts.
//! - `connect` uses `ClientOptions::open`, retrying briefly on
//!   `ERROR_PIPE_BUSY` (the transient window while the owner prepares its next
//!   accepting instance) and failing fast with [`crate::ConnectError::NotFound`]
//!   on `ERROR_FILE_NOT_FOUND`.
//! - Every instance is created with a `SECURITY_ATTRIBUTES` whose DACL grants
//!   only the current user's SID (`same_user_security_attributes`), rather
//!   than relying on the default named-pipe permissions — mirroring
//!   `isekai-fs-guard::windows_acl`'s `set_private_acl` style.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;

use async_trait::async_trait;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    GetTokenInformation, InitializeSecurityDescriptor, SetSecurityDescriptorDacl, ACL,
    NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::pipe_classify::{
    classify_connect_error, is_already_claimed, should_retry_connect, ConnectDisposition,
    CONNECT_MAX_RETRIES, CONNECT_RETRY_BACKOFF,
};
use crate::{ClaimError, ConnectError, ExclusiveChannel};

const WIN_TRUE: BOOL = BOOL(1);
const WIN_FALSE: BOOL = BOOL(0);

/// One established named-pipe connection, from either end. The owner's
/// `accept()` yields a `Server` view and a client's `connect()` yields a
/// `Client` view; both are byte streams and implement `AsyncRead`/`AsyncWrite`,
/// so this enum lets the crate expose a single `Connection` associated type
/// (both tokio types are `Unpin`, so delegation needs no pin projection).
pub enum PipeConnection {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

impl tokio::io::AsyncRead for PipeConnection {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            PipeConnection::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            PipeConnection::Client(c) => std::pin::Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for PipeConnection {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.get_mut() {
            PipeConnection::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            PipeConnection::Client(c) => std::pin::Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            PipeConnection::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
            PipeConnection::Client(c) => std::pin::Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            PipeConnection::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            PipeConnection::Client(c) => std::pin::Pin::new(c).poll_shutdown(cx),
        }
    }
}

/// A real Windows named-pipe [`ExclusiveChannel`]. Holds the single pipe
/// server instance currently waiting to accept the next client; each
/// [`ExclusiveChannel::accept`] connects it and prepares the following one.
pub struct WindowsNamedPipeChannel {
    name: String,
    /// The next server instance, already created and waiting to be connected.
    /// `Some` between accepts; taken by `accept` and replaced with the
    /// following instance before returning.
    pending: Option<NamedPipeServer>,
}

impl WindowsNamedPipeChannel {
    /// Creates one server instance on `name` with a same-user DACL. `first`
    /// requests `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails if the pipe
    /// already exists — that's how ownership is claimed exactly once.
    fn create_server_instance(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let security = same_user_security_attributes()?;
        let mut attrs = security.security_attributes();
        // SAFETY: `attrs` points at a `SECURITY_DESCRIPTOR` (and, through it,
        // an ACL and SID buffer) all kept alive in `security`, which outlives
        // this call. `CreateNamedPipeW` copies the descriptor into the new
        // object during the call, so nothing here needs to outlive it.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(name, &mut attrs as *mut _ as *mut c_void)
        };
        drop(security);
        server
    }
}

#[async_trait]
impl ExclusiveChannel for WindowsNamedPipeChannel {
    type Connection = PipeConnection;

    async fn try_claim(name: &str) -> Result<Self, ClaimError> {
        match Self::create_server_instance(name, true) {
            Ok(server) => Ok(Self { name: name.to_string(), pending: Some(server) }),
            Err(e) if is_already_claimed(e.kind()) => {
                Err(ClaimError::AlreadyClaimed { name: name.to_string() })
            }
            Err(source) => Err(ClaimError::Io { name: name.to_string(), source }),
        }
    }

    async fn accept(&mut self) -> io::Result<Self::Connection> {
        // The pending instance is always present between accepts (set by
        // `try_claim`, and re-set below before every return).
        let server = self
            .pending
            .take()
            .expect("WindowsNamedPipeChannel::pending is always Some between accepts");
        server.connect().await?;
        // Prepare the next accepting instance immediately, so a client that
        // connects right after this one is served rather than seeing the pipe
        // as momentarily gone. A failure to create it must not lose the
        // just-connected client: report it only if the *next* accept is
        // attempted (leave `pending` as None; the expect above would then
        // fire — but `create_server_instance` failing here is the same class
        // of genuine channel failure `accept` is documented to surface).
        self.pending = Some(Self::create_server_instance(&self.name, false)?);
        Ok(PipeConnection::Server(server))
    }

    async fn connect(name: &str) -> Result<Self::Connection, ConnectError> {
        let mut retries_done = 0usize;
        loop {
            match ClientOptions::new().open(name) {
                Ok(client) => return Ok(PipeConnection::Client(client)),
                Err(source) => {
                    let disposition = classify_connect_error(source.raw_os_error(), source.kind());
                    match disposition {
                        ConnectDisposition::NotFound => {
                            return Err(ConnectError::NotFound { name: name.to_string() })
                        }
                        ConnectDisposition::Retry | ConnectDisposition::Fatal => {
                            if should_retry_connect(disposition, retries_done, CONNECT_MAX_RETRIES) {
                                retries_done += 1;
                                tokio::time::sleep(CONNECT_RETRY_BACKOFF).await;
                                continue;
                            }
                            return Err(ConnectError::Io { name: name.to_string(), source });
                        }
                    }
                }
            }
        }
    }
}

/// Owns every allocation a `SECURITY_ATTRIBUTES` transitively points at (the
/// current-user SID buffer, the LocalAlloc'd ACL, and the security
/// descriptor), so the whole graph stays valid for as long as this value
/// lives. The descriptor is boxed so its address is stable even if this struct
/// is moved before `security_attributes()` is called.
struct SameUserSecurity {
    _token_buf: Vec<u8>,
    _acl: LocalAllocGuard,
    descriptor: Box<SECURITY_DESCRIPTOR>,
}

impl SameUserSecurity {
    /// Builds a `SECURITY_ATTRIBUTES` referencing this value's owned
    /// descriptor. The returned struct borrows from `self`, so `self` must
    /// outlive every use of it (enforced by keeping `self` alive across the
    /// `create_*` call in `create_server_instance`).
    fn security_attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&*self.descriptor as *const SECURITY_DESCRIPTOR) as *mut c_void,
            bInheritHandle: WIN_FALSE,
        }
    }
}

/// Builds a security descriptor whose DACL grants full access to only the
/// current user's SID and nobody else — the named-pipe analogue of
/// `isekai-fs-guard::windows_acl::set_private_acl`.
fn same_user_security_attributes() -> io::Result<SameUserSecurity> {
    let token_buf = current_user_token_buf()?;
    let current_user_sid = sid_in_token_buf(&token_buf);

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

        let mut acl: *mut ACL = std::ptr::null_mut();
        let result = SetEntriesInAclW(Some(&[entry]), None, &mut acl);
        if result.0 != 0 {
            return Err(win32_io_error(result.0));
        }
        let acl_guard = LocalAllocGuard(acl as *mut c_void);

        let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
        let psd = PSECURITY_DESCRIPTOR(&mut *descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void);
        InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION)
            .map_err(|e| win32_io_error(e.code().0 as u32))?;
        SetSecurityDescriptorDacl(psd, WIN_TRUE, Some(acl as *const ACL), WIN_FALSE)
            .map_err(|e| win32_io_error(e.code().0 as u32))?;

        Ok(SameUserSecurity { _token_buf: token_buf, _acl: acl_guard, descriptor })
    }
}

/// Frees a `LocalAlloc`-backed pointer (the allocation convention
/// `SetEntriesInAclW` follows) on drop.
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

fn win32_io_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// Returns the raw `TOKEN_USER` buffer for the current process's token; the
/// `PSID` inside it (via [`sid_in_token_buf`]) is valid only while this buffer
/// lives. Mirrors `isekai-fs-guard::windows_acl::current_user_token_buf`.
fn current_user_token_buf() -> io::Result<Vec<u8>> {
    unsafe {
        let mut token = HANDLE(std::ptr::null_mut());
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| win32_io_error(e.code().0 as u32))?;
        let _guard = TokenGuard(token);

        let mut needed: u32 = 0;
        // Expected to fail with ERROR_INSUFFICIENT_BUFFER; `needed` is set
        // regardless (the standard Win32 "query the size first" pattern).
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(io::Error::other("GetTokenInformation returned no size"));
        }
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), needed, &mut needed)
            .map_err(|e| win32_io_error(e.code().0 as u32))?;
        Ok(buf)
    }
}

fn sid_in_token_buf(buf: &[u8]) -> PSID {
    // SAFETY: `buf` was sized and filled by `GetTokenInformation(TokenUser,
    // ...)`, which guarantees a valid `TOKEN_USER` at its start.
    unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid }
}
