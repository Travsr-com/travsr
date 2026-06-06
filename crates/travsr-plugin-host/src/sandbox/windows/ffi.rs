//! Unsafe Win32 FFI wrappers for AppContainer + Job Object sandbox (RFC-014).
//! All `unsafe` blocks in travsr-plugin-host are confined to this file.
#![allow(unsafe_code)]
#![allow(clippy::io_other_error)]

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    HANDLE_FLAG_INHERIT, HLOCAL, INVALID_HANDLE_VALUE, WAIT_FAILED,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, FreeSid, WinCapabilityInternetClientSid, DACL_SECURITY_INFORMATION, PSID,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessId,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, IO_COUNTERS, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

// ── Access masks ──────────────────────────────────────────────────────────────

pub(super) const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
pub(super) const ACCESS_GENERIC_ALL: u32 = 0x1000_0000;

// ── SE_GROUP_ENABLED for capability SID attributes ────────────────────────────

const SE_GROUP_ENABLED: u32 = 0x0000_0004;

// ── Job Object limits (ADR-017 Rule 1) ────────────────────────────────────────

const JOB_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024; // 4 GiB
const JOB_TIME_LIMIT: i64 = 3_000_000_000; // 300 s in 100-ns intervals
const JOB_ACTIVE_PROC_LIMIT: u32 = 64;

// ── SECURITY_MAX_SID_SIZE ─────────────────────────────────────────────────────

const SECURITY_MAX_SID_SIZE: usize = 68;

// ── PROC_THREAD_ATTRIBUTE constants ──────────────────────────────────────────
// Not exported as named consts in windows-sys 0.61; defined from Win32 SDK values.

const PROC_THREAD_ATTR_SECURITY_CAPABILITIES: usize = 0x0002_0009; // = 131081
const PROC_THREAD_ATTR_HANDLE_LIST: usize = 0x0002_0002; // = 131074

// ── Stdio mode (mirrors sandbox::StdioCfg without the cross-module dep) ──────

#[derive(Clone, Copy)]
pub(super) enum StdioMode {
    Pipe,
    Null,
    Inherit,
}

// ── RAII types ────────────────────────────────────────────────────────────────

/// RAII wrapper for a `PSID` obtained from `DeriveAppContainerSidFromAppContainerName`.
/// Freed via `FreeSid` on drop.
pub(super) struct AppContainerSid(PSID);

impl AppContainerSid {
    pub(super) fn as_psid(&self) -> PSID {
        self.0
    }
}

impl Drop for AppContainerSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { FreeSid(self.0) };
        }
    }
}

unsafe impl Send for AppContainerSid {}
unsafe impl Sync for AppContainerSid {}

/// Owned HANDLE that calls `CloseHandle` on drop (job-specific).
pub(super) struct OwnedJobHandle(HANDLE);

impl OwnedJobHandle {
    pub(super) fn as_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedJobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe impl Send for OwnedJobHandle {}
unsafe impl Sync for OwnedJobHandle {}

/// Owned generic HANDLE that calls `CloseHandle` on drop.
pub(super) struct OwnedHandle(pub(super) HANDLE);

impl OwnedHandle {
    pub(super) fn as_handle(&self) -> HANDLE {
        self.0
    }

