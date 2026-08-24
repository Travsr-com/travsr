#!/usr/bin/env python3
"""Thin wrapper around the `travsr` CLI, plus release artifact handling."""

import hashlib
import re
import shutil
import subprocess
import time
import tarfile
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

#: Ceiling on any single CLI call. A sanity suite that can itself hang is worse
#: than no suite: it turns a product defect into an unattended stuck job.
DEFAULT_TIMEOUT_S = 900

#: Emitted on stderr by several commands. Stripped when comparing output so a
#: staleness banner does not defeat a substring assertion about the answer.
_WARN_RE = re.compile(r"^warning:.*$", re.MULTILINE)


@dataclass
class Run:
    """One CLI invocation's result."""

    argv: list[str]
    code: int
    stdout: str
    stderr: str
    timed_out: bool = False

    @property
    def out(self) -> str:
        """stdout + stderr, since the CLI splits notes and answers across both."""
        return f"{self.stdout}\n{self.stderr}"

    @property
    def answer(self) -> str:
        """Output with `warning:` lines removed, for asserting on the answer."""
        return _WARN_RE.sub("", self.out)

    def __str__(self) -> str:
        return f"$ travsr {' '.join(self.argv)}\n{self.out.strip()}"


class Travsr:
    """A `travsr` binary bound to a repo directory."""

    def __init__(self, binary: Path, cwd: Path, env_extra: Optional[dict] = None) -> None:
        self.binary = binary
        self.cwd = cwd
        # TRAVSR_DISABLE_REGISTRY keeps the suite out of the caller's global repo
        # registry. Without it, every run leaves a fixture repo registered on the
        # machine that ran it.
        self.env_extra = {"TRAVSR_DISABLE_REGISTRY": "1", **(env_extra or {})}

    def run(self, *args: str, timeout: int = DEFAULT_TIMEOUT_S) -> Run:
        import os

        env = {**os.environ, **self.env_extra}
        try:
            p = subprocess.run(
                [str(self.binary), *args],
                cwd=self.cwd,
                env=env,
                capture_output=True,
                text=True,
                timeout=timeout,
                # Never inherit stdin: a subcommand that decides to prompt would
                # otherwise block the whole suite forever.
                stdin=subprocess.DEVNULL,
            )
            return Run(list(args), p.returncode, p.stdout, p.stderr)
        except subprocess.TimeoutExpired as e:
            return Run(
                list(args),
                -1,
                e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or ""),
                e.stderr.decode() if isinstance(e.stderr, bytes) else (e.stderr or ""),
                timed_out=True,
            )

    # ── convenience accessors ────────────────────────────────────────────────

    def version(self) -> str:
        # stdout only. `out` folds stderr in, so any note the CLI happens to
        # print would become part of the version string and fail an equality
        # check for a reason that has nothing to do with the version (#742
        # review).
        return self.run("--version").stdout.strip()

    def status(self) -> str:
        return self.run("status").out

    def semantic_state(self) -> str:
        """The `semantic: ...` field of `travsr status`, or "" if absent."""
        m = re.search(r"semantic:\s*([a-z]+(?:\s*\([^)]*\))?)", self.status())
        return m.group(1).strip() if m else ""

    def daemon_running(self) -> bool:
        return "not running" not in self.run("daemon", "status").out

    def stop_daemon(self) -> None:
        if self.daemon_running():
            self.run("daemon", "stop", timeout=120)


# ── release artifacts ────────────────────────────────────────────────────────

#: Targets published per release. Kept explicit rather than globbed so a release
#: that silently stops shipping one is a failure, not an unnoticed absence.
EXPECTED_TARGETS = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
]


def host_target() -> Optional[str]:
    import platform

    system, machine = platform.system(), platform.machine()
    if system == "Darwin":
        return "aarch64-apple-darwin" if machine == "arm64" else "x86_64-apple-darwin"
    if system == "Linux":
        return "aarch64-unknown-linux-gnu" if machine in ("aarch64", "arm64") else "x86_64-unknown-linux-gnu"
    if system == "Windows":
        return "x86_64-pc-windows-msvc"
    return None


#: Release assets are tens of megabytes each, so a TLS handshake timeout on one
#: of five is ordinary rather than exceptional. Retried because a network hiccup
#: aborting a release check trains people to ignore the tool, and a checker
#: nobody trusts is worse than no checker.
DOWNLOAD_ATTEMPTS = 3


