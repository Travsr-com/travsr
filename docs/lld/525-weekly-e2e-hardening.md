# LLD 525-2: the weekly lifecycle E2E is built to rot

Covers item 1 of #525.

## Problem

`.github/workflows/embed-lifecycle-e2e.yml` runs the model-switch lifecycle
E2E weekly. As merged it has no cache for `~/.travsr`, no failure
notification, and a 30 minute timeout, and nothing in the repository checks
any of that.

## Root cause

The job inherited an assumption from the test it runs. The test module doc
(`crates/travsr-cli/tests/embed_lifecycle_e2e.rs:15-17`) justifies using the
real `~/.travsr` instead of a fake `$HOME` because "a CI run benefits from the
same cache across runs". That is a statement about a persistent machine.
Hosted runners are ephemeral, so on CI the sentence is only true if the
workflow restores the directory, and the workflow never did
(`Swatinem/rust-cache@v2` covers `~/.cargo` and `target/` only). Every run
therefore re-downloads the sidecar plus roughly 600 MB of weights,
anonymously, inside a 30 minute budget.

The second half is the same shape: nothing observes the job. No `if: failure()`
step, and `schedule` plus `workflow_dispatch` only take effect once the file is
on the default branch, so the workflow had never executed at all when it
merged. A weekly job whose only reader is a red badge nobody looks at reports
coverage it does not have.

So the true defect is not three missing YAML keys. It is that the job's
correctness properties lived only in the YAML, where no test could see them,
next to a test file that asserts a caching claim CI does not satisfy.

## Options considered

1. **Only add the cache and raise the timeout.** Rejected: leaves the failure
   invisible, which is the half that turns into false confidence.
2. **Add a `pull_request` trigger so the job validates itself.** Rejected: it
   is a two-model download and two real reindex passes, exactly the per-PR cost
   the test is `#[ignore]`d to avoid.
3. **Notify by email or a webhook.** Rejected: no such channel is configured in
   this repository, and issues are the notification surface already used.
4. **Assert the workflow shape in a new CI script.** Rejected: `.github/scripts`
   holds shell gates wired into a separate job, and this needs no new job. The
   claim it protects is in a Rust test file, so the assertion goes there.

## Chosen design

- `actions/cache@v4` on `~/.travsr/bin` and `~/.travsr/models`, keyed on the
  embed catalog source so a catalog change does not restore stale weights,
  with a prefix restore key so an unchanged catalog still hits.
- `timeout-minutes: 60`. Cache entries are evicted after 7 days unused and the
  cron is weekly, so cold is the normal case.
- An `if: failure()` step that opens, or comments on, a single tracking issue,
  with `permissions: issues: write`.
- `GH_TOKEN` on the test step so `fetch_latest_version_for_repo` is not
  rate-limited into `version_fallback`, which would test a sidecar several
  releases old and report it as a pass.
- The cron moves from `'30 3 * * 0'` to `'30 5 * * 0'`: the old slot started 15
  minutes into `docs-lane-nightly` (`'15 3 * * *'`, timeout 90).
- `the_weekly_workflow_caches_the_model_dir_and_reports_its_own_failure` in the
  E2E test file asserts the cache paths, the failure step and the issues
  permission. It runs on every PR, unlike the job it describes.

## Why this is optimal here

The guard test is what makes this a fix rather than a one-time edit: it puts
the job's two decay modes under the same CI everything else is under, at the
cost of one file read, and it sits beside the comment whose claim it enforces.
Everything else is the smallest change that makes the job survivable.

## Test plan

- The guard test, confirmed red first (missing `actions/cache`), then green.
- `ruby -ryaml` parse of the workflow, plus a step-name listing, since the repo
  has no YAML linter.
- `cargo test -p travsr-cli -p travsr-mcp`, clippy, fmt.
- The E2E job itself is unchanged in what it runs, and cannot be exercised from
  a branch: `schedule` and `workflow_dispatch` only apply on the default
  branch. A dispatch run after merge is the measurement that settles whether 60
  minutes is right.

## Risks

- The timeout is still a guess until a real run is measured. 60 was chosen over
  a larger value so a genuine hang still fails the same day.
- The failure step files an issue on every red week until fixed; it comments on
  the existing open issue rather than opening a new one, matched by title.
- GitHub auto-disables scheduled workflows after 60 days of repository
  inactivity. Nothing here prevents that, and it is not worth a keepalive job
  for a repository under active development.
