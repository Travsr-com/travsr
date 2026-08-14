# RFC-025 - Sidecar Version Contract (Compatibility Floor + Staleness Advisory)

- **Status:** Draft - **Solution Architect design + Principal Architect ruling (§4).** Generalizes issue #701 from one sidecar to the whole external-binary family. Phased rollout (P1 unblocks #701).
- **Authors:** Principal Architect + Solution Architect (personas)
- **Affects:** `travsr-plugin-host` (NEW `SidecarSpec` trait, `installed_version`, floor enforcement in `resolve_backend` / `tool_available`, TTL latest-cache - the home crate), `travsr-cli` (`embed.rs`, `lang.rs`, `install.rs`, `status`, `embed status`, `lang list` health line), `travsr-daemon` (four new structured log events, transition-only de-dup), `crates/travsr-plugin-host/src/embed_catalog.toml` + `phase_b/catalog.rs` (`version_fallback` bumps), `bench/` and CI (two honesty tests).
- **Origin:** Issue #701 - `travsr embed reindex` failed with `reindex failed: applying CDC tombstones: database is locked: Error code 517: Cannot promote read transaction to write transaction` on a stale cached `travsr-embed` v1.0.0 (three releases / five weeks behind v1.3.0). `travsr embed init --reinstall` fixed it.
- **Related:** #376 (content-hash CDC invalidation in travsr-embed v1.2.0, which #701 required and the stale binary lacked); #410 M2 (vendored-hash supply-chain pinning in `resolve_install_tag`); #347/#348 (daemon log observability, whose stable event-key contract this RFC extends).

---

## 1. Problem statement (evidence-based)

Travsr spawns external, independently-versioned binaries. A user's cached copy of one silently drifted three releases behind the running platform, and the failure surfaced as a cryptic storage error with no mention of a version:

```
$ travsr embed reindex
reindex failed: applying CDC tombstones: database is locked:
Error code 517: Cannot promote read transaction to write transaction
```

