#!/usr/bin/env python3
"""Phase B edge-correctness audit — the measurement harness for the W1-W4 epic.

Plan: docs/plans/phase-b-edge-correctness-epic.md
      docs/plans/issue-529-method-call-receiver-resolution.md

Every acceptance number in those plans comes from this script. Run it against a
graph.db before and after each workstream:

    python3 bench/phase-b-edge-audit.py [path/to/graph.db] [--compare other.db]

Sections map to workstreams:
  [W1] provenance + language distribution      (must become truthful)
  [W3] dangling-endpoint rate, per language    (must become 0)
  [W4] #529 false ref/call edges               (must drop; W1 must not move it)

The #529 false-edge metric uses a deliberately conservative proxy: an edge into
`fn:T.method` is counted false when the token `T` never appears anywhere in the
caller's own source file. It under-counts (a genuine cross-file edge whose type
is merely mentioned reads as true), so reported false counts are LOWER BOUNDS.
Real ground truth needs rust-analyzer LSIF — see epic §6, W3b.
"""

import collections
import os
import re
import sqlite3
import sys


def open_db(path):
    if not os.path.exists(path):
        sys.exit(f"no such db: {path}")
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def w1_provenance(db):
    print("\n[W1] ref/call by provenance / stored edge language")
    for prov, lang, n in db.execute(
        "SELECT provenance, language, COUNT(*) FROM edges "
        "WHERE kind='ref/call' GROUP BY 1,2 ORDER BY 3 DESC"
    ):
        print(f"      {n:8d}  provenance={prov:<12s} language={lang}")
    print("\n      all edges by stored language (bug: always the 'typescript' default)")
    for lang, n in db.execute("SELECT language, COUNT(*) FROM edges GROUP BY 1 ORDER BY 2 DESC"):
        print(f"      {n:8d}  {lang}")
    print("\n      nodes by language, for contrast")
    for lang, n in db.execute(
        "SELECT language, COUNT(*) FROM nodes GROUP BY 1 ORDER BY 2 DESC LIMIT 6"
    ):
        print(f"      {n:8d}  {lang}")


def w3_dangling(db):
    print("\n[W3] dangling ref/call endpoints (dst node does not exist)")
    row = db.execute(
        "SELECT SUM(ns.id IS NULL), SUM(nd.id IS NULL), COUNT(*) FROM edges e "
        "LEFT JOIN nodes ns ON ns.id=e.src LEFT JOIN nodes nd ON nd.id=e.dst "
        "WHERE e.kind='ref/call'"
    ).fetchone()
    src_missing, dst_missing, total = row[0] or 0, row[1] or 0, row[2]
    pct = 100.0 * dst_missing / total if total else 0.0
    print(f"      total={total}  src_missing={src_missing}  dst_missing={dst_missing} ({pct:.1f}%)")

    print("\n      by SOURCE-node language (edges.language is unusable until W1)")
    print(f"      {'language':<14s} {'dangling':>9s} {'total':>8s} {'pct':>7s}")
    for lang, dang, tot in db.execute(
        "SELECT COALESCE(ns.language,'<none>'), SUM(nd.id IS NULL), COUNT(*) FROM edges e "
        "JOIN nodes ns ON ns.id=e.src LEFT JOIN nodes nd ON nd.id=e.dst "
        "WHERE e.kind='ref/call' GROUP BY 1 ORDER BY 3 DESC"
    ):
        print(f"      {lang:<14s} {dang or 0:>9d} {tot:>8d} {100.0*(dang or 0)/tot:>6.1f}%")

    print("\n      by SOURCE-node kind — 'file' callers are the LSIF ingest defect")
    for kind, n in db.execute(
        "SELECT COALESCE(ns.kind,'<MISSING>'), COUNT(*) FROM edges e "
        "LEFT JOIN nodes ns ON ns.id=e.src WHERE e.kind='ref/call' "
        "GROUP BY 1 ORDER BY 2 DESC LIMIT 6"
    ):
        print(f"      {n:8d}  {kind}")

    n_sites = db.execute("SELECT COUNT(*) FROM edge_sites WHERE kind='ref/call'").fetchone()[0]
    print(f"\n      edge_sites ref/call rows: {n_sites}   (W3b must make this grow)")


