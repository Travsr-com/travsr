//! Unsafe Win32 FFI wrappers for AppContainer + Job Object sandbox (ADR-017).
//! All `unsafe` blocks in travsr-plugin-host are confined to this file
//! (sanctioned by ADR-017 Amendment A2, which records the confinement,
//! encapsulation, and verification invariants this file must uphold).
#![allow(unsafe_code)]
#![allow(clippy::io_other_error)]

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::STILL_ACTIVE;
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL,
    INVALID_HANDLE_VALUE, WAIT_FAILED,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, ACCESS_MODE, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, FreeSid, GetAce, WinCapabilityInternetClientSid,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
    OBJECT_INHERIT_ACE, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
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
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, IO_COUNTERS,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

// ── Access masks ──────────────────────────────────────────────────────────────

pub(super) const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
pub(super) const ACCESS_GENERIC_ALL: u32 = 0x1000_0000;
/// Read + execute (GENERIC_READ | GENERIC_EXECUTE): the minimum an
/// AppContainer token needs to map and run a program image (PR #577 review).
pub(super) const ACCESS_GENERIC_READ_EXECUTE: u32 = 0x8000_0000 | 0x2000_0000;

// ── SE_GROUP_ENABLED for capability SID attributes ────────────────────────────

const SE_GROUP_ENABLED: u32 = 0x0000_0004;

// ── Job Object limits (ADR-017 Rule 1) ────────────────────────────────────────

const JOB_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024; // 4 GiB
const JOB_ACTIVE_PROC_LIMIT: u32 = 64;

/// #504: per-job user CPU time cap, in 100-ns intervals.
///
/// `PerJobUserTimeLimit` accumulates CPU time across every thread of every
/// process in the job. The old flat 300 s was chosen as a wall-clock analogue
/// of the transport's `INVOKE_TIMEOUT_SECS`, so a multithreaded Phase B pass
/// on 8 cores burned it in ~40 s of wall time and Windows terminated the
/// whole job (PluginCrashed, faster machines failing sooner). Scaling by the
/// logical core count restores the intended meaning — the cap cannot fire
/// before ~300 s of wall time even at full parallelism, so it only catches a
/// genuine runaway spin the transport watchdog has not already killed.
fn job_cpu_time_limit() -> i64 {
    const BASE_SECS: i64 = 300;
    const HUNDRED_NS_PER_SEC: i64 = 10_000_000;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as i64)
        .unwrap_or(1);
    BASE_SECS * cores * HUNDRED_NS_PER_SEC
}

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

/// ACE type byte for `ACCESS_ALLOWED_ACE` (Win32 `ACCESS_ALLOWED_ACE_TYPE`).
const ACE_TYPE_ACCESS_ALLOWED: u8 = 0;

/// True when any `ACCESS_ALLOWED_ACE` in `dacl` satisfies `matches`.
///
/// # Safety
/// `dacl` must be null or a valid, readable `ACL` (as returned by
/// `GetNamedSecurityInfoW`).
unsafe fn any_allow_ace(
    dacl: *const ACL,
    matches: impl Fn(*const ACCESS_ALLOWED_ACE) -> bool,
) -> bool {
    if dacl.is_null() {
        return false;
    }
    for i in 0..u32::from((*dacl).AceCount) {
        let mut ace_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        if GetAce(dacl, i, &mut ace_ptr) == 0 {
            continue;
        }
        let header = ace_ptr as *const ACE_HEADER;
        if (*header).AceType != ACE_TYPE_ACCESS_ALLOWED {
            continue;
        }
        if matches(ace_ptr as *const ACCESS_ALLOWED_ACE) {
            return true;
        }
    }
    false
}

/// # Safety
/// `ace` must be a valid `ACCESS_ALLOWED_ACE` and `sid` a valid SID.
unsafe fn ace_is_for_sid(ace: *const ACCESS_ALLOWED_ACE, sid: PSID) -> bool {
    EqualSid(std::ptr::addr_of!((*ace).SidStart) as PSID, sid) != 0
}

/// #505: true when `dacl` already carries an inheritable allow ACE for `sid`
/// whose mask covers `access_mask`.
///
/// # Safety
/// `dacl` must be null or a valid, readable `ACL` (as returned by
/// `GetNamedSecurityInfoW`); `sid` must be a valid SID.
unsafe fn dacl_has_inheritable_allow_ace(dacl: *const ACL, sid: PSID, access_mask: u32) -> bool {
    const INHERIT_BOTH: u8 = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8;
    any_allow_ace(dacl, |ace| unsafe {
        (*ace).Header.AceFlags & INHERIT_BOTH == INHERIT_BOTH
            && (*ace).Mask & access_mask == access_mask
            && ace_is_for_sid(ace, sid)
    })
}

/// #575: true when `dacl` carries any allow ACE for `sid`, whatever its mask or
/// inheritance flags.
///
/// # Safety
/// `dacl` must be null or a valid, readable `ACL` (as returned by
/// `GetNamedSecurityInfoW`); `sid` must be a valid SID.
unsafe fn dacl_has_any_allow_ace(dacl: *const ACL, sid: PSID) -> bool {
    any_allow_ace(dacl, |ace| unsafe { ace_is_for_sid(ace, sid) })
}