    /// Transfer raw handle ownership out without calling `CloseHandle`.
    /// Use when wrapping in `std::fs::File::from_raw_handle`.
    pub(super) fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

/// RAII wrapper for an initialized `LPPROC_THREAD_ATTRIBUTE_LIST` buffer.
/// Calls `DeleteProcThreadAttributeList` on drop; the `Vec<u8>` frees the memory.
struct AttrList {
    buf: Vec<u8>,
}

impl AttrList {
    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr() as _
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        // Deinitialize the list; buf is freed when Vec drops.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

/// Handles returned by a successful `spawn_in_appcontainer` call.
pub(super) struct SpawnedHandles {
    pub process: OwnedHandle,
    pub _job: OwnedJobHandle, // KILL_ON_JOB_CLOSE fires when this drops
    pub pid: u32,
    pub stdin_write: Option<OwnedHandle>, // parent write end of stdin pipe
    pub stdout_read: Option<OwnedHandle>, // parent read end of stdout pipe
    pub stderr_read: Option<OwnedHandle>, // parent read end of stderr pipe
}

// ── Helper: convert a &str / &Path to null-terminated UTF-16 ─────────────────

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn path_to_wide(p: &Path) -> Vec<u16> {
    to_wide(&p.to_string_lossy())
}

// ── Public safe wrappers (called from windows.rs) ────────────────────────────

/// Derives the AppContainer SID for `profile_name`. Fail-closed: returns `Err` on failure.
pub(super) fn derive_appcontainer_sid(profile_name: &str) -> io::Result<AppContainerSid> {
    let wide = to_wide(profile_name);
    let mut sid: PSID = std::ptr::null_mut();
    let hr = unsafe { DeriveAppContainerSidFromAppContainerName(wide.as_ptr(), &mut sid) };
    if hr != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("DeriveAppContainerSidFromAppContainerName failed: HRESULT {hr:#010x}"),
        ));
    }
    Ok(AppContainerSid(sid))
}

/// Ensures the AppContainer profile exists. Ignores `ERROR_ALREADY_EXISTS`.
pub(super) fn ensure_appcontainer_profile(profile_name: &str) -> io::Result<()> {
    let name_wide = to_wide(profile_name);
    let desc_wide = to_wide("Travsr plugin sandbox");
    let mut out_sid: PSID = std::ptr::null_mut();
    let hr = unsafe {
        CreateAppContainerProfile(
            name_wide.as_ptr(),
            name_wide.as_ptr(),
            desc_wide.as_ptr(),
            std::ptr::null(),
            0,
            &mut out_sid,
        )
    };
    if !out_sid.is_null() {
        unsafe { FreeSid(out_sid) };
    }
    if hr != 0 {
        let err = hr as u32;
        if err != (0x8007_0000 | ERROR_ALREADY_EXISTS) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("CreateAppContainerProfile failed: HRESULT {hr:#010x}"),
            ));
        }
    }
    Ok(())
}

/// Adds an allow ACE for `sid` with `access_mask` to the DACL of `path`.
pub(super) fn grant_path_access(path: &Path, sid: PSID, access_mask: u32) -> io::Result<()> {
    let path_wide = path_to_wide(path);

    let mut old_dacl = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let err = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut old_dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    struct SdGuard(HLOCAL);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }
    let _sd_guard = SdGuard(sd);

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };

    let mut new_dacl = std::ptr::null_mut();
    let err = unsafe { SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl) };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    struct AclGuard(HLOCAL);
    impl Drop for AclGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }
    let _acl_guard = AclGuard(new_dacl as HLOCAL);

    let err = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Ok(())
}