def w4_false_edges(db, repo_root):
    print("\n[W4] #529 false ref/call edges into unique-leaf qualified methods")
    rows = db.execute(
        "SELECT id, signature, path FROM nodes "
        "WHERE kind IN ('function','method') AND signature LIKE 'fn:%.%'"
    ).fetchall()
    by_leaf = collections.defaultdict(list)
    for nid, sig, path in rows:
        by_leaf[sig.split(".")[-1]].append((nid, sig, path))
    uniq = {leaf: v[0] for leaf, v in by_leaf.items() if len(v) == 1}
    print(
        f"      qualified nodes={len(rows)}  distinct leaves={len(by_leaf)}  "
        f"globally-unique leaves={len(uniq)}"
    )

    src_cache = {}

    def source(rel):
        if rel not in src_cache:
            try:
                with open(os.path.join(repo_root, rel), encoding="utf8", errors="replace") as fh:
                    src_cache[rel] = fh.read()
            except OSError:
                src_cache[rel] = ""
        return src_cache[rel]

    total_in = suspect = 0
    offenders = []
    for _leaf, (nid, sig, npath) in uniq.items():
        recv = sig[3:].rsplit(".", 1)[0]
        callers = db.execute(
            "SELECT c.path FROM edges e JOIN nodes c ON c.id=e.src "
            "WHERE e.dst=? AND e.kind='ref/call'",
            (nid,),
        ).fetchall()
        if not callers:
            continue
        pat = re.compile(r"\b" + re.escape(recv) + r"\b")
        bad = sum(1 for (cpath,) in callers if not pat.search(source(cpath)))
        total_in += len(callers)
        suspect += bad
        if bad:
            offenders.append((bad, len(callers), sig, npath))

    pct = 100.0 * suspect / total_in if total_in else 0.0
    print(f"      in-edges into that set: {total_in}")
    print(f"      receiver type absent from caller file (FALSE, lower bound): {suspect} ({pct:.1f}%)")
    offenders.sort(reverse=True)
    print("\n      top offenders (false/total)")
    for bad, tot, sig, path in offenders[:12]:
        print(f"      {bad:5d}/{tot:<5d}  {sig}  ({path})")
    return {"false": suspect, "in_edges": total_in}


def totals(db):
    n = db.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]
    e = db.execute("SELECT COUNT(*) FROM edges").fetchone()[0]
    rc = db.execute("SELECT COUNT(*) FROM edges WHERE kind='ref/call'").fetchone()[0]
    return n, e, rc


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    db_path = args[0] if args else ".travsr/graph.db"
    compare = None
    if "--compare" in sys.argv:
        compare = sys.argv[sys.argv.index("--compare") + 1]
    repo_root = os.environ.get("TRAVSR_REPO_ROOT", os.getcwd())

    db = open_db(db_path)
    n, e, rc = totals(db)
    print(f"=== {db_path}")
    print(f"    nodes={n}  edges={e}  ref/call={rc}")
    w1_provenance(db)
    w3_dangling(db)
    w4_false_edges(db, repo_root)

    if compare:
        cdb = open_db(compare)
        cn, ce, crc = totals(cdb)
        print(f"\n=== compare: {compare}")
        print(f"    nodes={cn}  edges={ce}  ref/call={crc}")
        print(f"    delta: nodes {n-cn:+d}  edges {e-ce:+d}  ref/call {rc-crc:+d}")
        w3_dangling(cdb)
        w4_false_edges(cdb, repo_root)


if __name__ == "__main__":
    main()
