//! Container-limit-aware sizing signals (#736 item 8).
//!
//! Every sizing decision in this crate used to read **host** topology and
//! **host** free RAM: `p_core_count()` walks sysfs/sysctl/the Windows API for
//! physical P-cores, `available_memory_mb()` reads `/proc/meminfo`'s
//! `MemAvailable`, and `auto_capacity_fraction()` asked
//! `available_parallelism()` for the core count. Inside a cgroup-limited
//! container (Docker `--cpus 2 -m 4g`, Kubernetes requests/limits) those
//! reads all report the *machine*, not the *cage*: a 64-core node with 256 GB
//! free tells a 2-CPU/4 GB pod to spawn 8 embed workers and size its RAM
//! guard against memory it will be OOM-killed for touching.
//!
//! This module is the single place that knows how to read the cage:
//!
//! * [`effective_cpu_count`] — `available_parallelism()` (already
//!   cgroup-quota-aware on Linux since Rust 1.64) *and* a direct read of the
//!   cgroup v2 `cpu.max` / v1 `cpu.cfs_quota_us` files, returning the minimum
//!   of every signal that exists. The direct read matters because callers
//!   like `p_core_count()` deliberately bypass `available_parallelism()` to
//!   count physical P-cores — this function is what caps them back down.
//! * [`effective_available_memory_mb`] — clamps a host "available RAM"
//!   figure to the cgroup's own headroom (`memory.max - memory.current`,
//!   v1: `limit_in_bytes - usage_in_bytes`) when a real limit is set.
//!
//! Linux is the only OS where container limits are readable from inside the
//! container, and the only OS travsr ships containerized on; Windows and
//! macOS have no `/sys/fs/cgroup`, so there both functions fall through to
//! the host signal unchanged (`#[cfg(target_os = "linux")]` gates every file
//! read). The string parsers are pure and un-gated so their unit tests run
//! on every OS, per the repo's "test the logic, not the kernel" convention.

/// Effective CPU count: host parallelism capped by any cgroup CPU quota.
///
/// Base signal is [`std::thread::available_parallelism`], which on Linux
/// already accounts for cgroup CPU quotas (Rust >= 1.64) and CPU affinity
/// masks. We still read the quota files ourselves and take the minimum of
/// all available signals, for two reasons (#736):
///
/// 1. Belt and braces: `available_parallelism()` reads the quota at libstd's
///    discretion and its container awareness has version- and
///    mount-layout-dependent gaps (e.g. v1 hierarchies mounted in
///    non-standard places). The direct read costs three tiny file reads at
///    sizing time — not on any hot path.
/// 2. Callers such as `embed_catalog::p_core_count()` *deliberately* bypass
///    `available_parallelism()` to count physical P-cores from
///    sysfs/sysctl/`GetLogicalProcessorInformationEx`. Those reads see host
///    topology straight through a container boundary, and this function is
///    the cap they apply afterwards.
///
/// Never returns 0; a fractional quota (e.g. `--cpus 0.5`) rounds up to 1
/// so sizing always has at least one worker to hand out.
pub fn effective_cpu_count() -> usize {
    let base = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    #[cfg(target_os = "linux")]
    {
        let mut n = base;
        if let Some(q) = cgroup_v2_cpu_quota() {
            n = n.min(q);
        }
        if let Some(q) = cgroup_v1_cpu_quota() {
            n = n.min(q);
        }
        return n.max(1);
    }

    #[cfg(not(target_os = "linux"))]
    base.max(1)
}

/// Clamp a host "available RAM" figure (MiB) to the cgroup's own headroom.
///
/// `host_available_mb` is whatever the caller derived from host-wide signals
/// (`/proc/meminfo` `MemAvailable`, `GlobalMemoryStatusEx`, `vm_stat`). In a
/// memory-limited container those describe the node, not the pod: sizing
/// against them provisions workers the OOM killer will reap (#736). When a
/// real cgroup limit exists (not `"max"`, not the v1 page-rounded
/// `i64::MAX` sentinel), the container's true headroom is
/// `limit - current usage`, and the safe answer is the *minimum* of the two
/// views — the host figure still matters because a container can be limited
/// above what the loaded node actually has free.
///
/// The convention `0 == "host signal unavailable"` (which makes
/// `derive_num_workers_inner` skip its RAM guard) is preserved on the way
/// through — except that when the host read failed but a cgroup limit *is*
/// readable, the cgroup headroom is returned instead of 0, because a real
/// limit is exactly the case where skipping the guard over-provisions.
///
/// Non-Linux: returned unchanged — no cgroups to read.
pub fn effective_available_memory_mb(host_available_mb: u64) -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Some(cgroup_headroom_mb) = cgroup_memory_headroom_mb() {
            if host_available_mb == 0 {
                return cgroup_headroom_mb;
            }
            return host_available_mb.min(cgroup_headroom_mb);
        }
    }

    host_available_mb
}

