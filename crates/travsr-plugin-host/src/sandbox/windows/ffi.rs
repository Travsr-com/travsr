//! Unsafe Win32 FFI wrappers for AppContainer + Job Object sandbox (RFC-014).
//! All `unsafe` blocks in travsr-plugin-host are confined to this file.
#![allow(unsafe_code)]
#![allow(dead_code)] // full CreateProcessW integration pending P5-S3
#![allow(clippy::io_other_error)]

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, FreeSid, WinCapabilityInternetClientSid, DACL_SECURITY_INFORMATION,
    PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    IO_COUNTERS, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
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

// PSID (*mut c_void) is not Send/Sync by default; we uphold the invariant that
// this type is the sole owner and is not shared across threads without join.
unsafe impl Send for AppContainerSid {}
unsafe impl Sync for AppContainerSid {}

/// Owned HANDLE that calls `CloseHandle` on drop.
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

/// `SECURITY_CAPABILITIES` wrapper that implements `Copy + Send + Sync + 'static`.
///
/// # Safety
/// The `AppContainerSid` field must point to a live SID allocation for the
/// duration of any `CreateProcessW` call. Callers must keep the `AppContainerSid`
/// RAII guard alive until after the process is created.
#[derive(Copy, Clone)]
pub(super) struct SendSyncSecurityCapabilities(pub(super) SECURITY_CAPABILITIES);

unsafe impl Send for SendSyncSecurityCapabilities {}
unsafe impl Sync for SendSyncSecurityCapabilities {}

// ── Helper: convert a &str / &Path to null-terminated UTF-16 ────────────────

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
/// Fail-closed: returns `Err` on any other failure.
pub(super) fn ensure_appcontainer_profile(profile_name: &str) -> io::Result<()> {
    let name_wide = to_wide(profile_name);
    let desc_wide = to_wide("Travsr plugin sandbox");
    let mut out_sid: PSID = std::ptr::null_mut();
    let hr = unsafe {
        CreateAppContainerProfile(
            name_wide.as_ptr(),
            name_wide.as_ptr(), // display name = profile name
            desc_wide.as_ptr(),
            std::ptr::null(),
            0,
            &mut out_sid,
        )
    };
    // Free the SID returned by CreateAppContainerProfile.
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
/// Uses `GetNamedSecurityInfoW` → `SetEntriesInAclW` → `SetNamedSecurityInfoW`
/// so existing DACL entries are preserved.
pub(super) fn grant_path_access(path: &Path, sid: PSID, access_mask: u32) -> io::Result<()> {
    let path_wide = path_to_wide(path);

    // 1. Fetch the existing DACL.
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

    // 2. Build a merged DACL with our new ACE prepended.
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

    // 3. Apply the merged DACL.
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

/// Builds `SECURITY_CAPABILITIES` for Standard (no capabilities) or Elevated
/// (WinCapabilityInternetClientSid) policy.
///
/// Returns `(cap_sid_buf, cap_attr, security_caps)`. All three must be kept alive
/// until after `CreateProcessW` — their lifetimes are tied by raw pointer references
/// inside `security_caps`.
pub(super) fn build_security_capabilities(
    container_sid: PSID,
    elevated: bool,
) -> io::Result<([u8; SECURITY_MAX_SID_SIZE], Option<SID_AND_ATTRIBUTES>, SECURITY_CAPABILITIES)>
{
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
            // Pointer into cap_attr — caller must keep cap_attr alive.
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

/// Assigns the process identified by `process_handle` to `job`.
pub(super) fn assign_to_job(job: HANDLE, process_handle: HANDLE) -> io::Result<()> {
    let ok = unsafe { AssignProcessToJobObject(job, process_handle) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Finds the first thread of `pid` via `CreateToolhelp32Snapshot` and calls
/// `ResumeThread` on it. Used after `CREATE_SUSPENDED` to start execution.
pub(super) fn resume_first_thread(pid: u32) -> io::Result<()> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    struct SnapGuard(HANDLE);
    impl Drop for SnapGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    let _snap = SnapGuard(snapshot);

    let mut te: THREADENTRY32 = unsafe { std::mem::zeroed() };
    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;

    if unsafe { Thread32First(snapshot, &mut te) } == 0 {
        return Err(io::Error::last_os_error());
    }

    loop {
        if te.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            struct ThreadGuard(HANDLE);
            impl Drop for ThreadGuard {
                fn drop(&mut self) {
                    unsafe { CloseHandle(self.0) };
                }
            }
            let _tg = ThreadGuard(thread);
            let prev = unsafe { ResumeThread(thread) };
            if prev == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if unsafe { Thread32Next(snapshot, &mut te) } == 0 {
            break;
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no thread found for pid {pid}"),
    ))
}