/// Builds `SECURITY_CAPABILITIES` for Standard or Elevated policy.
pub(super) fn build_security_capabilities(
    container_sid: PSID,
    elevated: bool,
) -> io::Result<(
    [u8; SECURITY_MAX_SID_SIZE],
    Option<SID_AND_ATTRIBUTES>,
    SECURITY_CAPABILITIES,
)> {
    if elevated {
        let mut sid_buf = [0u8; SECURITY_MAX_SID_SIZE];
        let mut sid_size = SECURITY_MAX_SID_SIZE as u32;
        let ok = unsafe {
            CreateWellKnownSid(
                WinCapabilityInternetClientSid,
                std::ptr::null_mut(),
                sid_buf.as_mut_ptr() as PSID,
                &mut sid_size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let cap_attr = SID_AND_ATTRIBUTES {
            Sid: sid_buf.as_ptr() as PSID,
            Attributes: SE_GROUP_ENABLED,
        };
        let caps = SECURITY_CAPABILITIES {
            AppContainerSid: container_sid,
            Capabilities: &cap_attr as *const SID_AND_ATTRIBUTES as *mut SID_AND_ATTRIBUTES,
            CapabilityCount: 1,
            Reserved: 0,
        };
        Ok((sid_buf, Some(cap_attr), caps))
    } else {
        let caps = SECURITY_CAPABILITIES {
            AppContainerSid: container_sid,
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        Ok(([0u8; SECURITY_MAX_SID_SIZE], None, caps))
    }
}

/// Creates a Job Object with ADR-017 resource limits.
pub(super) fn create_job_with_limits() -> io::Result<OwnedJobHandle> {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }

    let limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_TIME
        | JOB_OBJECT_LIMIT_JOB_MEMORY;

    let jeli = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            PerProcessUserTimeLimit: 0,
            PerJobUserTimeLimit: JOB_TIME_LIMIT,
            LimitFlags: limit_flags,
            MinimumWorkingSetSize: 0,
            MaximumWorkingSetSize: 0,
            ActiveProcessLimit: JOB_ACTIVE_PROC_LIMIT,
            Affinity: 0,
            PriorityClass: 0,
            SchedulingClass: 0,
        },
        IoInfo: IO_COUNTERS {
            ReadOperationCount: 0,
            WriteOperationCount: 0,
            OtherOperationCount: 0,
            ReadTransferCount: 0,
            WriteTransferCount: 0,
            OtherTransferCount: 0,
        },
        ProcessMemoryLimit: 0,
        JobMemoryLimit: JOB_MEMORY_LIMIT,
        PeakProcessMemoryUsed: 0,
        PeakJobMemoryUsed: 0,
    };

    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &jeli as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        unsafe { CloseHandle(job) };
        return Err(io::Error::last_os_error());
    }

    Ok(OwnedJobHandle(job))
}

// ── P5-S3: CreateProcessW + AppContainer spawn ────────────────────────────────

