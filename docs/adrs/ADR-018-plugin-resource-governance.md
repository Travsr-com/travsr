# ADR-018: Plugin Resource Governance

**Date:** 2026-06-04
**Status:** Proposed
**Phase:** Cross-cutting (RFC-013 Phase 3 — depends on ADR-017 sandbox)
**Author:** DevOps Engineer (policy owner) + Principal Architect (invariant owner)
**Extends:** ADR-017 (unified plugin sandbox & trust — Rule 1 names "CPU / RAM / wall-clock caps enforced" but does not specify the mechanism or the limits; this ADR specifies them)
**Related:** RFC-013 (runtime language pluggability — D7), RFC-011 (two-transport plugin architecture — the Sidecar this governs), CLAUDE.md §Non-Negotiable Principles (#2 always-fresh, #5 OCI free tier, #6 ARM64)

---

## Context

RFC-013 moves non-builtin Tree-sitter grammars **out of the host binary and into
long-lived `travsr-lang-*` sidecar processes** (Direction A). These processes run
third-party code we cannot audit — including C grammars whose memory behaviour is
opaque to us — and they persist across many file parses rather than dying after one.

ADR-017 Rule 1 declares that every Sidecar spawn runs under
`SandboxPolicy::Standard` with "CPU / RAM / wall-clock caps enforced," but it does
**not** define:

- *what* the caps are (numbers),
- *which OS mechanism* enforces them,
- what happens to a process that **leaks slowly** without ever crashing or hanging,
- how a hang during the verification probe (RFC-013 D4) is bounded.

The current code has neither: `travsr-plugin-host/src/supervisor.rs` is
crash-count only (`record_crash`, `is_disabled`), and
`travsr-plugin-host/src/sandbox/policy.rs` (`SandboxPolicy::{Standard, Elevated}`)
exposes no resource knobs. A leak, hang, or runaway-CPU plugin today would degrade
or take down the daemon.

The asymmetry that makes this tractable: in Direction A a plugin leak lives in the
**plugin's** address space, not the daemon's, so the OS reclaims it on respawn.
Governance therefore reduces to *bounding and recycling* the child, not preventing
leaks we cannot fix in third-party code.

**Hard platform reality:** macOS `sandbox-exec` (Seatbelt) **cannot cap process
memory.** Linux can (`RLIMIT_AS` / cgroup `memory.max`). Any resource policy that
assumes a uniform hard cap is wrong on macOS — this ADR makes the macOS path
explicit rather than pretending parity.

This ADR decides:

1. The resource limits applied to every plugin process and to the probe.
2. The enforcement mechanism per platform, including the macOS watchdog.
3. The recycling and idle-spin-down policy that bounds slow leaks.
4. What verdict a resource breach produces (the SOFT/transient class of RFC-013 D4).

---

## Decision

### Rule 1 — Resource limits are a property of `SandboxPolicy`, defined once

`SandboxPolicy::Standard` gains a normative resource block, applied at **every**
Sidecar spawn and at the verification probe (RFC-013 D3/D4):

```
ResourceLimits (defaults — tunable via travsr.toml, never upward past the daemon's own ceiling):
  memory_max        = 1024 MiB    per plugin process (RSS / address space)
  parse_deadline    = 30 s        wall-clock, per ParseRequest
  probe_deadline    = 10 s        wall-clock, per verification attempt (RFC-013 D4)
  cpu_quota         = 1 core-equivalent (best-effort; not fatal, throttle not kill)
  output_max        = 64 MiB      per ParseResponse on the wire (forged-giant-output guard)
  recycle_after     = 2000 files  OR  memory_high (768 MiB RSS), whichever first
  idle_ttl          = 120 s       no in-flight files → spin down
```

Numbers are defaults chosen against the OCI A1 free-tier envelope (4 OCPU / 24 GB
*total* across all instances — CLAUDE.md): a single plugin must never be able to
claim a daemon-fatal share. `memory_high` (recycle trigger) sits below `memory_max`
(kill trigger) so a leaking-but-productive plugin is drained gracefully before it
is force-killed.

### Rule 2 — Enforcement mechanism per platform

| Platform | Memory cap | CPU | Wall-clock | Mechanism notes |
|---|---|---|---|---|
| **Linux** (incl. OCI A1 / aarch64) | cgroup v2 `memory.max` (+ `memory.high` for the recycle trigger); `RLIMIT_AS` as a belt-and-braces fallback | cgroup `cpu.max` (throttle, not kill) | host-side timer in the supervisor (`SIGKILL` on breach) | enforced via the same bubblewrap container ADR-017 already spawns; no new privileged path |
| **macOS** | **watchdog only** — `sandbox-exec` cannot cap memory. Supervisor samples child RSS (`proc_pid_rusage` / `task_info`) on a fixed interval (1 s) and `SIGKILL`s on `memory_max` breach | best-effort `nice`; no hard quota | host-side timer in the supervisor | **explicit fidelity gap:** enforcement is *sampled*, so a sub-interval spike can transiently exceed `memory_max`. Accepted (Consequences). |
| **Windows** | Job Object `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (memory + kill-on-job-close) | Job Object CPU rate | host-side timer | tracked but lower priority — Windows plugin sidecars are post-MVP |

The host-side wall-clock timer and the RSS watchdog live in the **supervisor**, not
the sandbox, so they work identically whether or not the OS sandbox can self-enforce.
Fail-closed (ADR-017 Rule 2) is unchanged: if neither the sandbox nor the watchdog
can be established, the plugin is **disabled**, not run uncapped.

### Rule 3 — Recycling bounds slow leaks

A plugin process is **drained and respawned** when it crosses `recycle_after`
(file count) or `memory_high` (RSS), whichever comes first — the PHP-FPM /
Gunicorn `max_requests` pattern. Recycling:

- waits for in-flight `ParseRequest`s to complete (graceful), then `SIGTERM` →
  `SIGKILL` after a grace period;
- **reuses the cached subscription verdict** (ADR-019) — respawn does **not**
  re-run the probe, because the binary hash is unchanged;
- is invisible to the index run (the next `ParseRequest` transparently spawns a
  fresh child).

This makes *any* leak — including in third-party C grammar code we cannot patch —
self-healing and bounded, without requiring us to find or fix it.

### Rule 4 — Idle spin-down

A plugin with no in-flight files for `idle_ttl` is terminated to release memory.
The next request for that language respawns it on demand. Because the verdict is
cached (ADR-019), respawn is a process start, not a re-verification — cheap.

### Rule 5 — A resource breach is a SOFT (transient) verdict, never HARD

Per RFC-013 D4, a kill for `ResourceExceeded` or `Timeout` is **environmental**,
not a binary-intrinsic fault: the binary may be correct and the machine merely
loaded. Therefore:

- a resource kill **does not** write a HARD `Rejected` verdict and **does not**
  poison the binary's hash-keyed reputation;
- it surfaces as `Unverified{ResourceExceeded|Timeout}` and is **re-probed** on the
  next trigger (RFC-013 D4 reproducibility rule: only a failure that reproduces
  across retries becomes sticky);
- the affected file is marked failed for this run (ADR-017 Tests: "a plugin
  exceeding the wall-clock cap is killed and the file marked failed without hanging
  the index run") — the index run continues.

---

## Threat model — rows touched / added (T-table)

> Numbering continues the global table; ADR-017 occupies T11–T13.

| Row | Asset | Threat | Likelihood | Impact | Mitigation | Δ |
|---|---|---|---|---|---|---|
| **T14 (new)** | Daemon availability | A plugin (or its C grammar) leaks/runs away and starves or OOM-kills the daemon | Med | High | Per-process `memory_max` kill + `memory_high` recycle (Rules 1–3); daemon insulated by process boundary (Direction A) | new |
| **T15 (new)** | Daemon availability | A plugin hangs forever on a crafted input, stalling the index run | Med | Med | `parse_deadline` / `probe_deadline` host-side timer → `SIGKILL` (Rules 1–2) | new |
| **T16 (new)** | Daemon memory / wire | A plugin emits an enormous `ParseResponse` to exhaust daemon RAM | Low | Med | `output_max` cap on the decoded response; over-cap response rejected, file failed | new |
| **T13 (touched)** | Graph integrity / availability | In-process C-grammar memory bug (first-party builtins, Rule 4 ADR-017) | Low | High | Unchanged for the 4 builtins; **reduced overall** because the other 8 grammars move to bounded sidecars | improved |

---

## Tests QA must add

- **Memory kill:** a plugin that allocates past `memory_max` is `SIGKILL`ed; the
  daemon RSS is unaffected; the file is marked failed; the index run completes.
- **Recycle:** a plugin crossing `recycle_after` files (or `memory_high` RSS) is
  drained and respawned with the **same cached verdict** (no re-probe); no parse is
  lost across the boundary.
- **Hang/timeout:** a plugin that blocks past `parse_deadline` is killed; verdict is
  `Unverified{Timeout}` (SOFT), not `Rejected`.
- **Giant output:** a plugin returning a response over `output_max` is rejected
  without the daemon allocating the full payload.
- **Idle spin-down:** a plugin idle past `idle_ttl` is terminated; the next request
  respawns it without re-probing.
- **macOS watchdog fidelity:** under the sampled watchdog, a process exceeding
  `memory_max` is killed within ≤ 2 sample intervals; document the transient
  overshoot window.
- **Fail-closed (inherited):** with no enforceable cap available, the plugin is
  disabled, not run uncapped (ADR-017 Rule 2).

---

## Consequences

**Positive:**
- Bounded, self-healing memory for third-party plugins without needing to audit or
  fix their leaks. The daemon is structurally insulated (process boundary) — the
  decisive advantage of Direction A over WASM-in-host (RFC-013 Option 2), where a
  leak would grow the daemon's own heap.
- Resource caps become a reviewed-once property of `SandboxPolicy`, inheriting the
  ADR-017 "define once, apply at every spawn" posture.
- Migrating 8 grammars from in-process to bounded sidecars **reduces** the T13
  in-daemon-crash surface overall.

**Negative / accepted:**
- **macOS memory enforcement is sampled, not hard-capped** — a sub-interval spike
  can transiently exceed `memory_max` before the watchdog fires. Accepted: the
  process boundary still protects the daemon, and recycling bounds steady-state.
  Tightening would require a macOS helper (e.g. a launchd-managed control) —
  deferred unless real overshoot is observed.
- Recycling adds respawn overhead (a fresh process + handshake) on the recycle
  boundary. Bounded by `recycle_after` being large (2000 files) relative to spawn
  cost; tunable.
- Per-process caps mean a single huge file in a pathological language could exceed
  `memory_max` legitimately and fail; the limit is tunable per language via
  `travsr.toml` (still never upward past the daemon ceiling).

---

## Escalations

- **A default that forces off the OCI free tier** (e.g. a language whose legitimate
  parse needs > the per-instance share) → CTO + DevOps for an envelope decision; do
  not silently raise `memory_max` past the daemon's own ceiling.
- **macOS overshoot causing a real incident** → revisit for a launchd/helper-based
  hard cap; Principal Architect + DevOps.

---

## Verdict

**APPROVED_WITH_MITIGATIONS** — the resource block (Rule 1) and the per-platform
mechanism (Rule 2), including the macOS watchdog, must land with RFC-013 Phase 3
before any non-builtin grammar is migrated to a sidecar (RFC-013 Phase 5). The
sampled-macOS fidelity gap is accepted and documented.

---

## References

- RFC-013 — Open Language Identity & Runtime Language Pluggability (D7; this ADR is its resource-governance dependency)
- ADR-017 — Unified Plugin Sandbox & Trust Model (Rule 1 names the caps; this ADR specifies them)
- ADR-019 — Language Subscription & Verdict Cache (resource kills produce SOFT verdicts; recycling reuses cached verdicts)
- RFC-011 — Two-Transport Language Plugin Architecture (the Sidecar transport governed here)
- CLAUDE.md — Non-Negotiable Principles (#2 always-fresh, #5 OCI free tier, #6 ARM64)