/// Adds an allow ACE for `sid` with `access_mask` to the DACL of `path`.
///
/// #505: idempotent. `SetNamedSecurityInfoW` with an inheritable ACE
/// propagates through the whole subtree — a full DACL rewrite of every file
/// under `path`. The AppContainer profile SID is deterministic per repo, so
/// once the grant exists this returns after a single security-descriptor
/// read instead of re-churning the tree on every sidecar spawn (minutes on a
/// monorepo, permanent MFT/USN write traffic, EDR alarms). Exactly one ACE
/// per (repo, SID, mask) persists on the tree; it is intentionally left in
/// place across spawns and removed by `revoke_path_access` when the repo is
/// deregistered (#575).
pub(super) fn grant_path_access(path: &Path, sid: PSID, access_mask: u32) -> io::Result<()> {
    grant_path_access_impl(path, sid, access_mask, SUB_CONTAINERS_AND_OBJECTS_INHERIT)
}

/// Grants access to exactly this object, with NO_INHERITANCE (0) — nothing
/// propagates to children. Used for ancestor-traverse grants (see
/// `grant_ancestor_traverse`): a plain `grant_path_access` would use
/// `SUB_CONTAINERS_AND_OBJECTS_INHERIT` and leak the grant to every sibling
/// under that ancestor, defeating repo isolation.
pub(super) fn grant_path_access_this_only(
    path: &Path,
    sid: PSID,
    access_mask: u32,
) -> io::Result<()> {
    grant_path_access_impl(path, sid, access_mask, 0)
}

/// Frees a `LocalAlloc`ed block (a security descriptor or an ACL) on drop.
struct LocalGuard(HLOCAL);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Reads the DACL of `path_wide`. The pointer borrows from the security
/// descriptor the returned guard owns, so the guard must outlive it.
fn read_dacl(path_wide: &[u16]) -> io::Result<(*mut ACL, LocalGuard)> {
    let mut dacl = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    let err = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Ok((dacl, LocalGuard(sd)))
}

fn ace_entry(
    sid: PSID,
    access_mask: u32,
    mode: ACCESS_MODE,
    inheritance: u32,
) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask,
        grfAccessMode: mode,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    }
}

/// Merges `ea` into `old_dacl` and writes the result back to `path_wide`.
fn apply_dacl_entry(
    path_wide: &[u16],
    old_dacl: *mut ACL,
    ea: &EXPLICIT_ACCESS_W,
) -> io::Result<()> {
    let mut new_dacl = std::ptr::null_mut();
    let err = unsafe { SetEntriesInAclW(1, ea, old_dacl, &mut new_dacl) };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    let _acl_guard = LocalGuard(new_dacl as HLOCAL);

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

fn grant_path_access_impl(
    path: &Path,
    sid: PSID,
    access_mask: u32,
    inheritance: u32,
) -> io::Result<()> {
    let path_wide = path_to_wide(path);
    let (old_dacl, _sd_guard) = read_dacl(&path_wide)?;

    // #505: grant already present → skip the subtree rewrite entirely. Only
    // meaningful for the inheriting case (dacl_has_inheritable_allow_ace only
    // matches (OI)(CI) aces); the this-only grant always re-applies, which is
    // idempotent and cheap on the short ancestor chains it's used for.
    if inheritance == SUB_CONTAINERS_AND_OBJECTS_INHERIT
        && unsafe { dacl_has_inheritable_allow_ace(old_dacl, sid, access_mask) }
    {
        return Ok(());
    }

    let ea = ace_entry(sid, access_mask, GRANT_ACCESS, inheritance);
    apply_dacl_entry(&path_wide, old_dacl, &ea)
}

fn is_missing_object(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code)
        if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32)
}

/// #575: removes every allow ACE for `sid` from the DACL of `path`, whatever
/// its mask or inheritance flags, and reports whether anything was removed.
///
/// Idempotent, so deregistration never fails on residue that is already gone:
/// a path that no longer exists and a DACL carrying no ACE for `sid` both
/// return `Ok(false)` without touching the object. The pre-check also keeps the
/// inheritance propagation walk (the cost #505 removed) off every path that was
/// never granted.
pub(super) fn revoke_path_access(path: &Path, sid: PSID) -> io::Result<bool> {
    let path_wide = path_to_wide(path);
    let (old_dacl, _sd_guard) = match read_dacl(&path_wide) {
        Ok(dacl) => dacl,
        Err(e) if is_missing_object(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    if !unsafe { dacl_has_any_allow_ace(old_dacl, sid) } {
        return Ok(false);
    }

    // REVOKE_ACCESS drops every entry for the trustee, so the mask and
    // inheritance an ACE was granted with do not have to be reproduced here.
    let ea = ace_entry(sid, 0, REVOKE_ACCESS, 0);
    apply_dacl_entry(&path_wide, old_dacl, &ea)?;
    Ok(true)
}

/// #575: deletes the AppContainer profile and its on-disk profile directory,
/// reporting whether one existed. An absent profile is not an error.
pub(super) fn delete_appcontainer_profile(profile_name: &str) -> io::Result<bool> {
    const HRESULT_WIN32: u32 = 0x8007_0000;
    let wide = to_wide(profile_name);
    let hr = unsafe { DeleteAppContainerProfile(wide.as_ptr()) };
    if hr == 0 {
        return Ok(true);
    }
    let err = hr as u32;
    if err == (HRESULT_WIN32 | ERROR_NOT_FOUND) || err == (HRESULT_WIN32 | ERROR_FILE_NOT_FOUND) {
        return Ok(false);
    }
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("DeleteAppContainerProfile failed: HRESULT {hr:#010x}"),
    ))
}

