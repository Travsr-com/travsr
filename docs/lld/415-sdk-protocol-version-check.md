# LLD 415 - SDK runners enforce the daemon protocol version

- **Issue:** #415 `[travsr-plugin-sdk] SDK runners skip protocol-version check and have no tests`
- **Crates touched:** `travsr-plugin-sdk`
- **Contract sources:** RFC-011 §4 (fail-fast versioning), ADR-017 ("Protocol version is fail-fast"), RFC-025 §9 (`protocol_version` mismatch = hard refuse)

## Problem

`run_plugin` (`crates/travsr-plugin-sdk/src/runner.rs:27`) and `run_embed_plugin`
(`crates/travsr-plugin-sdk/src/embed_runner.rs:35-39`) receive
`daemon_protocol_version` in the handshake and only log it. Neither compares it
against the SDK's compiled `PROTOCOL_VERSION` / `EMBED_PROTOCOL_VERSION`, so a
plugin built on the SDK will serve `Parse` / `Invoke` / `Embed` / `Knn` frames to
a peer whose wire contract it does not speak. The crate also has no tests at all,
so the dispatch logic in both loops is unguarded against protocol enum changes.

## Root cause

The protocol is symmetric on the wire, but *enforcement* was written only into the
host. Every host entry point compares the version it received against its own
compiled constant and refuses:

- `crates/travsr-plugin-host/src/transport.rs:248` - `h.protocol_version != PROTOCOL_VERSION` returns `IndexError::ProtocolVersionMismatch`
- `crates/travsr-plugin-host/src/dispatcher.rs:64` - the same check at registration
- `crates/travsr-plugin-host/src/embed_sidecar.rs:215` - `EmbedError::VersionMismatch`

Because the host compares the *response* version to its own constant, the
first-party daemon already catches a skew in **both** directions, and catches it
before it sends any work frame. So this is not a live bug against the Travsr
daemon, and for that path the SDK check is a duplicate.

The real defect is a contract gap, not a missing direction. `travsr-plugin-sdk` is
the published surface third-party plugin authors build against, and it is the only
place a third-party binary gets its half of the fail-fast rule. RFC-011 §4 and
ADR-017 state that rule as a property of the protocol ("never driven with a
mismatched contract"), not as a host feature, and the reason ADR-017 gives is
mis-decoding: the frame types are serde-lenient (`#[serde(default)]` on
`InvokeRequest::corpus`, `scratch` and `files`, and on
`EmbedHandshakeResponse::max_batch`; unknown fields ignored), so a skewed frame
does not fail to parse, it parses into silently wrong values. A plugin driven by
any peer that does not implement the host's check therefore degrades into wrong
nodes and edges instead of erroring. The SDK is the only side that can close that.

Two points where the issue's framing is incomplete:

1. It asks for rejection "on a major-version mismatch". There is no major version.
   `PROTOCOL_VERSION: u32 = 1` (`types.rs:7`) and `EMBED_PROTOCOL_VERSION: u32 = 1`
   (`embed.rs:21`) are monotonic integers, and RFC-011 §4 describes the field as
   "monotonic; daemon fail-fasts on mismatch". The documented policy is exact
   match, so a semver-style major-only comparison would invent a laxer policy than
   the contract.
2. It also lists handshake-first ordering under L1. That is a separate property
   (the host controls ordering), it is absent from the acceptance criteria, and it
   is out of scope here.

## Options considered

| Option | Verdict |
| --- | --- |
| **Warn and continue** (log at `warn!`, keep serving) | Rejected. This is exactly the "silent wrong output on version skew" mode RFC-011 §4 says the design "structurally eliminates", and ADR-017 names the resulting forged nodes/edges as threat T12. |
| **Negotiate down** (plugin serves the older of the two versions) | Rejected. There is one version and no per-version compatibility shims to select between, so negotiation is machinery for a matrix that does not exist. RFC-025 §9 rules a `protocol_version` mismatch a "hard refuse", not a downgrade. |
| **Major-version-only match** (as the issue suggests) | Rejected. `PROTOCOL_VERSION` has no major component; see Root cause. |
| **Refuse and exit, exact match** (chosen) | Matches the host's own `!=` comparison and all three documents verbatim. |

## Chosen design

In the handshake arm of each runner: if `daemon_protocol_version` differs from the
SDK's compiled constant, write a `PluginResponse::Error` /
`EmbedPluginResponse::Error` naming both versions, log at `error!`, and break the
loop so no work frame is ever served. Otherwise proceed unchanged.

The comparison lives once in `src/protocol_compat.rs` as
`ensure_protocol_compatible(daemon_version, supported_version) -> Result<(), PluginError>`,
used by both runners, so the two near-duplicate loops (issue T1) cannot drift.

Each runner's loop body moves into a private `run_*_loop(plugin, reader, writer)`
generic over `Read`/`Write`. The public `run_plugin` / `run_embed_plugin`
signatures are unchanged; the split exists so the loops are drivable from an
in-memory `Cursor` in tests.

### Why optimal here

- It is the smallest change that makes the SDK obey the already-written policy: one
  comparison, one shared helper, no new public API, no new protocol fields
  (RFC-025: "No handshake protocol changes").
- Responding with `Error` before exiting gives the peer a decodable frame
  explaining the refusal instead of a bare EOF it must guess at. Both response
  enums already carry an `Error(PluginError)` variant, so nothing on the wire
  changes.
- It matches the host's error-handling idiom (`error!` then break) and its
  exact-match comparison, so both halves of the protocol fail the same way for the
  same reason.

## What the documented version contract requires, and how this implements it

- RFC-011 §4: "`HandshakeResponse.protocol_version` is a monotonic integer. If the
  daemon does not support it, the plugin is refused at registration with a clear
  error, never driven with a mismatched contract." Implemented as `!=` (monotonic
  integer, not semver), a `PluginError` whose message names both versions (clear
  error), and a loop break before any work frame (never driven).