/// Quote a single argument for a Windows command line.
fn quote_arg(arg: &str) -> String {
    if !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push_str("\\\"");
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Build a Windows command line string: `"program" arg1 arg2 ...`
fn build_command_line(program: &str, args: &[String]) -> String {
    let mut line = quote_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    line
}

/// Build a UTF-16 double-null-terminated environment block.
/// Contains an allowlist of non-sensitive parent variables plus
/// TEMP/TMP/TMPDIR forced to `scratch_dir` (PSE R2).
///
/// SYSTEMROOT, SystemDrive, COMPUTERNAME, OS, PROCESSOR_ARCHITECTURE are
/// included because the Windows AppContainer setup path expands %SYSTEMROOT%
/// internally using the child env block; omitting them causes CreateProcessW
/// to fail with ERROR_ENVVAR_NOT_FOUND (203).
pub(super) fn build_env_block(scratch_dir: &Path) -> Vec<u16> {
    const ALLOWLIST: &[&str] = &[
        // Shell / locale
        "PATH",
        "LANG",
        "LC_ALL",
        // Windows-required: AppContainer setup expands %SYSTEMROOT% from the child env
        "SYSTEMROOT",
        "SystemRoot",
        "SystemDrive",
        "COMPUTERNAME",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "WINDIR",
        // User identity (non-secret)
        "USERNAME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "APPDATA",
        "PUBLIC",
    ];
    let scratch = scratch_dir.to_string_lossy().to_string();
    let mut block: Vec<u16> = Vec::new();

    for var in ALLOWLIST {
        if let Ok(val) = std::env::var(var) {
            let entry = format!("{var}={val}");
            block.extend(entry.encode_utf16());
            block.push(0);
        }
    }
    // TEMP/TMP/TMPDIR → scratch dir (PSE R2: child may only write to scratch)
    for var in &["TEMP", "TMP", "TMPDIR"] {
        let entry = format!("{var}={scratch}");
        block.extend(entry.encode_utf16());
        block.push(0);
    }
    block.push(0); // final double-null terminator
    block
}

/// Create an anonymous pipe. Both ends are non-inheritable by default.
pub(super) fn create_pipe_pair() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read_h: HANDLE = std::ptr::null_mut();
    let mut write_h: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        CreatePipe(
            &mut read_h,
            &mut write_h,
            std::ptr::null::<SECURITY_ATTRIBUTES>(),
            0,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((OwnedHandle(read_h), OwnedHandle(write_h)))
}

/// Set `HANDLE_FLAG_INHERIT` on a handle so it can be listed in
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` and inherited by the child (PSE R1).
pub(super) fn make_inheritable(h: HANDLE) -> io::Result<()> {
    let ok = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Allocate and initialize an attribute list for `count` attributes.
fn init_attr_list(count: u32) -> io::Result<AttrList> {
    let mut size: usize = 0;
    // First call: query required size (expected to fail with ERROR_INSUFFICIENT_BUFFER).
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
    if size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("InitializeProcThreadAttributeList size query returned 0 (count={count})"),
        ));
    }
    let mut buf = vec![0u8; size];
    let ok =
        unsafe { InitializeProcThreadAttributeList(buf.as_mut_ptr() as _, count, 0, &mut size) };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "InitializeProcThreadAttributeList init failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(AttrList { buf })
}

/// Wrap an `OwnedHandle` as a writable `std::fs::File` (e.g. stdin pipe write end).
/// Ownership transfers into the File; CloseHandle is called on File::drop.
pub(super) fn handle_into_write_file(h: OwnedHandle) -> std::fs::File {
    use std::os::windows::io::FromRawHandle;
    unsafe { std::fs::File::from_raw_handle(h.into_raw() as _) }
}

/// Wrap an `OwnedHandle` as a readable `std::fs::File` (e.g. stdout pipe read end).
pub(super) fn handle_into_read_file(h: OwnedHandle) -> std::fs::File {
    use std::os::windows::io::FromRawHandle;
    unsafe { std::fs::File::from_raw_handle(h.into_raw() as _) }
}

/// Spawn `program` with `args` inside an AppContainer + Job Object.
///
/// # Safety contract (PSE R5)
/// `security_caps` must remain alive on the caller's stack until this function
/// returns. The `_cap_sid_buf` and `_cap_attr` from `build_security_capabilities`
/// must be bound as named locals in the calling frame.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_in_appcontainer(
    program: &str,
    args: &[String],
    scratch_dir: &Path,
    security_caps: &SECURITY_CAPABILITIES,
    job: OwnedJobHandle,
    stdin_mode: StdioMode,
    stdout_mode: StdioMode,
    stderr_mode: StdioMode,
) -> io::Result<SpawnedHandles> {
    // ── 1. Resolve stdio (keepalives stay alive until after CreateProcessW) ───
    //
    // For Pipe:   child end is an OwnedHandle we'll drop after CreateProcessW;
    //             parent end is returned to the caller in SpawnedHandles.
    // For Null:   NUL device File is kept open here; dropped after CreateProcessW.
    // For Inherit: parent's stdio handle, made inheritable; no owned resource.

    // stdin: child reads, parent writes
    let (stdin_child_h, stdin_parent_pipe, _stdin_nul);
    let stdin_child_pipe: Option<OwnedHandle>;
    match stdin_mode {
        StdioMode::Pipe => {
            let (r, w) = create_pipe_pair()?;
            make_inheritable(r.as_handle())?;
            stdin_child_h = r.as_handle();
            stdin_child_pipe = Some(r);
            stdin_parent_pipe = Some(w);
            _stdin_nul = None;
        }
        StdioMode::Null => {
            let f = std::fs::File::open("NUL")
                .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
            use std::os::windows::io::AsRawHandle;
            let h = f.as_raw_handle() as HANDLE;
            make_inheritable(h)?;
            stdin_child_h = h;
            stdin_child_pipe = None;
            stdin_parent_pipe = None;
            _stdin_nul = Some(f);
        }
        StdioMode::Inherit => {
            use std::os::windows::io::AsRawHandle;
            let h = std::io::stdin().as_raw_handle() as HANDLE;
            if !h.is_null() {
                make_inheritable(h)?;
            }
            stdin_child_h = h;
            stdin_child_pipe = None;
            stdin_parent_pipe = None;
            _stdin_nul = None;
        }
    }

    // stdout: child writes, parent reads
    let (stdout_child_h, stdout_parent_pipe, _stdout_nul);
    let stdout_child_pipe: Option<OwnedHandle>;
    match stdout_mode {
        StdioMode::Pipe => {
            let (r, w) = create_pipe_pair()?;
            make_inheritable(w.as_handle())?;
            stdout_child_h = w.as_handle();
            stdout_child_pipe = Some(w);
            stdout_parent_pipe = Some(r);
            _stdout_nul = None;
        }
        StdioMode::Null => {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open("NUL")
                .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
            use std::os::windows::io::AsRawHandle;
            let h = f.as_raw_handle() as HANDLE;
            make_inheritable(h)?;
            stdout_child_h = h;
            stdout_child_pipe = None;
            stdout_parent_pipe = None;
            _stdout_nul = Some(f);
        }
        StdioMode::Inherit => {
            use std::os::windows::io::AsRawHandle;
            let h = std::io::stdout().as_raw_handle() as HANDLE;
            if !h.is_null() {
                make_inheritable(h)?;
            }
            stdout_child_h = h;
            stdout_child_pipe = None;
            stdout_parent_pipe = None;
            _stdout_nul = None;
        }
    }

    // stderr: child writes, parent reads
    let (stderr_child_h, stderr_parent_pipe, _stderr_nul);
    let stderr_child_pipe: Option<OwnedHandle>;
    match stderr_mode {
        StdioMode::Pipe => {
            let (r, w) = create_pipe_pair()?;
            make_inheritable(w.as_handle())?;
            stderr_child_h = w.as_handle();
            stderr_child_pipe = Some(w);
            stderr_parent_pipe = Some(r);
            _stderr_nul = None;
        }
        StdioMode::Null => {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open("NUL")
                .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
            use std::os::windows::io::AsRawHandle;
            let h = f.as_raw_handle() as HANDLE;
            make_inheritable(h)?;
            stderr_child_h = h;
            stderr_child_pipe = None;
            stderr_parent_pipe = None;
            _stderr_nul = Some(f);
        }
        StdioMode::Inherit => {
            use std::os::windows::io::AsRawHandle;
            let h = std::io::stderr().as_raw_handle() as HANDLE;
            if !h.is_null() {
                make_inheritable(h)?;
            }
            stderr_child_h = h;
            stderr_child_pipe = None;
            stderr_parent_pipe = None;
            _stderr_nul = None;
        }
    }

    // ── 2. Build PROC_THREAD_ATTRIBUTE_LIST with 2 attributes ────────────────
    // Attribute 1: SECURITY_CAPABILITIES (AppContainer SID)
    // Attribute 2: HANDLE_LIST (exactly the 3 child-side stdio handles, PSE R1)

    let mut attr_list = init_attr_list(2)?;

    // Attribute 1: AppContainer SECURITY_CAPABILITIES (PSE R5: security_caps still live)
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list.as_mut_ptr(),
            0,
            PROC_THREAD_ATTR_SECURITY_CAPABILITIES,
            security_caps as *const SECURITY_CAPABILITIES as *const _,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }

    // Attribute 2: restrict inheritance to only the 3 child-side handles (PSE R1)
    let mut handle_list: Vec<HANDLE> = Vec::new();
    if !stdin_child_h.is_null() {
        handle_list.push(stdin_child_h);
    }
    if !stdout_child_h.is_null() {
        handle_list.push(stdout_child_h);
    }
    if !stderr_child_h.is_null() {
        handle_list.push(stderr_child_h);
    }

    if !handle_list.is_empty() {
        let ok = unsafe {
            UpdateProcThreadAttribute(
                attr_list.as_mut_ptr(),
                0,
                PROC_THREAD_ATTR_HANDLE_LIST,
                handle_list.as_ptr() as *const _,
                handle_list.len() * std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "UpdateProcThreadAttribute(HANDLE_LIST) failed: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
    }

    // ── 3. STARTUPINFOEXW (PSE R3: STARTF_USESTDHANDLES + CREATE_NO_WINDOW) ──
    let mut si_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si_ex.StartupInfo.hStdInput = stdin_child_h;
    si_ex.StartupInfo.hStdOutput = stdout_child_h;
    si_ex.StartupInfo.hStdError = stderr_child_h;
    si_ex.lpAttributeList = attr_list.as_mut_ptr();

    // ── 4. Command line, current directory, env block ────────────────────────
    let cmdline = build_command_line(program, args);
    let mut cmdline_wide = to_wide(&cmdline);
    let scratch_wide = path_to_wide(scratch_dir);
    let mut env_block = build_env_block(scratch_dir);

    // ── 5. CreateProcessW ─────────────────────────────────────────────────────
    // PSE R3: CREATE_NO_WINDOW | PSE R2: CREATE_UNICODE_ENVIRONMENT
    // PSE R4: lpCurrentDirectory = scratch_dir
    //
    // lpApplicationName is NULL: CreateProcessW then resolves the executable
    // from the first token of lpCommandLine using the standard search order
    // (app dir → current dir → System32 → Windows dir → PATH), which is the
    // only way to find bare exe names like "powershell.exe" on the PATH.
    // When `program` is already an absolute path it resolves directly.
    let creation_flags = CREATE_SUSPENDED
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_NO_WINDOW
        | CREATE_UNICODE_ENVIRONMENT;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),          // lpApplicationName = NULL (resolve from cmdline)
            cmdline_wide.as_mut_ptr(), // lpCommandLine (must be mutable)
            std::ptr::null(),          // lpProcessAttributes
            std::ptr::null(),          // lpThreadAttributes
            1,                         // bInheritHandles = TRUE (PSE R1)
            creation_flags,
            env_block.as_mut_ptr() as *const _, // lpEnvironment
            scratch_wide.as_ptr(),              // lpCurrentDirectory (PSE R4)
            &si_ex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "CreateProcessW({program:?}) failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }

    // Wrap process and thread handles immediately so they're closed on any failure.
    let process = OwnedHandle(pi.hProcess);
    let thread_h = OwnedHandle(pi.hThread); // closed after ResumeThread
    let pid = unsafe { GetProcessId(pi.hProcess) };

    // Child-side pipe ends no longer needed — drop to close our copies.
    drop(stdin_child_pipe);
    drop(stdout_child_pipe);
    drop(stderr_child_pipe);
    // NUL device files dropped here too; child has inherited handles.

    // ── 6. Assign to Job Object BEFORE resuming (PSE: job assigned first) ────
    let ok = unsafe { AssignProcessToJobObject(job.as_handle(), process.as_handle()) };
    if ok == 0 {
        let e = io::Error::last_os_error();
        unsafe { TerminateProcess(process.as_handle(), 1) };
        return Err(e);
    }

    // ── 7. Resume the process (was created suspended) ────────────────────────
    let prev = unsafe { ResumeThread(thread_h.as_handle()) };
    if prev == u32::MAX {
        let e = io::Error::last_os_error();
        unsafe { TerminateProcess(process.as_handle(), 1) };
        return Err(e);
    }
    // thread_h drops here (CloseHandle on thread is safe after ResumeThread)

    Ok(SpawnedHandles {
        process,
        _job: job,
        pid,
        stdin_write: stdin_parent_pipe,
        stdout_read: stdout_parent_pipe,
        stderr_read: stderr_parent_pipe,
    })
}

/// Wait for the process to exit and return the raw exit code.
pub(super) fn wait_for_process(handle: HANDLE) -> io::Result<u32> {
    let res = unsafe { WaitForSingleObject(handle, INFINITE) };
    if res == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code)
}

/// Terminate the process immediately with exit code 1.
pub(super) fn terminate_process(handle: HANDLE) -> io::Result<()> {
    let ok = unsafe { TerminateProcess(handle, 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

