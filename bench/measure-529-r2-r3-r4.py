#!/usr/bin/env python3
"""#529 Phase 0 gates R-2/R-3/R-4 (plan §6.1) — companion to phase-b-edge-audit.py.

R-1 (baseline false-edge count) is covered by phase-b-edge-audit.py's [W4]
section. This script covers the three gates that need the REAL AST-scoped
resolver's output, produced by the ignored Rust dump tool:

    cargo test -p travsr-analysis --release -- --ignored \
        dump_recv_type_measurement_529 --nocapture

which writes bench/_529-recv-dump.tsv (path, line, method, recv_kind,
recv_multiline, recv_type) for every method-call site in this repo's own
Rust source, using the exact same CALL_QUERY + resolve_receiver_type path
extraction will use in production.

Usage: python3 bench/measure-529-r2-r3-r4.py [.travsr/graph.db] [dump.tsv]
"""

import collections
import csv
import os
import re
import sqlite3
import sys


def load_dump(path):
    rows = []
    with open(path, newline="", encoding="utf8") as fh:
        reader = csv.DictReader(fh, delimiter="\t")
        for r in reader:
            r["recv_multiline"] = r["recv_multiline"] == "True"
            rows.append(r)
    return rows


def r2_chain_continuation_artifact(rows):
    print("\n[R-2] validate the chain-continuation artifact claim")
    multiline = [r for r in rows if r["recv_multiline"]]
    if not multiline:
        print("      no multi-line-receiver call sites found")
        return True
    by_kind = collections.Counter(r["recv_kind"] for r in multiline)
    ident = by_kind.get("identifier", 0)
    pct = 100.0 * ident / len(multiline)
    print(f"      multi-line-receiver call sites: {len(multiline)}")
    for kind, n in by_kind.most_common():
        print(f"        {n:6d}  recv_kind={kind}")
    print(f"      identifier-shaped (T1-eligible): {ident} ({pct:.1f}%)")
    ok = pct >= 50.0
    print(f"      GATE: {'PASS' if ok else 'FAIL'} (expect the majority to be plain identifiers)")
    return ok


def load_qualified_nodes(db):
    rows = db.execute(
        "SELECT id, signature, path FROM nodes "
        "WHERE kind IN ('function','method') AND signature LIKE 'fn:%.%'"
    ).fetchall()
    by_leaf = collections.defaultdict(list)
    sig_set = set()
    for nid, sig, path in rows:
        sig_set.add(sig)
        by_leaf[sig.split(".")[-1]].append((nid, sig, path))
    return sig_set, by_leaf


def load_known_graph_types(db):
    rows = db.execute(
        "SELECT signature FROM nodes WHERE kind IN ('struct','enum','trait')"
    ).fetchall()
    return {sig.split(":", 1)[1] for (sig,) in rows if ":" in sig}


def r3_recall_win(rows, sig_set, by_leaf):
    print("\n[R-3] cost the recall win (ambiguous-leaf calls that resolve exactly now)")
    ambiguous_leaves = {leaf for leaf, v in by_leaf.items() if len(v) > 1}
    wins = 0
    for r in rows:
        t = r["recv_type"]
        if not t:
            continue
        leaf = r["method"]
        if leaf not in ambiguous_leaves:
            continue
        qsig = f"fn:{t}.{leaf}"
        if qsig in sig_set:
            wins += 1
    print(f"      distinct ambiguous leaves in graph: {len(ambiguous_leaves)}")
    print(f"      call sites into an ambiguous leaf that now resolve exactly: {wins}")
    print("      (today: 0 edges for every one of these — #521 CO-A1 drops all ambiguous-leaf calls)")
    return wins


def r4_wrong_resolution_risk(rows, sig_set, by_leaf, known_types, repo_root):
    print("\n[R-4] bound the risk (real edges branch 2 would wrongly delete)")
    src_cache = {}

    def source(rel):
        if rel not in src_cache:
            try:
                with open(os.path.join(repo_root, rel), encoding="utf8", errors="replace") as fh:
                    src_cache[rel] = fh.read()
            except OSError:
                src_cache[rel] = ""
        return src_cache[rel]

    risky = []
    for r in rows:
        t = r["recv_type"]
        if not t:
            continue
        leaf = r["method"]
        qsig = f"fn:{t}.{leaf}"
        if qsig in sig_set:
            continue  # branch 1: resolves exactly, no risk
        if t in known_types:
            continue  # branch 3: falls through unchanged, no risk
        # branch 2 would fire (T not a graph type at all) — would this call
        # site actually resolve to something real today (leaf-unique-match)?
        candidates = by_leaf.get(leaf, [])
        if len(candidates) != 1:
            continue  # today's CO-A1 gate already drops this — no regression
        nid, sig, path = candidates[0]
        real_type = sig[3:].rsplit(".", 1)[0]
        caller_src = source(r["path"])
        if re.search(r"\b" + re.escape(real_type) + r"\b", caller_src):
            risky.append((r["path"], r["line"], leaf, t, real_type))

    print(f"      candidate wrong-resolution sites: {len(risky)}")
    for path, line, leaf, wrong_t, real_t in risky[:20]:
        print(f"        {path}:{line}  .{leaf}()  resolved={wrong_t!r} actual={real_t!r}")
    ok = len(risky) <= 10
    print(f"      GATE: {'PASS' if ok else 'FAIL'} (require <= 10)")
    return risky


def main():
    db_path = sys.argv[1] if len(sys.argv) > 1 else ".travsr/graph.db"
    dump_path = sys.argv[2] if len(sys.argv) > 2 else "bench/_529-recv-dump.tsv"
    repo_root = os.environ.get("TRAVSR_REPO_ROOT", os.getcwd())

    if not os.path.exists(db_path):
        sys.exit(f"no such db: {db_path}")
    if not os.path.exists(dump_path):
        sys.exit(
            f"no such dump: {dump_path}\n"
            "run: cargo test -p travsr-analysis --release -- --ignored "
            "dump_recv_type_measurement_529 --nocapture"
        )

    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = load_dump(dump_path)
    sig_set, by_leaf = load_qualified_nodes(db)
    known_types = load_known_graph_types(db)

    print(f"=== {dump_path}  ({len(rows)} method-call sites, "
          f"{sum(1 for r in rows if r['recv_type'])} with recv_type resolved)")

    r2_ok = r2_chain_continuation_artifact(rows)
    r3_recall_win(rows, sig_set, by_leaf)
    risky = r4_wrong_resolution_risk(rows, sig_set, by_leaf, known_types, repo_root)

    print("\n=== summary")
    print(f"      R-2 (chain-continuation is mostly identifier-shaped): {'PASS' if r2_ok else 'FAIL'}")
    print(f"      R-4 (wrong-resolution risk <= 10): {'PASS' if len(risky) <= 10 else 'FAIL'} ({len(risky)} found)")


if __name__ == "__main__":
    main()
