//! OS-authenticated, current-user-only local control transport.
use std::{io, os::windows::io::AsRawHandle, path::PathBuf, ptr};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        SystemInformation::GetTickCount64,
        Threading::{
            GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub creation: u64,
    pub executable: PathBuf,
    pub sid: String,
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one non-null Win32 handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Local(*mut core::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: pointer comes from a Win32 API that documents LocalFree ownership.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn checked(value: i32) -> io::Result<()> {
    if value == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn identity(pid: u32) -> io::Result<Identity> {
    // SAFETY: API receives a numeric PID, requests query access only, and returns an owned handle.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = Handle(raw);
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: owned process handle and four distinct writable outputs remain valid for the call.
    checked(unsafe {
        GetProcessTimes(
            process.0,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    })?;
    let mut path = vec![0u16; 32768];
    let mut len = 32768u32;
    // SAFETY: buffer holds len UTF-16 elements, process is queryable, len is writable.
    checked(unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &raw mut len) })?;
    let mut token = ptr::null_mut();
    // SAFETY: process is valid, output receives an owned query-only token handle.
    checked(unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &raw mut token) })?;
    let token = Handle(token);
    let mut needed = 0;
    // SAFETY: null/zero buffer is the documented size query, needed is writable.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err(io::Error::other("invalid token user size"));
    }
    // usize storage provides TOKEN_USER alignment; allocation is rounded upward.
    let mut storage = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned buffer has at least needed writable bytes, token and output are valid.
    checked(unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    })?;
    // SAFETY: successful TokenUser query initialized a properly aligned TOKEN_USER in storage.
    let sid = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
    let mut string = ptr::null_mut();
    // SAFETY: SID points into live successful token query; API allocates a terminated string.
    checked(unsafe { ConvertSidToStringSidW(sid, &raw mut string) })?;
    let allocated = Local(string.cast());
    let mut count = 0;
    // SAFETY: API guarantees a terminated UTF-16 string; scanning is bounded for defense in depth.
    while count < 1024 && unsafe { *string.add(count) } != 0 {
        count += 1;
    }
    if count == 1024 {
        return Err(io::Error::other("invalid SID text"));
    }
    // SAFETY: count was measured within the allocated terminated string.
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(string, count) });
    drop(allocated);
    Ok(Identity {
        pid,
        creation: (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
        executable: PathBuf::from(String::from_utf16_lossy(&path[..len as usize]))
            .canonicalize()?,
        sid,
    })
}

pub fn peer(pipe: &impl AsRawHandle, client: bool) -> io::Result<Identity> {
    let mut pid = 0;
    // SAFETY: AsRawHandle borrows a connected named-pipe handle; pid is a writable output.
    checked(unsafe {
        if client {
            GetNamedPipeClientProcessId(pipe.as_raw_handle(), &raw mut pid)
        } else {
            GetNamedPipeServerProcessId(pipe.as_raw_handle(), &raw mut pid)
        }
    })?;
    identity(pid)
}

pub fn pipe(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let own = identity(std::process::id())?;
    // Protected DACL grants only this SID. Remote clients are separately refused.
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", own.sid)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: terminated SDDL remains live; descriptor receives a LocalFree-owned allocation.
    checked(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    })?;
    let allocation = Local(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| io::Error::other("SECURITY_ATTRIBUTES size overflow"))?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: attributes and descriptor remain live until CreateNamedPipe returns; no borrowed pointer escapes.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(16)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    drop(allocation);
    result
}

#[must_use]
pub fn monotonic_millis() -> u64 {
    // SAFETY: GetTickCount64 takes no pointers and is process-independent on this boot.
    unsafe { GetTickCount64() }
}

pub fn interactive_session() -> io::Result<bool> {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    };
    let mut session = 0;
    // SAFETY: current PID is valid and session points to a writable u32.
    checked(unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) })?;
    if session == 0 {
        return Ok(false);
    }
    // SAFETY: the returned station is borrowed from this process, never closed here.
    let station = unsafe { GetProcessWindowStation() };
    if station.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut flags = USEROBJECTFLAGS::default();
    let length = u32::try_from(size_of::<USEROBJECTFLAGS>())
        .map_err(|_| io::Error::other("USEROBJECTFLAGS size overflow"))?;
    // SAFETY: station is a live borrowed handle, flags is a correctly sized writable struct.
    checked(unsafe {
        GetUserObjectInformationW(
            station,
            UOI_FLAGS,
            (&raw mut flags).cast(),
            length,
            ptr::null_mut(),
        )
    })?;
    // WSF_VISIBLE (Winuser.h): a nonzero session alone also includes scheduled,
    // non-interactive desktops where a login window would not be reachable.
    Ok(flags.dwFlags & 1 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pipe_os_acl_rejects_anonymous_sid_and_accepts_current_user() {
        let name = format!(r"\\.\pipe\cdr-acl-test-{}", uuid::Uuid::new_v4());
        let server = pipe(&name, true).unwrap();
        let other = name.clone();
        let denied = std::thread::spawn(move || {
            use windows_sys::Win32::{
                Security::{ImpersonateAnonymousToken, RevertToSelf},
                System::Threading::GetCurrentThread,
            };
            struct Restore;
            impl Drop for Restore {
                fn drop(&mut self) {
                    // SAFETY: this dedicated test thread owns its impersonation; always revert before exit.
                    assert_ne!(unsafe { RevertToSelf() }, 0);
                }
            }
            // SAFETY: GetCurrentThread is a borrowed pseudo-handle; impersonation is confined to this new thread.
            checked(unsafe { ImpersonateAnonymousToken(GetCurrentThread()) }).unwrap();
            let restore = Restore;
            let result = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(other);
            drop(restore);
            result.err().and_then(|error| error.raw_os_error())
        })
        .join()
        .unwrap();
        assert_eq!(
            denied,
            Some(5),
            "anonymous access must fail with ACCESS_DENIED"
        );
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .unwrap();
        server.connect().await.unwrap();
        assert_eq!(
            peer(&server, true).unwrap(),
            identity(std::process::id()).unwrap()
        );
        drop(client);
    }

    #[tokio::test]
    async fn pipe_rejects_remote_unc_transport_on_loopback() {
        let suffix = format!("cdr-remote-test-{}", uuid::Uuid::new_v4());
        let _server = pipe(&format!(r"\\.\pipe\{suffix}"), true).unwrap();
        let remote = format!(r"\\localhost\pipe\{suffix}");
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(remote)
                    .err()
                    .and_then(|e| e.raw_os_error())
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            error,
            Some(5),
            "remote transport must reach the pipe and receive ACCESS_DENIED, not just fail name resolution"
        );
    }
}