/// Grants FILE_TRAVERSE (0x0020) — "pass through," not "list contents" — on
/// every ancestor directory of `path`, non-inherited, so the AppContainer
/// token can resolve `path`'s canonical/real form.
///
/// Windows AppContainer tokens hold no `SeChangeNotifyPrivilege` (the
/// privilege normal tokens use to bypass per-component traverse checking), so
/// unlike a regular user process, an AppContainer token needs an explicit
/// FILE_TRAVERSE grant on EVERY directory between the volume root and the
/// granted object — not just the object itself — before the OS will resolve
/// its real path. A direct `CreateFile` open of a fully-qualified path still
/// works without this (confirmed: reads/writes to files under `repo_root`
/// succeed throughout), but `GetFinalPathNameByHandleW` — what
/// `std::fs::canonicalize` and Java NIO's `Path.toRealPath()` both call —
/// does the full per-component walk and fails with ACCESS_DENIED without it.
/// Found via sbt 2.x: `--server` mode's `bootServerSocket` derives its IPC
/// socket identity from `toRealPath()` on the project root, so it hit this
/// even though the wrapper's file I/O on the same root worked fine.
///
/// FILE_TRAVERSE alone (not a GENERIC_* mask) excludes FILE_LIST_DIRECTORY
/// (0x0001), so this cannot enumerate an ancestor's own contents — sibling
/// repos under the same parent stay invisible. Best-effort per ancestor: a
/// directory whose owner isn't the current user (well above the repo, e.g.
/// close to the volume root) may not be re-ACL-able without admin, in which
/// case canonicalization stays broken for that specific tool but every other
/// sandboxed operation (which doesn't need the real/canonical path) is
/// unaffected.
pub(super) fn grant_ancestor_traverse(path: &Path, sid: PSID) {
    const FILE_TRAVERSE: u32 = 0x0000_0020;
    for ancestor in path.ancestors().skip(1) {
        let _ = grant_path_access_this_only(ancestor, sid, FILE_TRAVERSE);
    }
}

/// #499: owns the storage a `SECURITY_CAPABILITIES` points into, so the
/// self-referential pointers stay valid however the value is moved.
///
/// `caps.Capabilities` points at the boxed `SID_AND_ATTRIBUTES`, whose `Sid`
/// points at the boxed SID buffer. Box gives both stable heap addresses:
/// moving `OwnedSecurityCapabilities` moves only the box pointers, never the
/// pointees. `AppContainerSid` is caller-owned and not covered here.
pub(super) struct OwnedSecurityCapabilities {
    _sid_buf: Option<Box<[u8; SECURITY_MAX_SID_SIZE]>>,
    _cap_attr: Option<Box<SID_AND_ATTRIBUTES>>,
    caps: SECURITY_CAPABILITIES,
}

impl OwnedSecurityCapabilities {
    /// The `SECURITY_CAPABILITIES` to hand to `UpdateProcThreadAttribute`.
    /// Valid for as long as `self` is alive (PSE R5).
    pub(super) fn caps(&self) -> &SECURITY_CAPABILITIES {
        &self.caps
    }
}