// ── Pure parsers ─────────────────────────────────────────────────────────────
//
// Un-gated so the tests below compile and run on every OS; only the
// file-reading callers are Linux-only, hence the dead-code allowance
// elsewhere.

/// cgroup v1 `memory.limit_in_bytes` reports "no limit" as `i64::MAX`
/// rounded **down to the page size** (`0x7FFF_FFFF_FFFF_F000` on 4 KiB
/// pages). The page size varies (up to 64 KiB on arm64), so rather than
/// matching one exact sentinel, anything within a page-size margin of
/// `i64::MAX` is treated as unlimited. No real container limit lives within
/// 64 KiB of 8 EiB.
const V1_UNLIMITED_FLOOR: u64 = i64::MAX as u64 - 65_536;

/// Parse cgroup v2 `cpu.max` content into a whole-CPU quota.
///
/// Format is `"$QUOTA $PERIOD"` where `$QUOTA` is either `max` (no limit →
/// `None`) or a microsecond budget per `$PERIOD` microseconds. A container
/// granted 1.5 CPUs (`"150000 100000"`) can genuinely keep 2 workers busy
/// part-time, and rounding *down* to 1 would strand the granted half-core,
/// so the quotient rounds up: `ceil(quota / period)`.
///
/// A lone `"$QUOTA"` with no period falls back to the kernel default period
/// of 100 000 µs (the kernel always writes both fields, but the default
/// keeps the parser total). Zero or malformed fields → `None` (no signal,
/// never a 0-CPU claim).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpu_max(content: &str) -> Option<usize> {
    let mut fields = content.split_whitespace();
    let quota = fields.next()?;
    if quota == "max" {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    let period: u64 = match fields.next() {
        Some(p) => p.parse().ok()?,
        None => 100_000,
    };
    if quota == 0 || period == 0 {
        return None;
    }
    Some(quota.div_ceil(period) as usize)
}

/// Parse cgroup v1 `cpu.cfs_quota_us` + `cpu.cfs_period_us` contents into a
/// whole-CPU quota. Quota `-1` (the v1 "no limit" spelling) → `None`; the
/// division rounds up for the same fractional-quota reason as
/// [`parse_cpu_max`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cfs_quota_period(quota: &str, period: &str) -> Option<usize> {
    let quota: i64 = quota.trim().parse().ok()?;
    if quota <= 0 {
        return None;
    }
    let period: u64 = period.trim().parse().ok()?;
    if period == 0 {
        return None;
    }
    Some((quota as u64).div_ceil(period) as usize)
}

/// Parse a cgroup memory *limit* file (`memory.max` v2,
/// `memory.limit_in_bytes` v1) into bytes. `None` when no real limit is set:
/// the v2 `"max"` spelling, the v1 page-rounded `i64::MAX` sentinel, or
/// unparseable content.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_memory_limit_bytes(content: &str) -> Option<u64> {
    let t = content.trim();
    if t == "max" {
        return None;
    }
    let bytes: u64 = t.parse().ok()?;
    if bytes >= V1_UNLIMITED_FLOOR {
        return None;
    }
    Some(bytes)
}

/// Parse a cgroup memory *usage* file (`memory.current` v2,
/// `memory.usage_in_bytes` v1) into bytes. Plain integer, `None` on garbage.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_memory_current_bytes(content: &str) -> Option<u64> {
    content.trim().parse().ok()
}

/// Extract this process's cgroup v2 path from `/proc/self/cgroup` content.
///
/// The v2 (unified) hierarchy is the line `0::<path>`; v1 controllers use
/// nonzero hierarchy IDs and named controller lists, so matching the literal
/// `0::` prefix is exact, not a heuristic. Returns the path *relative to the
/// cgroupfs mount* (usually `/sys/fs/cgroup`), which may be `/` itself.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_v2_cgroup_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

// ── Linux file plumbing ──────────────────────────────────────────────────────

