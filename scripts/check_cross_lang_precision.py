#!/usr/bin/env python3
"""Cross-language FFI edge precision gate.

Reads a graph JSON (produced by `travsr index --output`) and a hand-authored
golden expected_edges.json, then computes per-pair precision and fails if
any pair is below the threshold.

Exit code 0 = all pairs pass. Exit code 1 = at least one pair fails.
"""

import argparse
import json
import sys
from pathlib import Path


def load_json(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def edge_key(edge: dict) -> tuple:
    """Stable key for matching emitted edges against golden entries."""
    src = edge.get("src", {})
    dst = edge.get("dst", {})
    return (
        src.get("language", ""),
        src.get("signature", ""),
        dst.get("language", ""),
        dst.get("signature", ""),
    )


def compute_precision(emitted_edges: list, golden_edges: list, min_confidence: int) -> tuple:
    """Return (precision, matched_count, emitted_count, missing, spurious)."""
    # Filter emitted to FFICall edges above threshold
    ffi_emitted = [
        e for e in emitted_edges
        if e.get("kind") == "ffi/call"
        and e.get("confidence", 0) >= min_confidence
    ]

    golden_keys = set()
    for g in golden_edges:
        key = (
            g["src"].get("language", ""),
            g["src"].get("signature", ""),
            g["dst"].get("language", ""),
            g["dst"].get("signature", ""),
        )
        golden_keys.add(key)

    matched = []
    spurious = []
    for e in ffi_emitted:
        k = edge_key(e)
        if k in golden_keys:
            matched.append(e)
        else:
            spurious.append(e)

    missing = [g for g in golden_edges
               if (g["src"].get("language",""), g["src"].get("signature",""),
                   g["dst"].get("language",""), g["dst"].get("signature",""))
               not in {edge_key(e) for e in matched}]

    total = len(ffi_emitted)
    precision = len(matched) / total if total > 0 else 0.0
    return precision, len(matched), total, missing, spurious


def main():
    parser = argparse.ArgumentParser(description="Cross-language FFI precision gate")
    parser.add_argument("--graph", required=True, help="Path to graph JSON from travsr index")
    parser.add_argument("--golden", required=True, help="Path to expected_edges.json")
    parser.add_argument("--min-precision", type=float, default=0.90)
    args = parser.parse_args()

    graph = load_json(args.graph)
    golden = load_json(args.golden)

    all_emitted = graph.get("edges", [])
    exit_code = 0

    print(f"\n{'='*60}")
    print(f"Cross-Language Precision Gate (threshold: {args.min_precision:.0%})")
    print(f"{'='*60}\n")

    for pair_spec in golden.get("pairs", []):
        pair_name = pair_spec["pair"]
        fixture_root = pair_spec["fixture_root"]
        golden_edges = pair_spec["edges"]

        if pair_spec.get("xfail"):
            print(f"Pair: {pair_name}")
            print(f"  Status:    [SKIP] (xfail — {pair_spec.get('xfail_reason', 'no reason given')})")
            print()
            continue

        # Filter emitted edges to this fixture root
        pair_emitted = [
            e for e in all_emitted
            if fixture_root in e.get("src", {}).get("path", "")
            or fixture_root in e.get("dst", {}).get("path", "")
        ]

        precision, matched, total, missing, spurious = compute_precision(
            pair_emitted, golden_edges, min_confidence=30
        )

        status = "PASS" if precision >= args.min_precision and total > 0 else "FAIL"
        if status == "FAIL":
            exit_code = 1

        print(f"Pair: {pair_name}")
        print(f"  Status:    [{status}]")
        print(f"  Precision: {precision:.1%}  ({matched}/{total} emitted matched golden)")
        print(f"  Required:  ≥{args.min_precision:.0%}")

        if spurious:
            print(f"\n  Spurious edges (emitted but not in golden):")
            for e in spurious[:5]:  # cap output
                src = e.get("src", {})
                dst = e.get("dst", {})
                conf = e.get("confidence", "?")
                print(f"    - {src.get('language')}:{src.get('signature')} -> "
                      f"{dst.get('language')}:{dst.get('signature')}  conf={conf}")
                hint = "in negative/ dir" if "negative" in src.get("path","") else ""
                if hint:
                    print(f"      Hint: {hint}")

        if missing:
            print(f"\n  Missing edges (in golden but not emitted):")
            for g in missing[:5]:
                print(f"    - {g['src'].get('language')}:{g['src'].get('signature')} -> "
                      f"{g['dst'].get('language')}:{g['dst'].get('signature')}  [{g.get('tag','')}]")

        if total == 0:
            print(f"  WARNING: no FFICall edges emitted for this pair — resolver may not be wired")
            exit_code = 1

        print()

    print(f"{'='*60}")
    if exit_code == 0:
        print("RESULT: All pairs passed ✓")
    else:
        print("RESULT: One or more pairs FAILED — see details above")
    print(f"{'='*60}\n")

    sys.exit(exit_code)


if __name__ == "__main__":
    main()