Reproduced in the field (issue #701). The cached `~/.travsr/bin/travsr-embed` was **v1.0.0** (install-day), while the platform had moved to **v1.3.0**. The `applying CDC tombstones` text lives in the **separate `travsr-embed` repo** - it is the pre-#376 blanket tombstone-delete invalidation path, replaced by content-hash invalidation in travsr-embed **v1.2.0**. This repo's only `CDC tombstones` reference is the host-side store migration `crates/travsr-store/src/migrations/v17_node_tombstones.sql`, the host half of that same retired design. So a v1.0.0 sidecar ran a code path the current host no longer expects, and the mismatch manifested as a SQLite promotion error rather than "your sidecar is too old."

The install path never gave the user a chance to notice. The install gate is **existence-only** - `crates/travsr-cli/src/embed.rs:568`:

```rust
if dest.exists() && !reinstall {
    println!("  {} {} ready", pal.green("\u{25cf}"), backend.binary_name);
} else {
    // ...fetch_latest_version_for_repo(...) then download...
}
```

The version fetch (`crates/travsr-cli/src/install.rs:60` `fetch_latest_version_for_repo`, always live GitHub API) is reached **only** in the `else` branch. Once the file is present, `embed init` prints "ready" forever and never re-consults latest. The spawn path is equally blind - `crates/travsr-plugin-host/src/embed_catalog.rs:1379` `resolve_backend`, line 1388:

```rust
if !bin_path.exists() {
    return None;
}
```

Presence is the entire gate. `cmd_reindex` (`embed.rs:887`) → `run_reindex_locked` never touches a version. The same shape is duplicated in the Phase B language family: `crates/travsr-cli/src/lang.rs:362` (`Some(bin) if which(bin) && !reinstall => "already installed"`) and `lang.rs:418` (`wrapper_installed && !tool_available(entry.command)`).

**Install correctness is proxied by file PRESENCE, which is monotonic. It never flips false when a better release ships.** #701 is the first observed instance of a defect that is structural and family-wide, and a future third sidecar inherits it by copy-paste.

---

## 2. Root cause (architectural, not tuning)

There are two independently-versioned sidecar families today, both governed by presence alone, and both carrying - but not enforcing - the very version data that would have caught #701.

**Family 1 - EMBED (`travsr-embed`).** Existence-only install gate (`embed.rs:568`), existence-only resolve/spawn gate (`embed_catalog.rs:1388`). The only caller of `install_backend_with_progress` is `cmd_init` (`embed.rs:411`).

**Family 2 - PHASE B LANGUAGE (`travsr-lang-*` wrappers + `scip-*` tools).** Existence-only install gate (`lang.rs:362`, `lang.rs:418`). This family already has a *good* tag-resolution model worth reusing rather than reinventing: `lang.rs:566` `resolve_install_tag(pinned, version_fallback, override)`, precedence override → live latest → pin, with vendored-hash supply-chain pinning (#410 M2). `--version` probing already exists in indexer runners (`crates/travsr-indexer/src/runner.rs:554`, `ra_runner.rs:45`, `ra_runner.rs:212`).

Both families spawn a binary that could be arbitrarily old, and neither compares what is installed against what the host requires. Two shared primitives that would answer that question **already exist uniformly across both families**:

1. **The spawn handshake already carries both version axes, in both protocol crates.**
   - `crates/travsr-plugin-protocol/src/embed.rs:82-83`: `protocol_version: u32`, `plugin_version: String`.
   - `crates/travsr-plugin-protocol/src/types.rs:82-83`: the same pair for the Phase B language plugin protocol.
   - `protocol_version` = the wire-compatibility axis (bump on a breaking wire change). `plugin_version` = the binary's own crate semver (behavioral axis).
   - Today `plugin_version` is read **only for optional FEATURE detection, fail-open**: `crates/travsr-plugin-host/src/embed_sidecar.rs:84` `supports_doc_space()` (MIN 1.2), and `crates/travsr-plugin-host/src/embed_catalog.rs:323` `sidecar_supports_cancel()`, which runs `<bin> --version`, parses `travsr-embed MAJOR.MINOR.PATCH`, compares `(major, minor) >=` **locally, no network** - and returns only a `bool`. That is exactly the "read the installed version cheaply" primitive; it merely needs generalizing to *return* the version instead of a yes/no for one feature.

2. **Per-entry `version_fallback` catalog fields already exist in both families.** Embed: `crates/travsr-plugin-host/src/embed_catalog.toml`, every entry `version_fallback = "v1.0.0"` (lines 16, 43, 69, 95, 121). Phase B: `crates/travsr-plugin-host/src/phase_b/catalog.rs` (`version_fallback`, `wrapper_version_fallback`, e.g. scip-java `v0.12.3` at line 359).

Everything needed to detect #701 was already in the process. Three gaps stop it from being used:

- **G1 - NO COMPATIBILITY FLOOR.** The host declares no minimum-required version. Handshake versions are read for feature-gating but never enforced as a floor, so a v1.0.0 sidecar spawns happily and fails deep with a cryptic error. Fail-open is correct for **optional** features (`supports_doc_space`) but wrong for **required** behaviors - content-hash CDC invalidation (#376, travsr-embed v1.2.0) was required, not optional, and its absence is what produced #701's SQLite error.
- **G2 - NO STALENESS ADVISORY.** Existence-only install gates never re-consult latest after install day. The pin is silent and permanent.
- **G3 - FRAGMENTED LIFECYCLE.** Embed and Phase B each reimplement fetch / fallback / existence-gate independently. There is no shared abstraction, so any **future** sidecar re-inherits both G1 and G2 by default.

**Latent bug this RFC also closes:** every embed `version_fallback = "v1.0.0"` is *below* the v1.2.0 behavior floor that #701 requires. A fresh **offline** install (latest fetch fails → falls back) therefore lands *under* the floor - reproducing #701 on a brand-new machine. The fallback and the floor must be made consistent by construction.

---

## 3. Design thesis

> A sidecar is not "installed" because a file exists at a path. A sidecar is usable when the binary on disk satisfies the **behavioral floor the host requires** (a hard, offline gate) and the user has been told when a **newer release exists** (a soft, best-effort, install-time advisory). Both axes are already carried by the spawn handshake; this RFC enforces what the process already knows, in one place both families share.

Two version axes, two enforcement points, one home:

```
                       ┌──────────────── travsr-plugin-host (home crate) ────────────────┐
SidecarSpec (trait) ──►│  min_version()  version_fallback()  pinned()  install_name()    │
   over existing       │                                                                  │
   structs             │  installed_version(bin, live_handshake) -> Option<Semver>        │
                       │      prefer negotiated plugin_version (embed: live, 0-cost)      │
                       │      else cheap `<bin> --version` probe (offline)                │
                       │                                                                  │
   POINT A (HARD) ─────┤  resolve_backend / tool_available:                               │
   offline, every      │      installed_version  <  min_version  -> REFUSE + remedy       │
   resolve/spawn       │      protocol_version   != host        -> REFUSE (hard)          │
                       │                                                                  │
   POINT B (SOFT) ─────┤  embed init / lang install (existence branch, NO --reinstall):   │
   network,            │      best-effort fetch latest (24h TTL cache) -> advise if newer │
   install-time only   │      offline -> silent, never fails                              │
                       └──────────────────────────────────────────────────────────────────┘
```

Point A converts #701's cryptic SQLite error into an actionable one for **every** family and **every future** sidecar, with zero added network. Point B gives the user the chance to notice drift that presence-only installs deny them. Neither reindex nor any daemon spawn ever touches the network.

---

## 4. Principal Architect ruling (philosophy compliance)

This RFC adds no ML, no graph edges, and no retrieval behavior. It is evaluated against the invariants that *do* apply: crate boundaries, local-first, and the "no surprise network" posture.

| Invariant / principle | Verdict |
|---|---|
| #6 - MCP is the only external interface | **UPHELD.** No new interface. Point B's install-time latest-fetch reuses the existing GitHub-release fetch (`install.rs:60`) already used at install; it is not a query surface. |
| Local first - data stays on the machine; no surprise network | **UPHELD.** Point A is 100% offline (reads the negotiated handshake or a local `--version` probe). Point B fetches only at explicit install time, best-effort, cached 24h; offline is silent. Reindex and daemon spawns are network-free by construction. |
| Crate dependency rules (CLAUDE.md) | **UPHELD.** The trait and both enforcement points live in `travsr-plugin-host`, which already owns both catalogs and is depended on by `travsr-mcp`, `travsr-daemon`, and `travsr-cli`. No new edge is introduced; `travsr-analysis` core-only invariant untouched. |
| Fail-open vs fail-closed discipline | **CORRECTED.** The prior design fails open on *everything*, including required behaviors. This RFC fails **closed** on the compatibility floor (a required-behavior gate) and stays **open** on the advisory (an optional convenience). That is the correct split, and stating it explicitly is the point of G1. |
| #3 - LLM/ML must not determine structure | **N/A** - no model involved. |

**Ruling:** Enforcing a declared behavioral floor is not new policy; it is honoring version data the handshake already negotiates and that the host currently discards. The one genuinely new field is `min_version`. **Approved in principle**, subject to §11 acceptance criteria and the phased rollout in §10. The load-bearing constraint the PA attaches: **`min_version` must be bumped in the same commit that first relies on a sidecar behavior** - exactly as #376 should have set the embed floor to v1.2.0 and did not. A CI honesty test (§5.5) makes that non-optional going forward.

---

## 5. Component architecture (Solution Architect)

Home crate is **`travsr-plugin-host`**: it owns both catalogs, and `mcp` / `daemon` / `cli` all depend on it. The CLI calls into it for the install-time advisory; the daemon reads its cached state for log events. Nothing new sits above the trust boundary.

### 5.1 Part 1 - a uniform version descriptor as a TRAIT over existing structs

No struct rewrite. A single trait unifies the four descriptor types that already exist, adding exactly one new field:

```rust
// travsr-plugin-host
pub trait SidecarSpec {
    fn install_name(&self) -> &str;      // e.g. "travsr-embed", "scip-java"
    fn github_repo(&self) -> &str;       // existing
    fn min_version(&self) -> Semver;     // NEW - the behavioral floor the HOST requires
    fn version_fallback(&self) -> &str;  // existing (embed_catalog.toml / phase_b/catalog.rs)
    fn pinned(&self) -> bool;            // existing lang concept (resolve_install_tag)
}
```

`EmbedBackend`, `PhaseBEntry`, `ScipBinarySpec`, and `ZipBinarySpec` each `impl SidecarSpec`. `min_version` is the **only** genuinely new field; the rest are read-throughs to data these structs already hold. The trait exists so that Point A and Point B are written **once** against `&dyn SidecarSpec` and both families - plus any future sidecar - inherit the floor and advisory by implementing five methods, closing G3.

### 5.2 Part 2 - one local version reader (zero network)

Consolidate the two ad-hoc readers (`supports_doc_space`, `sidecar_supports_cancel`) into one function that returns the version instead of a feature bool:

```rust
// travsr-plugin-host, offline, cheap
fn installed_version(bin: &Path, live: Option<&Handshake>) -> Option<Semver>;
```

Resolution order:
1. **Prefer the already-negotiated handshake `plugin_version`.** For embed the sidecar is already spawned and the value is live at zero cost (`EmbedHandshakeResponse.plugin_version`, protocol `embed.rs:83`).
2. **Else a cheap `<bin> --version` probe.** Generalize the `sidecar_supports_cancel` parser (`embed_catalog.rs:339-347`) into `parse_sidecar_version`, returning `Semver` rather than `(major, minor) >= MIN`. The Phase B `--version` probes already present in the runners (`runner.rs:554`, `ra_runner.rs:45`/`:212`) feed the same parser.

**CORRECTION from prototype (2026-08-14) - do NOT lift `sidecar_supports_cancel`'s parser verbatim.** That parser takes `stdout.split_whitespace().last()`. It only works because `travsr-embed` prints a single-token `travsr-embed 1.2.0`. Run against the real Phase B tools installed on this machine, `.last()` silently mis-reads two of five:

| Binary (real `--version` output) | `.last()` (shipped) | first-semver-token (required) |
|---|---|---|
| `travsr-embed 1.2.0` | `1.2.0` ok | `1.2.0` |
| `scip-java` -> `0.12.3` | `0.12.3` ok | `0.12.3` |
| `scip-clang 0.4.0` (+ trailing lines) | **None** | `0.4.0` |
| `scip-ruby 0.4.7 git <hash> ... clean=1` | **None** (`clean=1`) | `0.4.7` |
| `travsr-embed.OLD` (pre-1.1.0) -> exit 1 | None (correct) | None (correct) |

`parse_sidecar_version` MUST scan for the first whitespace token that parses as `X.Y.Z` (`split_whitespace().find_map(parse_semver)`), not `.last()`. With `.last()` the floor would wrongly REFUSE healthy `scip-clang`/`scip-ruby`. This also means the existing `sidecar_supports_cancel` should be refactored onto the corrected parser (it is embed-only today so has not been bitten, but shares the latent bug).

Both paths are strictly local. `installed_version` is the single primitive Point A and the health line (§8) consume.

### 5.3 Part 3 - TWO enforcement points, TWO version axes

**POINT A - Compatibility floor (HARD, offline, every resolve/spawn).**
`resolve_backend` (`embed_catalog.rs:1379`) and `tool_available` (Phase B, behind `lang.rs:418`) compare `installed_version(bin, handshake)` against `spec.min_version()` **before** the sidecar does real work. Two sub-checks, both hard-refuse:

- Below the behavioral floor → refuse with the remedy named, e.g.
  `travsr-embed v1.0.0 is below the required v1.2.0 - run: travsr embed init --reinstall`
- `protocol_version` mismatch (wire axis) → the same hard refuse. Today `protocol_version` is read and never enforced; this makes a wire-incompatible sidecar refuse instead of misbehave.

Zero network on this path. This is the change that turns #701's `Error code 517` into an actionable message for every family and every future sidecar. Refusal is a clean typed error (`travsr-error` taxonomy), not a panic - the caller renders the remedy.

**POINT B - install-time checks (existence branch, `embed init` / `lang install` without `--reinstall`).**
The existence branch that currently just prints "ready" (`embed.rs:569`) or "already installed" (`lang.rs:362`) grows **two independent legs of the same branch**:

- **Leg 1 - offline floor check (HARD logic, WARN presentation).** Run the same Point A comparison (`installed_version` vs `spec.min_version()`) with **zero network**. If the on-disk binary is below the floor, print a **WARN** with the explicit remedy - e.g. `travsr-embed v1.0.0 is below the required v1.2.0 - run: travsr embed init --reinstall` - while still reporting a usable state (init does not abort). This catches a stale binary **at init, offline**, instead of deferring the discovery to the next `reindex`. The **hard refuse stays at resolve/spawn** (Point A, §5.3 above); init only warns, because init's job is to leave the user in a runnable state and the reinstall remedy is one command away.
- **Leg 2 - staleness advisory (SOFT, best-effort, network).** Fetch latest best-effort and, if newer than installed, print `newer travsr-embed v1.3.0 available - run: travsr embed init --reinstall`. Offline → silent, never fails the command. Result cached in `~/.travsr/.sidecar-latest.json` with a **24h TTL** (see §5.4).

Leg 1 depends on no network and always runs; Leg 2 is the only leg that touches the network. **Reindex and daemon spawns touch neither** - Point A is offline, and they read the cache only.

### 5.4 Cache ownership (`~/.travsr/.sidecar-latest.json`)

The 24h TTL latest-cache is warmed **only** by Point B Leg 2's install-time fetch. **The daemon never fetches** (local-first, no surprise network - §4). Two deliberate consequences follow, and neither is a bug:

- A **cold or expired** cache means the daemon's `sidecar.version.stale` event (§7.1) **simply does not fire**, and the health-line `latest` column (§8) is **omitted**. The correctness axis (installed vs required) is unaffected - it is computed offline and always shown.
- Therefore "the daemon emits `sidecar.version.stale`" must **not** be read as "the daemon proactively detects new releases." The daemon only re-surfaces what a prior CLI install already fetched and cached. Proactive release detection is out of scope by design (§13).

### 5.5 Part 4 - keep the floor honest (CI tests)

Two tests in CI enforce that the floor cannot silently rot:

- **(a) `version_fallback >= min_version` for every catalog entry** (offline, always runs). This forces the embed `v1.0.0` fallbacks to be bumped to at least the floor, closing the latent offline-install bug in §2: a fresh offline install can never land *below* the floor because the fallback it uses is itself `>= min_version`.
- **(b) `min_version <= latest_released`** (network test, skippable offline). A floor may never be set above what users can actually install.

**Governing rule (PA-mandated):** bump `min_version` in the **same commit** that first relies on a sidecar behavior - as #376 should have set the embed floor to v1.2.0.

---

## 6. Integration

### 6.1 Which call sites change

| Site | File:line | Change |
|---|---|---|
| Embed resolve/spawn | `embed_catalog.rs:1379` (`resolve_backend`), guard at `:1388` | After the existing existence check, add Point A floor + protocol check via `installed_version` before returning `Some(..)`. |
| Embed install advisory | `embed.rs:568-569` (existence branch of `install_backend_with_progress`) | Add Point B best-effort advisory (cached). The `else` fetch branch is unchanged. |
| Phase B tool gate | `lang.rs:418` (`tool_available`) | Point A floor for `scip-*` / wrapper tools. |
| Phase B install advisory | `lang.rs:362` (existence branch) | Point B advisory. |
| Version reader | `embed_catalog.rs:323` (`sidecar_supports_cancel`), `embed_sidecar.rs:84` (`supports_doc_space`) | Refactor both to consume the shared `installed_version` / `parse_sidecar_version`; behavior for those two features is preserved. |
| Tag resolver (P3) | `lang.rs:566` (`resolve_install_tag`) | Fold embed's inline `fetch_latest_version_for_repo` (`embed.rs:573-576`) into this shared resolver so both families share one tag-resolution path (closes G3 at the fetch layer too). |
| Catalog data | `embed_catalog.toml:16/43/69/95/121`, `phase_b/catalog.rs:359` | Bump `version_fallback` off `v1.0.0`; embed floor set to `v1.2.0`. |

### 6.2 Crate-boundary check

Trait, reader, both enforcement points, and the TTL cache live in `travsr-plugin-host`. CLI (`embed.rs`, `lang.rs`, `install.rs`) already depends on plugin-host; daemon already depends on plugin-host. No new crate edge; the `travsr-analysis → core only` and single-trust-boundary invariants are untouched.

---

## 7. Daemon log observability (new scope)

Daemon logging is `tracing` + `tracing-appender` daily rolling `daemon.log.YYYY-MM-DD` (`crates/travsr-daemon/src/lib.rs:7839`; `logfile.rs:60` `LOG_PREFIX`), `EnvFilter` default `info` (`lib.rs:7860`), with a rotation-aware reader behind `travsr daemon logs`. Critically, the daemon log carries a **stable, jq-queryable structured event-key contract** documented at `crates/travsr-daemon/src/logfile.rs:15-40` (keys like `phase_b.start`, `phase_b.complete`, `phase_b.indexed`, `embed.text.updated`, `head.reconcile.complete`, `kcore.updated`). **Renaming a key is a breaking change.** New events MUST follow the `<domain>.<event>` convention and are themselves a stable contract once shipped.

### 7.1 New stable structured events

The Phase B scheduler (`crates/travsr-daemon/src/phase_b_sched.rs`) re-arms every ~5s, and embed auto-reindex is gated in plugin-host `maybe_spawn_embed`. Version events therefore **MUST NOT fire every tick** - they are transition-only.

| Event key | Level | When | Fields |
|---|---|---|---|
| `sidecar.version.checked` | **DEBUG** | every resolve/spawn (kept at debug so healthy spawns do not flood at info) | `{family, install_name, installed_version, min_version, protocol_version, disposition: ok\|below_floor\|unreadable}` |
| `sidecar.version.below_floor` | **WARN** | Point A refusal | `{install_name, installed, required, remedy}` - **transition only** (log-once per `(install_name, installed_version)`), never every 5s tick |
| `sidecar.version.stale` | **INFO** | Point B advisory: cached latest > installed | `{install_name, installed, latest}` - daemon emits from the **cached** value only (the daemon itself must not fetch - see §5.4); transition-only. A cold/expired cache means this event **does not fire**; it is not proactive release detection. |
| `sidecar.version.unreadable` | **WARN** | `--version` / handshake parse failed | `{install_name, reason}` |

### 7.2 De-dup mechanism

An in-memory `last-logged-version` map keyed by `install_name` (state lives in plugin-host, read by the daemon's scheduler loop). The **health line** (§8) holds current state; the **log** records only transitions. A below-floor sidecar that is spawned every 5s produces exactly **one** `sidecar.version.below_floor` line per distinct `(install_name, installed_version)`, not one per tick. When the user reinstalls and the installed version changes, the key changes and a new transition may log.

---

## 8. Surfacing (non-log)

Add one sidecar-health line to `travsr status`, `travsr embed status`, and `travsr lang list`: installed vs required vs latest, per sidecar. Example:

```
sidecars:
  travsr-embed   1.0.0  below required 1.2.0  - run: travsr embed init --reinstall
  scip-java      0.12.3 ok        (latest 0.12.4 available)
```

`installed` and `required` come from the offline path (`installed_version` + `min_version`); `latest` is read from the 24h TTL cache and omitted when the cache is cold/offline. This makes the floor state legible without reading logs and gives the user the exact remedy string.

---

## 9. Failure modes & graceful degradation

| Condition | Behaviour |
|---|---|
| Sidecar below floor (Point A) | **Hard refuse** with named remedy. This is the #701 case: the fix is to refuse *before* the cryptic downstream error, not to tolerate it. |
| `protocol_version` mismatch | Hard refuse (same as below-floor); wire-incompatible sidecar never does real work. |
| `--version` / handshake unparseable **and a floor is declared** | `installed_version` → `None` → `sidecar.version.unreadable` (WARN). Treated conservatively: cannot prove compliance → **refuse**. |
| `--version` / handshake unparseable **and no `min_version` declared** (effectively `0.0.0`) | **Allow** - the floor is trivially met, so an unreadable version cannot violate it. Logged at **DEBUG** (`sidecar.version.checked`, disposition `unreadable_no_floor`), not WARN: a warning that fires forever on a permitted state trains readers to ignore warnings, and today every Phase B entry is in exactly this state. |
| `--version` probe **exceeds its deadline** (`VERSION_PROBE_TIMEOUT`, 3s) **and a floor is declared** | Watchdog kills the probe; `sidecar.version.probe_timeout` (WARN, transition-only by name) → `ProbeTimeout` → **degrade to usable**. A timeout is a transient/infra condition (a slow first exec, e.g. macOS Gatekeeper verifying a freshly-downloaded binary), not proof the binary is old, and the real work downstream is already bounded by its own per-call watchdog. Kept distinct from `unreadable` so the timeout case is explicit rather than inherited. |
| Offline at install (Point B advisory) | Silent; the install command never fails on a failed latest-fetch. |
| Offline at resolve/spawn (Point A) | Unaffected - Point A is 100% offline; the floor is enforced from local `--version` / handshake with no network. |
| Cache cold/stale (health line `latest`) | `latest` column omitted; installed-vs-required (the correctness axis) still shown. |
| Daemon wants "latest" | Reads cache only; **never fetches**. If cache is cold, `sidecar.version.stale` simply does not fire. |

Degradation is toward **refuse-with-remedy** on the required axis when age is *proven* (below-floor, or unreadable with a floor), **degrade-to-usable** when the version is merely *undetermined by a transient probe timeout* (the downstream operation stays watchdog-bounded regardless), and **silent** on the advisory axis. There is no path where drift produces a cryptic error the user cannot act on, and no path where a slow one-off `--version` exec turns a freshly-installed sidecar into a hard refusal.

---

## 10. Phased rollout

Each phase gets a throwaway **prototype** validated against the actual #701 end-user scenario (§11) before implementation.

- **P1 - unblocks #701 (correctness, both families).**
  `SidecarSpec::min_version` + `installed_version()` + Point A floor in `resolve_backend` / `tool_available`; set the embed floor to **v1.2.0**; bump every `version_fallback` off `v1.0.0`; the two honesty tests (§5.5). After P1, the reproduction command refuses with the actionable message instead of `Error code 517`.
- **P2 - visibility.**
  Point B advisory + 24h TTL `~/.travsr/.sidecar-latest.json` cache; the health line on `status` / `embed status` / `lang list` (§8).
- **P3 - observability + consolidation.**
  The four daemon-log events (§7.1) with transition-only de-dup (§7.2); enforce `protocol_version` mismatch as a hard refuse; fold embed's ad-hoc inline `fetch_latest_version_for_repo` into the shared `resolve_install_tag` so both families share one tag resolver (final close of G3).

---

## 11. Acceptance criteria

Validated against the **actual #701 scenario**: a `travsr-embed` **v1.0.0** binary on disk against a store/host expecting v1.2.0 behavior.

- [ ] `SidecarSpec` trait added in `travsr-plugin-host`; `EmbedBackend`, `PhaseBEntry`, `ScipBinarySpec`, `ZipBinarySpec` all `impl SidecarSpec`; `min_version()` present on each.
- [ ] `installed_version(bin, handshake)` consolidates `supports_doc_space` / `sidecar_supports_cancel`; both preexisting feature checks still pass; zero network on this path.
- [ ] **(#701 root fix)** With v1.0.0 on disk and floor v1.2.0, `travsr embed reindex` **refuses with the actionable floor message** (`travsr-embed v1.0.0 is below the required v1.2.0 - run: travsr embed init --reinstall`), **not** the `Error code 517` SQLite error.
- [ ] `travsr embed init` (no `--reinstall`, newer release available) prints the staleness advisory; offline prints nothing and exits 0.
- [ ] `travsr embed init` (no `--reinstall`) over a **below-floor** binary prints the WARN + remedy **offline** (Leg 1) and still leaves a usable state; the hard refuse remains at `reindex`/spawn.
- [ ] Cold/expired `~/.travsr/.sidecar-latest.json`: daemon emits no `sidecar.version.stale`, health-line `latest` column omitted, installed-vs-required still shown (cache-ownership behavior, §5.4).
- [ ] The daemon emits `sidecar.version.below_floor` **exactly once** for a below-floor sidecar spawned repeatedly by the ~5s scheduler (transition-only de-dup verified).
- [ ] `travsr status` shows the sidecar-health line (installed vs required vs latest) with the remedy.
- [ ] Honesty test (a): every catalog entry has `version_fallback >= min_version` (embed `v1.0.0` fallbacks bumped; a fresh **offline** install can never land below the floor).
- [ ] Honesty test (b): `min_version <= latest_released` for every entry (network, skippable offline).
- [ ] `protocol_version` mismatch hard-refuses (P3).
- [ ] Phase B family: an under-floor `scip-*`/wrapper tool refuses via `tool_available` with the same message shape (parity with embed).
- [ ] No new network call on `embed reindex` or any daemon spawn (Point A offline; advisory install-time + cached).

Each phase is prototyped against this scenario before implementation, per §10.

### 11.1 Prototype validation run (2026-08-14, std-only harness vs real binaries)

A throwaway `rustc` harness exercised the design's load-bearing logic against the actual sidecars in `~/.travsr/bin/`, including a real pre-1.1.0 `travsr-embed.OLD` (the #701-era binary) and current `travsr-embed 1.2.0`, plus `scip-java`/`scip-clang`/`scip-ruby`. Results:

- **P1 floor / #701 outcome CONFIRMED.** `travsr-embed 1.2.0` -> ALLOW; `travsr-embed.OLD` (`--version` exits 1) -> `Unreadable` -> REFUSE with remedy. The real #701 binary produces the actionable early refusal, not `Error code 517`. Note the #701 binary predates `--version` entirely (it self-names `travsr-embed-nomic`), so "unreadable -> refuse" is the *common* #701 path, not a rare edge.
- **P1 parser DEFECT FOUND AND CORRECTED** (see §5.2): the shipped `.last()` parser mis-reads `scip-clang` and `scip-ruby`; the corrected first-semver-token parser reads all five real binaries.
- **P2 advisory CONFIRMED** for all four states (fresh cache -> `1.3.0`, TTL-expired -> none, no cache -> none, at-latest -> none). Real latest tag resolved to `v1.3.0`.
- **P3 de-dup CONFIRMED**: 1 log line across a 10-tick storm; +1 on the reinstall transition.

Net: the RFC holds on all three phases; the only correction reality forced was the version-string parser generalization, now folded into §5.2.

---

## 12. Decisions (locked)

1. **`min_version` is the single new field; everything else is read-through.** The trait unifies four existing structs; no struct rewrite, no new catalog schema beyond the one field.
2. **Point A is hard and offline; Point B is soft, install-time, and cached.** The floor never touches the network; the advisory never fails a command. Reindex and daemon spawns are network-free.
3. **`min_version` bumps in the same commit that relies on the behavior.** Enforced by honesty test (a); the #376/#701 gap (behavior shipped, floor not raised) is the anti-pattern this rule forbids.
4. **Embed floor set to v1.2.0** (content-hash CDC invalidation, #376) and **all `version_fallback` bumped off `v1.0.0`** to `>= floor`.
5. **New log keys are a stable contract.** `sidecar.version.{checked,below_floor,stale,unreadable,probe_timeout}` follow `<domain>.<event>`. All four repeating events are transition-only: `below_floor`/`stale` key on the version(s); the two version-less events (`unreadable`, `probe_timeout`) key on the install name alone. The daemon emits `stale` from cache only (never fetches). `checked` is DEBUG and also carries the permitted `unreadable_no_floor` disposition (no separate WARN for a state nothing refuses).
6. **Home crate is `travsr-plugin-host`.** One trait, one reader, two enforcement points - both families and every future sidecar inherit them; no crate-boundary change.
7. **`parse_sidecar_version` scans for the first `X.Y.Z` token, never `.last()`.** Proven necessary against real `scip-clang`/`scip-ruby` output (§5.2, §11.1); the existing embed-only `sidecar_supports_cancel` is refactored onto it.

---

## 13. Security (trust-boundary exec on a new path)

`travsr-plugin-host` is the trust boundary (CLAUDE.md; ADR-017). This RFC adds a
new external-binary exec inside it: the offline version reader resolves a sidecar
via `travsr_core::exec::resolve_executable(install_name)` - which may return a
`PATH` match - and runs it with a fixed `--version` argument. That is worth
naming explicitly so a later reader finds it already reasoned about.

**Why `--version` is not the ADR-017 threat.** The threat ADR-017 governs, and
the reason its trust grant is *per-corpus*, is running a language's build tooling
**against a repository's contents**. The version probe takes **no repo input**:
the argument is the fixed literal `--version`, the binary is one the user
installed on purpose (`travsr embed init` / `travsr lang install`), and the probe
reads only the tool's own self-reported version. There is no attacker-controlled
input on this path, so it is deliberately **outside** the Phase B sandbox - a
sandbox here would guard nothing. The exec is also bounded: the probe runs under
a hard `VERSION_PROBE_TIMEOUT` (§9) and is SIGKILLed on expiry, so a wedged or
malicious binary cannot hang the trust-boundary crate.

**The probe does not run when it cannot matter.** A version probe whose result no
floor can consume is pure cost - an exec and a process spawn to compute a value
nothing reads. Every Phase B entry declares `Semver::ZERO` (decision 3: no floor
until a behavior relies on one), so the Phase B floor check is **gated on
`min_version() != Semver::ZERO`** and does not exec at all today. This keeps the
no-floor family paying nothing (no exec, no probe timeout, no log line), and
activates the exact moment a real Phase B floor is declared. The embed floor
(v1.2.0, the actual #701 defect) is the only live floor, so embed is the only
family that probes. When a Phase B floor lands, its probe inherits the same
bound and the same "fixed-argument, no-repo-input" property analysed above.

---

## 14. Out of scope

- **No auto-update / self-download.** A below-floor sidecar is *refused with a remedy*, never silently re-fetched and replaced - that would violate local-first and introduce surprise network. The user runs `--reinstall`.
- **No handshake protocol changes.** This RFC enforces `protocol_version` / `plugin_version` that the handshake already carries (`embed.rs:82-83`, `types.rs:82-83`); it does not add or alter wire fields.
- **No new per-invocation network calls.** The floor is offline; the advisory is install-time + 24h-cached; the daemon reads cache only.
- **No change to retrieval, graph, embeddings, or rerank behavior.** This is a lifecycle/version-contract RFC, not an algorithm RFC.
- **Fine-grained per-feature capability negotiation** (a matrix of feature flags beyond `supports_doc_space`/cancel) is a possible future refinement, not part of this contract.