/// Builds `SECURITY_CAPABILITIES` for Standard or Elevated policy.
///
/// #499: previously returned `(sid_buf, cap_attr, caps)` by value, which
/// moved the buffers while `caps` kept pointers into the callee's dead stack
/// frame — every Elevated spawn read freed memory. The capability data is now
/// heap-pinned before the internal pointers are taken.
pub(super) fn build_security_capabilities(
    container_sid: PSID,
    elevated: bool,
) -> io::Result<OwnedSecurityCapabilities> {
    if elevated {
        let mut sid_buf = Box::new([0u8; SECURITY_MAX_SID_SIZE]);
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
        let cap_attr = Box::new(SID_AND_ATTRIBUTES {
            Sid: sid_buf.as_ptr() as PSID,
            Attributes: SE_GROUP_ENABLED,
        });
        let caps = SECURITY_CAPABILITIES {
            AppContainerSid: container_sid,
            Capabilities: &*cap_attr as *const SID_AND_ATTRIBUTES as *mut SID_AND_ATTRIBUTES,
            CapabilityCount: 1,
            Reserved: 0,
        };
        Ok(OwnedSecurityCapabilities {
            _sid_buf: Some(sid_buf),
            _cap_attr: Some(cap_attr),
            caps,
        })
    } else {
        Ok(OwnedSecurityCapabilities {
            _sid_buf: None,
            _cap_attr: None,
            caps: SECURITY_CAPABILITIES {
                AppContainerSid: container_sid,
                Capabilities: std::ptr::null_mut(),
                CapabilityCount: 0,
                Reserved: 0,
            },
        })
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
            PerJobUserTimeLimit: job_cpu_time_limit(),
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

/// Insert or (case-insensitively, matching Windows env-name semantics)
/// overwrite an entry, so the final block never carries duplicate names.
fn upsert_env(entries: &mut Vec<(String, String)>, key: &str, val: String) {
    if let Some(e) = entries
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
    {
        e.1 = val;
    } else {
        entries.push((key.to_string(), val));
    }
}

/// Build a UTF-16 double-null-terminated environment block.
/// Contains an allowlist of non-sensitive parent variables, the language's
/// toolchain env passthrough (#501), `~/.travsr/bin` prepended to `PATH`
/// (#501), and TEMP/TMP/TMPDIR forced to `scratch_dir` (PSE R2).
///
/// SYSTEMROOT, SystemDrive, COMPUTERNAME, OS, PROCESSOR_ARCHITECTURE are
/// included because the Windows AppContainer setup path expands %SYSTEMROOT%
/// internally using the child env block; omitting them causes CreateProcessW
/// to fail with ERROR_ENVVAR_NOT_FOUND (203).
///
/// #501: this explicit block replaces the child env entirely — nothing is
/// inherited. Toolchain env (JAVA_HOME/GOPATH/GRADLE_USER_HOME/…) must
/// therefore be forwarded here, mirroring what linux.rs and macos.rs do for
/// their cleared sandbox envs; without it the sandbox ACL-grants the cache
/// directories but the analyzer has no variables telling it where they are.
pub(super) fn build_env_block(scratch_dir: &Path, toolchain_env: &[(String, String)]) -> Vec<u16> {
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
    let mut entries: Vec<(String, String)> = Vec::new();

    for var in ALLOWLIST {
        if let Ok(val) = std::env::var(var) {
            upsert_env(&mut entries, var, val);
        }
    }

    // #501: per-language toolchain env so the analyzer's build tool can locate
    // the caches the sandbox ACL-grants (windows.rs grants the paths).
    for (key, val) in toolchain_env {
        upsert_env(&mut entries, key, val.clone());
    }

    // #501: prepend ~/.travsr/bin so tools installed by `travsr lang install`
    // (e.g. scip-java, scip-go) resolve inside the sandbox.
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let travsr_bin = format!("{profile}\\.travsr\\bin");
        let base = entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let joined = if base.is_empty() {
            travsr_bin
        } else {
            format!("{travsr_bin};{base}")
        };
        upsert_env(&mut entries, "PATH", joined);
    }

    // TEMP/TMP/TMPDIR → scratch dir (PSE R2: child may only write to scratch).
    // Applied last so nothing above — toolchain env included — can redirect
    // the child's temp dir outside the scratch grant.
    let scratch = scratch_dir.to_string_lossy().to_string();
    for var in &["TEMP", "TMP", "TMPDIR"] {
        upsert_env(&mut entries, var, scratch.clone());
    }

    let mut block: Vec<u16> = Vec::new();
    for (key, val) in &entries {
        block.extend(format!("{key}={val}").encode_utf16());
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

/// Spawn a fully detached child with handle inheritance restricted to an
/// explicit allowlist of its own three NUL stdio handles (#572 residual).
///
/// `std::process::Command` forces `bInheritHandles=TRUE` whenever stdio is
/// configured, which hands the child *every* inheritable handle in this
/// process — not just the three std handles. The previous fix cleared
/// `HANDLE_FLAG_INHERIT` on our own std handles, which covered the direct
/// case (`travsr daemon start | tail`, where the leaked pipe handle IS our
/// stdout), but an inheritable handle a grandparent created (cargo → test
/// harness → travsr) and passed down without installing it as our stdout
/// still leaked into the long-lived daemon and pinned its pipe open: the
/// reader never saw EOF. `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` inverts the
/// model — only listed handles are inherited, whatever any other handle's
/// inherit flag says — so nothing else can leak, and per-handle flag
/// clearing is superseded.
///
/// The child is started with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
/// (#503: no console at all, out of the parent's Ctrl+C signal group) and
/// NUL stdio, and inherits this process's environment and current directory,
/// matching the `std::process::Command` spawn it replaces. Public because
/// the CLI's daemonizing re-exec (`travsr-cli`, `forbid(unsafe_code)`) is
/// the caller; the unsafe stays confined to ffi.rs per ADR-017 A2.
pub fn spawn_detached_with_inherit_allowlist(exe: &Path, args: &[&str]) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    // NUL stdio, one open per stream (mirrors spawn_in_appcontainer's Null
    // arms). The Files must outlive CreateProcessW; they drop on return.
    let stdin_nul = std::fs::File::open("NUL")
        .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
    let stdout_nul = std::fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
    let stderr_nul = std::fs::OpenOptions::new()
        .write(true)
        .open("NUL")
        .map_err(|e| io::Error::new(e.kind(), format!("NUL open failed: {e}")))?;
    let handle_list: [HANDLE; 3] = [
        stdin_nul.as_raw_handle() as HANDLE,
        stdout_nul.as_raw_handle() as HANDLE,
        stderr_nul.as_raw_handle() as HANDLE,
    ];
    for h in handle_list {
        make_inheritable(h)?;
    }

    // Single attribute: the inherit allowlist — exactly the child's stdio.
    let mut attr_list = init_attr_list(1)?;
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list.as_mut_ptr(),
            0,
            PROC_THREAD_ATTR_HANDLE_LIST,
            handle_list.as_ptr() as *const _,
            std::mem::size_of_val(&handle_list),
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

    let mut si_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si_ex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si_ex.StartupInfo.hStdInput = handle_list[0];
    si_ex.StartupInfo.hStdOutput = handle_list[1];
    si_ex.StartupInfo.hStdError = handle_list[2];
    si_ex.lpAttributeList = attr_list.as_mut_ptr();

    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let cmdline = build_command_line(&exe.to_string_lossy(), &args);
    let mut cmdline_wide = to_wide(&cmdline);

    let creation_flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // lpEnvironment / lpCurrentDirectory NULL → child inherits ours, matching
    // std::process::Command. bInheritHandles=TRUE is required for
    // STARTF_USESTDHANDLES; the HANDLE_LIST attribute above restricts it.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),          // lpApplicationName (resolve from cmdline)
            cmdline_wide.as_mut_ptr(), // lpCommandLine (must be mutable)
            std::ptr::null(),          // lpProcessAttributes
            std::ptr::null(),          // lpThreadAttributes
            1,                         // bInheritHandles = TRUE
            creation_flags,
            std::ptr::null(), // lpEnvironment = NULL (inherit)
            std::ptr::null(), // lpCurrentDirectory = NULL (inherit)
            &si_ex.StartupInfo as *const STARTUPINFOW,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "CreateProcessW({exe:?}) failed: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    // The daemon owns its own lifetime — the caller confirms liveness via the
    // repo flock, never via these handles — so close both immediately.
    drop(OwnedHandle(pi.hThread));
    drop(OwnedHandle(pi.hProcess));
    Ok(())
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
/// `security_caps` must come from a live `OwnedSecurityCapabilities` (#499):
/// the owner heap-pins the capability SID and `SID_AND_ATTRIBUTES` storage the
/// struct points into, and must outlive this call.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_in_appcontainer(
    program: &str,
    args: &[String],
    scratch_dir: &Path,
    toolchain_env: &[(String, String)],
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
    let mut env_block = build_env_block(scratch_dir, toolchain_env);

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

/// #500: true if a process with `pid` is currently running.
///
/// Opens the process with `PROCESS_QUERY_LIMITED_INFORMATION` (works for
/// non-child processes and across integrity levels) and checks
/// `GetExitCodeProcess` for `STILL_ACTIVE`. Returns `false` when the PID
/// cannot be opened (already exited and reaped) or an exit code is recorded.
/// Sends no signal. `pub(crate)`: `embed_catalog::pid_alive` delegates here so
/// the daemon-shutdown grace poll can observe the sidecar, keeping all unsafe
/// confined to this file.
pub(crate) fn pid_alive(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let handle = OwnedHandle(handle);
    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle.as_handle(), &mut code) };
    ok != 0 && code == STILL_ACTIVE as u32
}

/// Walk the `GetLogicalProcessorInformationEx(RelationProcessorCore)` buffer and
/// count processor cores whose `EfficiencyClass` equals the maximum observed value.
/// On homogeneous systems every core has the same class → returns total physical cores.
/// On Intel hybrid (12th gen+) P-cores have a higher class than E-cores.
///
/// `pub(crate)`: `embed_catalog::p_core_count` delegates here (as with
/// `pid_alive`), keeping all unsafe confined to this file per ADR-017
/// Amendment A2 Invariant 1.
pub(crate) fn windows_p_core_count() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
        SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    // First call: discover required buffer size.
    let mut buf_size: u32 = 0;
    // SAFETY: passing null buffer is the documented way to query size.
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            std::ptr::null_mut(),
            &mut buf_size,
        );
    }
    if buf_size == 0 {
        return None;
    }

    let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
    // SAFETY: buf is correctly sized from the previous call.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
            &mut buf_size,
        )
    };
    if ok == 0 {
        return None;
    }

    // Walk the variable-length buffer.  Each entry starts with a `Size` field
    // that gives its own byte length (entries are not fixed-size).
    let mut offset = 0usize;
    let mut max_class: u8 = 0;
    let mut class_counts: Vec<(u8, usize)> = Vec::new();

    while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
        <= buf_size as usize
    {
        // SAFETY: we bounds-check before casting.
        let entry = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        let entry_size = entry.Size as usize;
        if entry_size == 0 || offset + entry_size > buf_size as usize {
            break;
        }
        // SAFETY: Relationship == RelationProcessorCore, so the union field is Processor.
        let efficiency_class = unsafe { entry.Anonymous.Processor.EfficiencyClass };
        if efficiency_class > max_class {
            max_class = efficiency_class;
        }
        match class_counts
            .iter_mut()
            .find(|(c, _)| *c == efficiency_class)
        {
            Some((_, n)) => *n += 1,
            None => class_counts.push((efficiency_class, 1)),
        }
        offset += entry_size;
    }

    let p_count = class_counts
        .iter()
        .find(|(c, _)| *c == max_class)
        .map(|(_, n)| *n)
        .unwrap_or(0);
    if p_count > 0 {
        Some(p_count)
    } else {
        None
    }
}