/// Read a v2 interface file for this process's own cgroup: resolve the path
/// from `/proc/self/cgroup` first (correct under nested cgroups, e.g. a
/// systemd slice inside a container), falling back to the `/sys/fs/cgroup`
/// root when that resolution fails (some minimal container images mount
/// cgroupfs but hide `/proc/self/cgroup`, or namespace the path away).
#[cfg(target_os = "linux")]
fn read_own_cgroup_v2_file(name: &str) -> Option<String> {
    if let Ok(proc_cg) = std::fs::read_to_string("/proc/self/cgroup") {
        if let Some(rel) = parse_v2_cgroup_path(&proc_cg) {
            let path = format!("/sys/fs/cgroup{}/{name}", rel.trim_end_matches('/'));
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Some(content);
            }
        }
    }
    std::fs::read_to_string(format!("/sys/fs/cgroup/{name}")).ok()
}

#[cfg(target_os = "linux")]
fn cgroup_v2_cpu_quota() -> Option<usize> {
    parse_cpu_max(&read_own_cgroup_v2_file("cpu.max")?)
}

/// v1 quota via the conventional controller mount. Docker and Kubernetes
/// place the *container's* limit at the mount root as seen from inside the
/// container's cgroup namespace, so the fixed path is the right one for the
/// deployment this exists for; hosts without a v1 cpu controller simply
/// return `None`.
#[cfg(target_os = "linux")]
fn cgroup_v1_cpu_quota() -> Option<usize> {
    let quota = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").ok()?;
    let period = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us").ok()?;
    parse_cfs_quota_period(&quota, &period)
}