def download_release(tag: str, repo: str, dest: Path) -> tuple[bool, str]:
    """Fetch a published release's tarballs and SHA256SUMS via `gh`.

    Retries transient network failures. `--clobber` makes each attempt
    idempotent, and assets already fetched are re-verified against SHA256SUMS
    later regardless, so a partial download from a failed attempt cannot be
    mistaken for a good one.
    """
    if shutil.which("gh") is None:
        return False, "the `gh` CLI is required to download a release"
    dest.mkdir(parents=True, exist_ok=True)

    last = ""
    attempt = 0
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        try:
            p = subprocess.run(
                ["gh", "release", "download", tag, "--repo", repo,
                 # `.bundle` explicitly: `*.tar.gz` does not match
                 # `travsr-<tag>-<target>.tar.gz.bundle`, so without this the
                 # Sigstore bundles never land and signature verification has
                 # nothing to check.
                 "--pattern", "*.tar.gz", "--pattern", "*.bundle",
                 "--pattern", "SHA256SUMS", "--clobber"],
                cwd=dest, capture_output=True, text=True, timeout=1800,
            )
        except subprocess.TimeoutExpired:
            # Uncaught, this escaped download_release and out of resolve_binary as
            # a traceback, breaking the documented "error: ... / exit 2" contract
            # at the moment the operator most needs a readable message.
            last = "gh release download timed out after 1800s"
            if attempt < DOWNLOAD_ATTEMPTS:
                print(f"  download attempt {attempt} timed out, retrying")
                time.sleep(3 * attempt)
            continue
        if p.returncode == 0:
            return True, ""
        last = p.stderr.strip()
        # A missing or misspelled tag will never succeed, so do not spend three
        # attempts on it.
        if "release not found" in last.lower() or "not found" in last.lower():
            break
        if attempt < DOWNLOAD_ATTEMPTS:
            # `gh` can exit non-zero having written nothing to stderr (killed by a
            # signal, or an error on stdout). Indexing [-1] of an empty list then
            # raised IndexError from inside the retry handler, so the suite died
            # with a traceback instead of its readable failure path.
            tail = last.splitlines()[-1] if last else "(no stderr)"
            print(f"  download attempt {attempt} failed, retrying: {tail[:120]}")
            time.sleep(3 * attempt)
    return False, f"gh release download failed after {attempt} attempt(s): {last[:400]}"


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_sums(dest: Path) -> tuple[list[str], list[str]]:
    """Returns (verified, problems) against SHA256SUMS."""
    sums = dest / "SHA256SUMS"
    if not sums.exists():
        return [], ["SHA256SUMS not present in the release"]
    verified, problems = [], []
    listed: set[str] = set()
    for line in sums.read_text().splitlines():
        if not line.strip():
            continue
        parts = line.split()
        if len(parts) != 2:
            # Not silent. A line we cannot parse is a line we did not check, and
            # a filename containing a space lands here too, so dropping it
            # quietly is how an unchecked artifact reads as verified.
            problems.append(f"SHA256SUMS: cannot parse line {line.strip()!r}")
            continue
        want, name = parts[0], Path(parts[1]).name
        listed.add(name)
        f = dest / name
        if not f.exists():
            problems.append(f"{name}: listed in SHA256SUMS but not downloaded")
            continue
        got = sha256(f)
        if got == want:
            verified.append(name)
        else:
            problems.append(f"{name}: sha256 {got[:12]} != recorded {want[:12]}")
    # Reconciled the other way too. Driving only off SHA256SUMS asks "is every
    # listed file intact", which a release that shipped a tarball with no
    # checksum line answers yes to: the file is simply absent from the loop.
    # `check_signatures` makes the same argument about bundles, and the blind
    # spot is identical here (#742 review).
    for tgz in sorted(dest.glob("*.tar.gz")):
        if tgz.name not in listed:
            problems.append(f"{tgz.name}: published but absent from SHA256SUMS")

    return verified, problems


def extract(tarball: Path, dest: Path) -> Optional[Path]:
    """Extract a release tarball and return the `travsr` binary inside it."""
    dest.mkdir(parents=True, exist_ok=True)
    with tarfile.open(tarball) as tf:
        # `filter="data"` is the interpreter's own extraction guard: it rejects
        # absolute paths, parent traversal, device nodes, and crucially link
        # targets that point outside the destination.
        #
        # Checking member names alone was not enough, which is what the previous
        # version did. A symlink member named `travsr` with a linkname of
        # `../../../etc/passwd` passes a name check, and extraction then creates a
        # link out of the temp dir that a later member, or `extract`'s own
        # rglob("travsr") below, follows.
        try:
            tf.extractall(dest, filter="data")
        except TypeError:
            # `filter=` landed in 3.12. On older interpreters, validate names and
            # link targets by hand rather than silently unpacking unchecked.
            for m in tf.getmembers():
                for candidate in (m.name, m.linkname or ""):
                    if not candidate:
                        continue
                    cp = Path(candidate)
                    if cp.is_absolute() or ".." in cp.parts:
                        raise ValueError(
                            f"unsafe path in {tarball.name}: {m.name} -> {candidate}"
                        )
                if m.isdev():
                    raise ValueError(f"device node in {tarball.name}: {m.name}")
            tf.extractall(dest)
    for name in ("travsr", "travsr.exe"):
        found = sorted(dest.rglob(name))
        if found:
            return found[0]
    return None