/// Best-effort AVAILABLE physical RAM in MiB via `GlobalMemoryStatusEx` →
/// `ullAvailPhys`. Returns `None` on API failure — the caller falls back to
/// skipping its RAM guard.
///
/// `pub(crate)`: `embed_catalog::available_memory_mb` delegates here (as with
/// `pid_alive`), keeping all unsafe confined to this file per ADR-017
/// Amendment A2 Invariant 1.
pub(crate) fn available_physical_memory_mb() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut mem = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: mem is zero-initialised with dwLength correctly set.
    if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
        Some(mem.ullAvailPhys / (1024 * 1024))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{EqualSid, IsValidSid};

    fn well_known_sid() -> Box<[u8; SECURITY_MAX_SID_SIZE]> {
        let mut buf = Box::new([0u8; SECURITY_MAX_SID_SIZE]);
        let mut size = SECURITY_MAX_SID_SIZE as u32;
        let ok = unsafe {
            CreateWellKnownSid(
                WinCapabilityInternetClientSid,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as PSID,
                &mut size,
            )
        };
        assert_ne!(ok, 0, "CreateWellKnownSid failed");
        buf
    }

    /// #499 regression: the SECURITY_CAPABILITIES internal pointers must
    /// target the owner's heap storage, so they survive the owner being
    /// moved (returning it from build_security_capabilities was the bug).
    #[test]
    fn elevated_capability_pointers_survive_moves() {
        let container = well_known_sid();
        let owned = build_security_capabilities(container.as_ptr() as PSID, true).unwrap();

        // Move the owner to a new address; heap pointees must not move.
        let moved = Box::new(owned);
        let caps = moved.caps();
        assert_eq!(caps.CapabilityCount, 1);

        let cap_attr_ptr = caps.Capabilities as *const SID_AND_ATTRIBUTES;
        let expected_attr: &SID_AND_ATTRIBUTES = moved._cap_attr.as_ref().unwrap();
        assert_eq!(
            cap_attr_ptr, expected_attr as *const SID_AND_ATTRIBUTES,
            "Capabilities must point at the owner's boxed SID_AND_ATTRIBUTES"
        );

        let sid_ptr = unsafe { (*cap_attr_ptr).Sid };
        let expected_sid = moved._sid_buf.as_ref().unwrap().as_ptr() as PSID;
        assert_eq!(
            sid_ptr, expected_sid,
            "capability Sid must point at the owner's boxed SID buffer"
        );

        assert_ne!(
            unsafe { IsValidSid(sid_ptr) },
            0,
            "capability SID must be valid"
        );
        let reference = well_known_sid();
        assert_ne!(
            unsafe { EqualSid(sid_ptr, reference.as_ptr() as PSID) },
            0,
            "capability SID must be WinCapabilityInternetClientSid"
        );
    }

    /// Standard (non-elevated) policy must carry no capability list at all.
    #[test]
    fn standard_capabilities_have_no_capability_list() {
        let container = well_known_sid();
        let owned = build_security_capabilities(container.as_ptr() as PSID, false).unwrap();
        let caps = owned.caps();
        assert_eq!(caps.CapabilityCount, 0);
        assert!(caps.Capabilities.is_null());
        assert_eq!(caps.AppContainerSid, container.as_ptr() as PSID);
    }

    // ── #501: build_env_block — toolchain env passthrough ──────────────────

    fn decode_env_block(block: &[u16]) -> Vec<String> {
        String::from_utf16_lossy(block)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn env_value<'a>(entries: &'a [String], key: &str) -> Option<&'a str> {
        entries.iter().find_map(|e| {
            let (k, v) = e.split_once('=')?;
            k.eq_ignore_ascii_case(key).then_some(v)
        })
    }

    /// #501 regression: toolchain env must reach the child env block, and
    /// ~/.travsr/bin must lead PATH — previously the fixed allowlist silently
    /// dropped both, so Phase B analyzers could not find the caches the
    /// sandbox had ACL-granted.
    #[test]
    fn env_block_forwards_toolchain_env_and_prepends_travsr_bin() {
        let scratch = std::env::temp_dir();
        let tc = vec![
            ("GOPATH".to_string(), "C:\\test-gopath".to_string()),
            ("JAVA_HOME".to_string(), "C:\\test-jdk".to_string()),
        ];
        let entries = decode_env_block(&build_env_block(&scratch, &tc));

        assert_eq!(env_value(&entries, "GOPATH"), Some("C:\\test-gopath"));
        assert_eq!(env_value(&entries, "JAVA_HOME"), Some("C:\\test-jdk"));

        let profile = std::env::var("USERPROFILE").expect("USERPROFILE set on Windows");
        let path = env_value(&entries, "PATH").expect("PATH present");
        assert!(
            path.starts_with(&format!("{profile}\\.travsr\\bin")),
            "PATH must start with ~/.travsr/bin, got: {path}"
        );

        // Allowlisted system vars still present (AppContainer setup needs them).
        assert!(env_value(&entries, "SYSTEMROOT").is_some());
    }

    /// PSE R2 must win over toolchain env: TEMP/TMP/TMPDIR always point at the
    /// scratch dir even if a toolchain entry tries to redirect them.
    #[test]
    fn env_block_scratch_temp_overrides_toolchain_env() {
        let scratch = std::env::temp_dir();
        let scratch_s = scratch.to_string_lossy().to_string();
        let tc = vec![("TEMP".to_string(), "C:\\somewhere-else".to_string())];
        let entries = decode_env_block(&build_env_block(&scratch, &tc));

        for var in ["TEMP", "TMP", "TMPDIR"] {
            assert_eq!(
                env_value(&entries, var),
                Some(scratch_s.as_str()),
                "{var} must be forced to the scratch dir"
            );
        }
    }

    // ── #505: grant_path_access idempotence ─────────────────────────────────

    /// DACL of a path plus the guard keeping its backing SD allocation alive.
    struct DaclHandle {
        dacl: *mut ACL,
        sd: HLOCAL,
    }
    impl Drop for DaclHandle {
        fn drop(&mut self) {
            if !self.sd.is_null() {
                unsafe { LocalFree(self.sd) };
            }
        }
    }

    fn read_dacl(path: &Path) -> DaclHandle {
        let path_wide = to_wide(&path.to_string_lossy());
        let mut dacl = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let err = unsafe {
            GetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(err, ERROR_SUCCESS, "GetNamedSecurityInfoW failed");
        DaclHandle {
            dacl,
            sd: sd as HLOCAL,
        }
    }

    /// #505 regression: a repeated grant must detect the existing ACE and
    /// skip the subtree DACL rewrite — previously every sidecar spawn
    /// re-propagated ACLs across the entire repo tree.
    #[test]
    fn grant_path_access_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("travsr-acl-505-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let sid = derive_appcontainer_sid("travsr-test-505-acl").expect("derive sid");

        // Before any grant: the ACE must not be reported present.
        {
            let h = read_dacl(&dir);
            assert!(
                !unsafe {
                    dacl_has_inheritable_allow_ace(h.dacl, sid.as_psid(), ACCESS_GENERIC_READ)
                },
                "fresh dir must not carry the AppContainer ACE"
            );
        }

        grant_path_access(&dir, sid.as_psid(), ACCESS_GENERIC_READ).expect("first grant");

        // After the grant: detectable, so the second grant takes the skip
        // path and the ACE count stays put.
        let count_after_first = {
            let h = read_dacl(&dir);
            assert!(
                unsafe {
                    dacl_has_inheritable_allow_ace(h.dacl, sid.as_psid(), ACCESS_GENERIC_READ)
                },
                "granted ACE must be detected on re-read"
            );
            unsafe { (*h.dacl).AceCount }
        };

        grant_path_access(&dir, sid.as_psid(), ACCESS_GENERIC_READ).expect("second grant");
        let count_after_second = {
            let h = read_dacl(&dir);
            unsafe { (*h.dacl).AceCount }
        };
        assert_eq!(
            count_after_first, count_after_second,
            "repeat grant must not touch the DACL"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Like `dacl_has_inheritable_allow_ace` but ignoring inherit flags —
    /// ACEs inherited BY a file carry INHERITED_ACE, not the inherit bits,
    /// so file-level assertions need a flag-agnostic scan.
    unsafe fn dacl_has_allow_ace_any_flags(dacl: *const ACL, sid: PSID, mask: u32) -> bool {
        if dacl.is_null() {
            return false;
        }
        for i in 0..u32::from((*dacl).AceCount) {
            let mut ace_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &mut ace_ptr) == 0 {
                continue;
            }
            let header = ace_ptr as *const ACE_HEADER;
            if (*header).AceType != ACE_TYPE_ACCESS_ALLOWED {
                continue;
            }
            let ace = ace_ptr as *const ACCESS_ALLOWED_ACE;
            if (*ace).Mask & mask != mask {
                continue;
            }
            let ace_sid = std::ptr::addr_of!((*ace).SidStart) as PSID;
            if EqualSid(ace_sid, sid) != 0 {
                return true;
            }
        }
        false
    }

    /// PR #577 review: after travsr-store's owner-only hardening of ~/.travsr
    /// (#507: icacls /inheritance:r + user-only (OI)(CI)F), an AppContainer
    /// token has no path to binaries under ~/.travsr/bin unless the spawn
    /// grants one explicitly. This pins the exact sequence the spawn now
    /// performs: restriction strips the AppContainer's access, the
    /// read+execute grant restores it on the directory AND propagates to a
    /// pre-existing file inside (the plugin binary the child must map).
    #[test]
    fn grant_read_execute_restores_access_after_owner_only_restriction() {
        let dir = std::env::temp_dir().join(format!("travsr-acl-577-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("plugin.exe");
        std::fs::write(&binary, b"MZ").unwrap();

        // Replicate restrict_to_owner_windows (travsr-store/src/registry.rs).
        let user = std::env::var("USERNAME").expect("USERNAME set on Windows");
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let account = if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        };
        let status = std::process::Command::new("icacls")
            .args([
                dir.to_str().unwrap(),
                "/inheritance:r",
                "/grant:r",
                &format!("{account}:(OI)(CI)F"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls restriction must apply");

        let sid = derive_appcontainer_sid("travsr-test-577-rx").expect("derive sid");

        // Post-restriction: the AppContainer has no ACE on the dir.
        {
            let h = read_dacl(&dir);
            assert!(
                !unsafe {
                    dacl_has_inheritable_allow_ace(
                        h.dacl,
                        sid.as_psid(),
                        ACCESS_GENERIC_READ_EXECUTE,
                    )
                },
                "restricted dir must carry no AppContainer ACE"
            );
        }

        // The spawn-path grant restores read+execute...
        grant_path_access(&dir, sid.as_psid(), ACCESS_GENERIC_READ_EXECUTE)
            .expect("grant read+execute");
        {
            let h = read_dacl(&dir);
            assert!(
                unsafe {
                    dacl_has_inheritable_allow_ace(
                        h.dacl,
                        sid.as_psid(),
                        ACCESS_GENERIC_READ_EXECUTE,
                    )
                },
                "grant must be present on the directory"
            );
        }

        // ...and inheritance propagation reaches the pre-existing binary,
        // which is what the AppContainer child must be able to map. Windows
        // maps GENERIC_* to file-specific rights when an inheritable ACE
        // lands on a file, so assert the two specific bits image loading
        // needs: FILE_READ_DATA (0x1) and FILE_EXECUTE (0x20).
        {
            const FILE_READ_DATA_AND_EXECUTE: u32 = 0x1 | 0x20;
            let h = read_dacl(&binary);
            assert!(
                unsafe {
                    dacl_has_allow_ace_any_flags(h.dacl, sid.as_psid(), FILE_READ_DATA_AND_EXECUTE)
                },
                "grant must propagate read+execute to the existing plugin binary"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A different SID or a wider mask must NOT be treated as already
    /// granted — only an exact-or-superset mask for the same SID skips.
    #[test]
    fn grant_skip_requires_matching_sid_and_mask() {
        let dir = std::env::temp_dir().join(format!("travsr-acl-505b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let sid_a = derive_appcontainer_sid("travsr-test-505-sid-a").expect("derive sid a");
        let sid_b = derive_appcontainer_sid("travsr-test-505-sid-b").expect("derive sid b");

        grant_path_access(&dir, sid_a.as_psid(), ACCESS_GENERIC_READ).expect("grant read");

        let h = read_dacl(&dir);
        assert!(
            !unsafe {
                dacl_has_inheritable_allow_ace(h.dacl, sid_b.as_psid(), ACCESS_GENERIC_READ)
            },
            "another profile's SID must not match"
        );
        assert!(
            !unsafe { dacl_has_inheritable_allow_ace(h.dacl, sid_a.as_psid(), ACCESS_GENERIC_ALL) },
            "a read grant must not satisfy an ALL request"
        );
        drop(h);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── #504: Job Object CPU cap ────────────────────────────────────────────

    /// #504 regression: the per-job CPU cap must scale with core count so a
    /// multithreaded Phase B pass cannot exhaust it in a fraction of the
    /// intended 300 s wall-clock window (the old flat cap died in ~40 s of
    /// wall time on 8 cores).
    #[test]
    fn job_cpu_cap_scales_with_core_count() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as i64)
            .unwrap_or(1);
        assert_eq!(job_cpu_time_limit(), 300 * cores * 10_000_000);
        assert!(job_cpu_time_limit() >= 300 * 10_000_000);
    }

    /// The scaled cap (and the JOB_TIME flag) must actually land on the
    /// created Job Object, read back via QueryInformationJobObject.
    #[test]
    fn job_object_carries_scaled_cpu_cap() {
        use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;

        let job = create_job_with_limits().expect("create job object");
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                job.as_handle(),
                JobObjectExtendedLimitInformation,
                &mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *mut _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "QueryInformationJobObject failed");
        assert_eq!(
            info.BasicLimitInformation.PerJobUserTimeLimit,
            job_cpu_time_limit(),
            "job must carry the core-scaled CPU cap"
        );
        assert_ne!(
            info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_TIME,
            0,
            "JOB_TIME limit flag must stay set (runaway-spin backstop)"
        );
    }

    /// The block must never carry case-insensitive duplicate names —
    /// CreateProcessW env lookups treat names case-insensitively.
    #[test]
    fn env_block_has_no_duplicate_names() {
        let entries = decode_env_block(&build_env_block(
            &std::env::temp_dir(),
            &[("path".to_string(), "C:\\override".to_string())],
        ));
        let mut names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.split_once('=').map(|(k, _)| k.to_ascii_lowercase()))
            .collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            total,
            names.len(),
            "duplicate env names in block: {entries:?}"
        );
    }
}
