#!/usr/bin/env python3
"""The fixture repository the suite indexes.

Each language contributes one intra-file call and, where it matters, one
cross-file call, so `references` and `graph` have a known-correct answer to be
checked against rather than merely a non-empty one.
"""

import json
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class Symbol:
    """A symbol with a known definition site and a known use site."""

    name: str
    language: str
    #: Repo-relative file the definition must resolve to.
    def_file: str
    #: Repo-relative file the reference must be reported in.
    ref_file: str
    #: True when the analyzer needs a compile_commands.json and a trust grant.
    needs_compdb: bool = False


FILES: dict[str, str] = {
    "src/payment.ts": """\
export interface Charge { amountCents: number; currency: string; }

export class Ledger {
  private rows: Charge[] = [];
  record(c: Charge): void { this.rows.push(c); }
  total(): number { return this.rows.reduce((a, r) => a + r.amountCents, 0); }
}

export class PaymentService {
  constructor(private ledger: Ledger) {}
  charge(c: Charge): boolean {
    if (!validateCharge(c)) return false;
    this.ledger.record(c);
    return true;
  }
}

export function validateCharge(c: Charge): boolean {
  return c.amountCents > 0 && c.currency.length === 3;
}
""",
    # Cross-file: runCheckout -> PaymentService.charge. The only check here that
    # needs an edge between two files, which is the one Phase A alone cannot give.
    "src/checkout.ts": """\
import { PaymentService, Ledger, Charge } from "./payment";

export function runCheckout(amount: number): boolean {
  const svc = new PaymentService(new Ledger());
  const charge: Charge = { amountCents: amount, currency: "usd" };
  return svc.charge(charge);
}
""",
    # .mjs deliberately: ES module resolution was the known gap for javascript.
    "src/util.mjs": """\
export function computeTax(cents) { return Math.round(cents * 0.2); }
export function applyTax(cents) { return cents + computeTax(cents); }
""",
    "src/report.py": """\
class ReportBuilder:
    def __init__(self, rows):
        self.rows = rows

    def build_summary(self):
        return summarize_rows(self.rows)


def summarize_rows(rows):
    return {"count": len(rows), "total": sum(rows)}
""",
    "src/retry.rs": """\
pub struct RetryPolicy { pub max_attempts: u32 }

impl RetryPolicy {
    pub fn new(max_attempts: u32) -> Self { Self { max_attempts } }
    pub fn should_retry(&self, attempt: u32) -> bool { attempt < self.max_attempts }
}

pub fn run_with_retry(policy: &RetryPolicy) -> bool {
    let mut attempt = 0;
    while policy.should_retry(attempt) { attempt += 1; }
    true
}
""",
    "src/buffer.c": """\
#include <stddef.h>

size_t buffer_capacity(size_t used, size_t total) { return total - used; }

int buffer_has_room(size_t used, size_t total) {
    return buffer_capacity(used, total) > 0;
}
""",
    # A private static member function, which is the shape that exposed the
    # class-template and header-parsing gaps during the language work.
    "src/engine.cpp": """\
class Engine {
public:
  explicit Engine(int seed) : seed_(seed) {}
  int next() { return advance(seed_); }

private:
  static int advance(int s) { return s * 1103515245 + 12345; }
  int seed_;
};

int run_engine(int seed) { Engine e(seed); return e.next(); }
""",
}

SYMBOLS: list[Symbol] = [
    Symbol("validateCharge", "typescript", "src/payment.ts", "src/payment.ts"),
    Symbol("computeTax", "javascript", "src/util.mjs", "src/util.mjs"),
    Symbol("summarize_rows", "python", "src/report.py", "src/report.py"),
    Symbol("should_retry", "rust", "src/retry.rs", "src/retry.rs"),
    Symbol("buffer_capacity", "c", "src/buffer.c", "src/buffer.c", needs_compdb=True),
    Symbol("advance", "cpp", "src/engine.cpp", "src/engine.cpp", needs_compdb=True),
]

#: Cross-file caller that `graph <callee> --direction both` must surface.
CROSS_FILE_CALLER = "runCheckout"
CROSS_FILE_CALLEE = "PaymentService"


class Fixture:
    """A git repo on disk holding the fixture sources."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def build(self) -> None:
        (self.root / "src").mkdir(parents=True, exist_ok=True)
        # .travsr/ is ignored from the start. Committing the graph database is a
        # real hazard (`travsr init` adds no ignore entry of its own), and letting
        # it happen here would muddy every staleness assertion the suite makes.
        (self.root / ".gitignore").write_text(".travsr/\n")
        for rel, body in FILES.items():
            (self.root / rel).write_text(body)
        self._git("init", "-q")
        self._git("config", "user.email", "sanity@travsr.test")
        self._git("config", "user.name", "travsr sanity")
        self.commit("seed the fixture")

    def commit(self, message: str) -> str:
        self._git("add", "-A")
        self._git("commit", "-qm", message)
        return self._git("rev-parse", "--short", "HEAD").strip()

    def append(self, rel: str, text: str) -> None:
        with (self.root / rel).open("a") as f:
            f.write(text)

    def write_compdb(self) -> bool:
        """Emit a compile_commands.json for the C/C++ sources.

        The sysroot flag is not optional on macOS: without it scip-clang cannot
        resolve system headers, and the resulting miss is indistinguishable from
        an upstream analyzer bug. That misdiagnosis cost real time once already,
        so the fixture supplies it rather than leaving it to the environment.
        """
        cc = shutil.which("clang")
        if cc is None:
            return False
        sysroot = ""
        if shutil.which("xcrun"):
            p = subprocess.run(["xcrun", "--show-sdk-path"], capture_output=True, text=True)
            if p.returncode == 0 and p.stdout.strip():
                sysroot = f"-isysroot {p.stdout.strip()} "
        entries = [
            {
                "directory": str(self.root),
                "file": str(self.root / "src/buffer.c"),
                "command": f"clang {sysroot}-c src/buffer.c -o /dev/null",
            },
            {
                "directory": str(self.root),
                "file": str(self.root / "src/engine.cpp"),
                "command": f"clang++ -std=c++17 {sysroot}-c src/engine.cpp -o /dev/null",
            },
        ]
        (self.root / "compile_commands.json").write_text(json.dumps(entries, indent=2) + "\n")
        return True

    def _git(self, *args: str) -> str:
        p = subprocess.run(
            ["git", *args], cwd=self.root, capture_output=True, text=True, timeout=120
        )
        if p.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} failed: {p.stderr.strip()}")
        return p.stdout