- ADR-017: "Protocol version is fail-fast ... never driven with a mismatched
  contract that could mis-decode into forged nodes/edges." Implemented by refusing
  before `Parse`/`Invoke` dispatch, the only point at which forged nodes or edges
  could enter the graph.
- RFC-025 §9 disposition table: "`protocol_version` mismatch | Hard refuse (same as
  below-floor); wire-incompatible sidecar never does real work." Implemented as
  refuse-and-exit rather than warn-and-continue.

## Backwards compatibility

No currently-working sidecar breaks.

`PROTOCOL_VERSION` and `EMBED_PROTOCOL_VERSION` are both `1`, and the host sends
those same constants in the handshake request (`transport.rs:222`,
`embed_sidecar.rs:194`). A sidecar built against any released SDK therefore sees
`1 == 1` and behaves exactly as before.

The check can only fire on a version skew, and in every such case the first-party
host already refuses the sidecar on the handshake response, so that sidecar was not
working before this change either. The change turns "host refuses, sidecar keeps
looping" into "both sides refuse", which is what the contract already prescribes.

For third-party plugin authors the contract is now explicit: the binary must be
built against a `travsr-plugin-protocol` whose `PROTOCOL_VERSION` equals the
daemon's. A mismatching plugin emits one `PluginResponse::Error` frame reading
`incompatible daemon protocol version: daemon speaks N, plugin speaks M` and exits
its run loop, rather than serving requests it may mis-decode.

## Test plan

New `#[cfg(test)]` modules, in-memory via `std::io::Cursor`, with one fake plugin
per runner that counts how many work requests it served:

`runner.rs`
1. matching version: `Handshake(1)` then `Parse` yields `Handshake` then `Parse` responses, and the handshake reports `PROTOCOL_VERSION`.
2. mismatching version: `Handshake(99)` then `Parse` yields exactly one `Error` frame and the fake plugin's parse counter stays 0.
3. closed stdin: empty input yields no frames and returns.

`embed_runner.rs`
4. matching version: `Handshake(1)` then `Embed` yields `Handshake` then `Embed`.
5. mismatching version: `Handshake(99)` then `Embed` yields exactly one `Error` and the embed counter stays 0.
6. closed stdin: empty input yields no frames and returns.

`protocol_compat.rs`
7. equal versions are `Ok`; unequal versions produce a message naming both.

## Risks

- **A future protocol bump now hard-fails older sidecars from both sides.** That is
  the intended contract (RFC-025 §9); the host already hard-fails them, and this
  only makes the sidecar say why.
- **Refactoring the loops for testability** could change stdio behaviour. Mitigated
  by keeping the public entry points as thin wrappers constructing exactly the same
  `BufReader`/`BufWriter` over locked stdio as before.
- **`travsr-plugin-sdk` is under the `plugin-hashes.lock` gate** (ADR-017), so the
  lock file must be regenerated in the same commit.
