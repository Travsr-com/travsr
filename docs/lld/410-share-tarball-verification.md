# LLD 410: the share tarball is downloaded and extracted unverified

## Problem

`install_share_assets` fetches `<binary_name>-share.tar.gz` from a travsr-lang
release and extracts it into `~/.travsr/share/<binary_name>/` without checking
its sha256 and without any size cap. Every other download in `install.rs` runs
through `fetch_verified`, which enforces both. The wrapper binary from the *same
release tag* is sidecar-verified; its share tarball, unpacked next to it and run
by the emitter, is not.

## Root cause

Not a missing publication: a leftover download path that was never migrated onto
the shared helper.

* `fetch_verified` (`crates/travsr-cli/src/install.rs:485`) is the single
  download implementation for this file: 404 shaping, pre- and mid-stream size
  caps, progress rendering, and `Integrity::{Sidecar,Vendored,Unverified}`. Its
  doc comment says "single implementation for every download path in this file".
* `install_share_assets` (`crates/travsr-cli/src/install.rs:715`) predates that
  consolidation and still builds its own `reqwest::Client` and calls
  `resp.bytes()` directly (`install.rs:722-737`). #410 M1 moved it off
  `tar -xzf` onto the in-process `extract_tar_gz`, but left the fetch alone, so
  the integrity half of M1 was never applied to it.
* Consequence: no hash check at all, and an unbounded in-RAM buffer. The
  `MAX_ARCHIVE_BYTES` check inside `extract_tar_gz` (`install.rs:1511`) fires
  only *after* the whole body is already resident.

The release side is implicated, but weakly, and not in a way this repo can fix:

* No `-share.tar.gz` asset has ever been published on any travsr-lang release
  (v0.1.0 through v0.4.2 checked), and every catalog entry currently sets
  `has_share_assets: false` (`crates/travsr-plugin-host/src/phase_b/catalog.rs`),
  so the path is unreachable today. It becomes reachable the moment a catalog
  entry flips that flag.
* The publishing workflow is `Travsr-com/travsr-lang/.github/workflows/release.yml`,
  a different repository. It writes a `.sha256` next to *every* artifact it
  stages (three staging blocks, lines 168, 221, 267), which is why all 66 assets
  on v0.4.2 have a sidecar. A share tarball added to that workflow gets a sidecar
  for free; nothing in this repo needs a release change.

So the issue's framing ("publish `.sha256` sidecars for the share tarball") is
incomplete in two directions: the sidecar mechanism already exists generically
upstream, and the asset it would describe does not exist yet at all.

## Options considered

**A. `Integrity::Sidecar` through `fetch_verified` (chosen).** Reuses the helper,
matches the wrapper binary from the same release, and needs no upstream change.

**B. `Integrity::Vendored` with a hash in the catalog.** Strictly stronger, and
what M2 did for the pinned SCIP tools. Rejected: it requires a pinned asset to
vendor a hash *of*. The share tarball's version floats with the wrapper release
tag resolved at install time (`wrapper_install_tag` in `lang.rs`), so there is no
fixed artifact to pin, and no published artifact to hash. Vendoring a hash for an
asset that does not exist would be theatre.

**C. Derive integrity from the already-verified wrapper binary** (embed the
tarball's hash in the wrapper at build time). Chains the tarball to a binary that
is itself only sidecar-verified, so it buys nothing the sidecar does not, and
costs a build-time change in another repo plus a metadata format to parse.

**D. Delete the path.** Finding 3 on the issue suggests deciding whether
`install_share_assets` should exist at all. Out of scope for a security fix, and
leaving unverified extraction in place while that decision is made is the hazard.

## Chosen design

Add `fetch_share_tarball(base, version, binary_name)` beside the existing
`fetch_and_verify_binary`, taking `base` as a parameter for the same reason
(testable against a fixture server without touching process-global state). It
calls `fetch_verified` with `SIZE_LIMIT` and `Integrity::Sidecar`.
`install_share_assets` calls it and keeps only its extraction half.

`SIZE_LIMIT` (100 MB), not `MAX_ARCHIVE_BYTES` (200 MB): the share tarball is
source and metadata, and the tighter cap is the one the wrapper binary from the
same release already carries. `extract_tar_gz`'s own cap stays as the extractor's
independent guard.

## Why optimal here

It is the smallest change that gives the share tarball exactly the property the
binary beside it already has, using the helper that already implements it. No
second verification routine, no new constant, no upstream dependency.

## Threat model

**Closes.** A share tarball corrupted or truncated in transit. A tarball replaced
on the release or CDN without the sidecar being replaced in the same act. A
tarball served with an oversized or absent `Content-Length` (refused pre-download
on the advertised length, and mid-stream when none is advertised). A deleted
sidecar, which now fails the install rather than silently downgrading it to
unverified.

**Does not close.** A compromised travsr-lang release origin or publisher
account: the attacker controls the asset and the sidecar and can make them agree.
Only a vendored hash closes that, and option B explains why one is not available
for this asset. It also does not attest that any human reviewed the tarball's
contents.

**Unchanged.** Traversal and mode hardening still come from `extract_tar_gz`;
verification runs before a single entry is written, so a rejected tarball never
reaches the extractor at all.

## Test plan

Fixture-server tests alongside the existing `#410 T1` suite, driving
`fetch_share_tarball`:

* a tarball matching its published sidecar is returned;
* a tarball swapped after publication is refused with `SHA256 mismatch`;
* a missing sidecar fails rather than installing unverified;
* an oversized tarball is refused on its advertised length, before the body.

Plus the existing `extract_tar_gz` suite, unchanged.

## Risks

* If travsr-lang ever publishes a share tarball *without* a sidecar, the install
  fails instead of proceeding unverified. That is the intended trade and matches
  the wrapper path; the release workflow's per-asset sidecar step makes it
  unlikely. `lang.rs` already reports a share-asset failure as a warning rather
  than aborting the install.
* A share tarball larger than 100 MB would be refused. It is documented as source
  and metadata only, so this is a bound worth having rather than a limit to
  discover.