/// The cgroup's own memory headroom in MiB: `limit - current`, saturating,
/// from whichever hierarchy (v2 first, then v1) exposes a real limit.
/// `None` when no hierarchy sets one — the common bare-metal/dev case,
/// where the host figure stands alone.
///
/// An unreadable usage file next to a readable limit counts as usage 0:
/// headroom then equals the full limit, which still caps the host figure
/// (the fix this module exists for) without inventing usage we cannot see.
#[cfg(target_os = "linux")]
fn cgroup_memory_headroom_mb() -> Option<u64> {
    const MIB: u64 = 1024 * 1024;

    if let Some(limit) =
        read_own_cgroup_v2_file("memory.max").and_then(|c| parse_memory_limit_bytes(&c))
    {
        let current = read_own_cgroup_v2_file("memory.current")
            .and_then(|c| parse_memory_current_bytes(&c))
            .unwrap_or(0);
        return Some(limit.saturating_sub(current) / MIB);
    }

    if let Some(limit) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")
        .ok()
        .and_then(|c| parse_memory_limit_bytes(&c))
    {
        let current = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes")
            .ok()
            .and_then(|c| parse_memory_current_bytes(&c))
            .unwrap_or(0);
        return Some(limit.saturating_sub(current) / MIB);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── cpu.max (cgroup v2) ──────────────────────────────────────────────

    #[test]
    fn cpu_max_unlimited_is_none() {
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("max"), None);
        assert_eq!(parse_cpu_max("max 100000\n"), None);
    }

    #[test]
    fn cpu_max_whole_multiples() {
        assert_eq!(parse_cpu_max("200000 100000"), Some(2));
        assert_eq!(parse_cpu_max("100000 100000\n"), Some(1));
        assert_eq!(parse_cpu_max("800000 100000"), Some(8));
    }

    #[test]
    fn cpu_max_fractional_quota_rounds_up() {
        // 1.5 CPUs can keep 2 workers part-time busy; never round to 1.
        assert_eq!(parse_cpu_max("150000 100000"), Some(2));
        // 0.5 CPUs still gets 1 worker, never 0.
        assert_eq!(parse_cpu_max("50000 100000"), Some(1));
    }

    #[test]
    fn cpu_max_missing_period_uses_kernel_default() {
        // The kernel always writes both fields; the 100_000 µs default keeps
        // the parser total anyway.
        assert_eq!(parse_cpu_max("150000\n"), Some(2));
        assert_eq!(parse_cpu_max("100000"), Some(1));
    }

    #[test]
    fn cpu_max_garbage_is_none() {
        assert_eq!(parse_cpu_max(""), None);
        assert_eq!(parse_cpu_max("banana 100000"), None);
        assert_eq!(parse_cpu_max("100000 banana"), None);
        assert_eq!(parse_cpu_max("0 100000"), None);
        assert_eq!(parse_cpu_max("100000 0"), None);
    }

    // ── cfs_quota_us / cfs_period_us (cgroup v1) ─────────────────────────

    #[test]
    fn cfs_unlimited_minus_one_is_none() {
        assert_eq!(parse_cfs_quota_period("-1\n", "100000\n"), None);
    }

    #[test]
    fn cfs_quota_rounds_up() {
        assert_eq!(parse_cfs_quota_period("200000", "100000"), Some(2));
        assert_eq!(parse_cfs_quota_period("150000\n", "100000\n"), Some(2));
        assert_eq!(parse_cfs_quota_period("50000", "100000"), Some(1));
    }

    #[test]
    fn cfs_garbage_or_zero_is_none() {
        assert_eq!(parse_cfs_quota_period("", "100000"), None);
        assert_eq!(parse_cfs_quota_period("100000", ""), None);
        assert_eq!(parse_cfs_quota_period("0", "100000"), None);
        assert_eq!(parse_cfs_quota_period("100000", "0"), None);
        assert_eq!(parse_cfs_quota_period("banana", "100000"), None);
    }

    // ── memory limit / usage ─────────────────────────────────────────────

    #[test]
    fn memory_limit_v2_max_is_none() {
        assert_eq!(parse_memory_limit_bytes("max\n"), None);
        assert_eq!(parse_memory_limit_bytes("max"), None);
    }

    #[test]
    fn memory_limit_v1_sentinel_is_none() {
        // Exact 4 KiB-page-rounded i64::MAX, as Docker-on-v1 reports it.
        assert_eq!(parse_memory_limit_bytes("9223372036854771712\n"), None);
        // Unrounded i64::MAX (some kernels), and 64 KiB-page rounding.
        assert_eq!(parse_memory_limit_bytes("9223372036854775807"), None);
        assert_eq!(
            parse_memory_limit_bytes(&(i64::MAX as u64 - 65_535).to_string()),
            None
        );
    }

    #[test]
    fn memory_limit_real_values_parse() {
        // 4 GiB, the shape a `docker run -m 4g` produces.
        assert_eq!(
            parse_memory_limit_bytes("4294967296\n"),
            Some(4_294_967_296)
        );
        assert_eq!(parse_memory_limit_bytes("536870912"), Some(536_870_912));
    }

    #[test]
    fn memory_limit_garbage_is_none() {
        assert_eq!(parse_memory_limit_bytes(""), None);
        assert_eq!(parse_memory_limit_bytes("banana"), None);
        assert_eq!(parse_memory_limit_bytes("-1"), None);
    }

    #[test]
    fn memory_current_parses_plain_integer() {
        assert_eq!(parse_memory_current_bytes("123456789\n"), Some(123_456_789));
        assert_eq!(parse_memory_current_bytes("0"), Some(0));
        assert_eq!(parse_memory_current_bytes("banana"), None);
    }

    // ── /proc/self/cgroup v2 path extraction ─────────────────────────────

    #[test]
    fn v2_path_from_unified_only() {
        // The whole-file shape on a pure-v2 host.
        assert_eq!(
            parse_v2_cgroup_path("0::/user.slice/user-1000.slice/session-2.scope\n"),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
    }

    #[test]
    fn v2_path_skips_v1_controller_lines() {
        // Hybrid layout: v1 controllers first, unified line last.
        let content = "12:cpu,cpuacct:/docker/abc123\n\
                       3:memory:/docker/abc123\n\
                       0::/docker/abc123\n";
        assert_eq!(parse_v2_cgroup_path(content), Some("/docker/abc123"));
    }

    #[test]
    fn v2_path_root_and_absent() {
        assert_eq!(parse_v2_cgroup_path("0::/\n"), Some("/"));
        assert_eq!(parse_v2_cgroup_path("3:memory:/docker/abc\n"), None);
        assert_eq!(parse_v2_cgroup_path(""), None);
    }

    // ── public functions: portable invariants ────────────────────────────

    #[test]
    fn effective_cpu_count_is_at_least_one_and_at_most_parallelism() {
        let n = effective_cpu_count();
        assert!(n >= 1);
        // Outside a container the quota files are absent (or "max"), so this
        // must equal the base signal; inside one it can only be lower.
        let base = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        assert!(n <= base, "effective {n} exceeds base parallelism {base}");
    }

    #[test]
    fn effective_memory_never_exceeds_host_figure_when_host_known() {
        assert!(effective_available_memory_mb(16_384) <= 16_384);
        assert!(effective_available_memory_mb(1) <= 1);
    }
}
