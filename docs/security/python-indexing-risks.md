# Python Indexing — Security Risks and Mitigations

**Applies to:** `travsr-indexer/src/python_lsif.rs` · S13 (#171) onwards

## Summary

Travsr optionally invokes `pyright --outputjson` as a subprocess to enrich Python semantic edges. pyright loads and type-checks user `.py` files, which means it executes Python's import machinery. This introduces a security boundary that users must understand.

## What pyright does

pyright imports user Python modules to resolve types. A repository containing a malicious `__init__.py` with module-level side-effects (e.g. `os.system(...)`) could execute arbitrary code under the indexing user's UID when pyright processes that module.

## Mitigations implemented in S13

1. **Explicit minimal environment** — pyright is invoked with `env_clear()`. Only `PATH` (single entry pointing to the pyright binary), `HOME` (set to a temp dir), and `LANG/LC_ALL` are set. `PYTHONPATH`, `PYTHONHOME`, `NODE_OPTIONS`, `LD_PRELOAD` are never inherited.
2. **`Stdio::null()` for stdin** — pyright receives no stdin; the channel is closed before exec.
3. **Working directory = per-invocation `tempdir()`** — pyright writes no cache files into the user's repo.
4. **Hard resource limits** — on Unix, `RLIMIT_AS` (2 GB), `RLIMIT_CPU` (60 s), `RLIMIT_NOFILE` (256) are applied before exec via `pre_exec`.
5. **Timeout** — a 30-second wall-clock timeout kills the subprocess if it does not complete.

## Known gaps (deferred to S18)

- **No sandbox** — full bwrap (Linux) / sandbox-exec (macOS) / AppContainer (Windows) wrapping is **not** implemented in S13. The mitigations above reduce the attack surface but do not provide OS-level isolation.
- pyright is **opt-in** — it only runs if `pyright` is present on `PATH`. Users who do not install pyright receive tree-sitter-only Python edges with no risk.

## Recommendation

Do not run `travsr index` on untrusted repositories with pyright installed unless you have reviewed the repository's Python code for hostile module-level side-effects. This is the same risk profile as running `pyright` directly on an untrusted codebase.

A full subprocess sandbox will be specified and implemented in the S18 security audit sprint.
