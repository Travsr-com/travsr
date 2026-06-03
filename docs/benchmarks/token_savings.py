#!/usr/bin/env python3
"""
Token-savings measurement: graph-gated discovery vs grep/read-only baseline.

For each target symbol we answer the SAME structural-discovery question —
"what are the callers, dependencies, and blast radius of <symbol>?" — two ways,
and count the tokens that would actually land in an AI agent's context window.

  Arm A (graph-gated):   travsr ask "<nl>"  +  travsr graph <sym> --direction both
                         The graph output IS the structural answer; no file reads.
  Arm B (grep + read):   grep -rn <sym>  +  read every distinct file containing a hit.
                         A grep-driven agent must read each hit file to classify the
                         occurrence (call? def? comment?) and reconstruct the structure.

Assumptions (stated, so the number is honest):
  * Baseline reads WHOLE files containing a hit. Inflates baseline (agents sometimes
    read slices) but also UNDER-counts it (no transitive caller-of-caller following,
    which a real blast-radius task requires). Net: a conservative midpoint.
  * Token counts via tiktoken cl100k_base — a widely-cited proxy for LLM tokenization
    (not Anthropic's exact tokenizer, but stable and reproducible).
"""
import os
import subprocess
import sys

try:
    import tiktoken
    _enc = tiktoken.get_encoding("cl100k_base")
    def ntok(s: str) -> int: return len(_enc.encode(s))
    TOKENIZER = "tiktoken/cl100k_base"
except Exception:
    def ntok(s: str) -> int: return (len(s) + 3) // 4
    TOKENIZER = "approx chars/4 (tiktoken unavailable)"

REPO = "/Users/ak/Desktop/Proj/travsr"
TRAVSR = os.path.join(REPO, "target/release/travsr")
ENV = {**os.environ, "TRAVSR_DISABLE_REGISTRY": "1"}

# (symbol_for_graph, natural_language_for_ask)
SYMBOLS = [
    ("search_nodes_fuzzy",     "search nodes fuzzy"),
    ("dispatch_tool_call",     "mcp dispatch tool call"),
    ("knapsack",               "knapsack budget"),
    ("get_graph_json",         "get graph json"),
    ("backfill_fts_if_needed", "fts backfill needed"),
]


def run(cmd):
    r = subprocess.run(cmd, cwd=REPO, env=ENV, capture_output=True, text=True)
    
    return r.stdout + r.stderr


def arm_graph(sym, nl):
    out = run([TRAVSR, "ask", nl])
    out += run([TRAVSR, "graph", sym, "--direction", "both", "--depth", "2"])
    return ntok(out)


WINDOW = 25  # lines of context a "surgical" grep-agent reads around each hit


def arm_grep(sym):
    """Returns (whole_file_tokens, slice_tokens, n_files).

    whole_file = upper baseline (read every hit file fully).
    slice      = lower baseline (read only +/-WINDOW lines, merged, around hits) —
                 a 'good' grep-agent. Note: neither follows transitive callers, so
                 both UNDER-count a true blast-radius task.
    """
    hits = run(["grep", "-rn", "--include=*.rs", "--include=*.ts",
                sym, "crates", "packages"])
    by_file = {}
    for ln in hits.splitlines():
        parts = ln.split(":", 2)
        if len(parts) < 3 or not parts[1].isdigit():
            continue
        by_file.setdefault(parts[0], []).append(int(parts[1]))

    whole = slice_ = hits
    for rel, linenos in by_file.items():
        p = os.path.join(REPO, rel)
        try:
            with open(p, encoding="utf-8", errors="replace") as fh:
                lines = fh.readlines()
        except OSError:
            continue
        whole += "".join(lines)
        keep = set()
        for n in linenos:                       # n is 1-based
            keep.update(range(max(0, n - 1 - WINDOW), min(len(lines), n + WINDOW)))
        slice_ += "".join(lines[i] for i in sorted(keep))
    return ntok(whole), ntok(slice_), len(by_file)


def main():
    print(f"Tokenizer: {TOKENIZER}")
    print(f"Repo:      {REPO}")
    print(f"Baselines: whole = read full hit files | slice = +/-{WINDOW} lines around hits\n")
    hdr = (f"{'symbol':<24}{'graph':>8}{'slice':>9}{'whole':>9}{'files':>6}"
           f"{'vs slice':>10}{'vs whole':>10}")
    print(hdr)
    print("-" * len(hdr))

    sg = ss = sw = 0
    for sym, nl in SYMBOLS:
        g = arm_graph(sym, nl)
        whole, sliced, nfiles = arm_grep(sym)
        sg += g; ss += sliced; sw += whole
        sv_s = (1 - g / sliced) * 100 if sliced else 0.0
        sv_w = (1 - g / whole) * 100 if whole else 0.0
        print(f"{sym:<24}{g:>8,}{sliced:>9,}{whole:>9,}{nfiles:>6}"
              f"{sv_s:>9.0f}%{sv_w:>9.0f}%")

    print("-" * len(hdr))
    a_s = (1 - sg / ss) * 100 if ss else 0.0
    a_w = (1 - sg / sw) * 100 if sw else 0.0
    print(f"{'TOTAL':<24}{sg:>8,}{ss:>9,}{sw:>9,}{'':>6}{a_s:>9.0f}%{a_w:>9.0f}%")
    print(f"\nToken reduction (graph-gated): {a_s:.0f}% vs surgical grep-agent, "
          f"{a_w:.0f}% vs whole-file reads")


if __name__ == "__main__":
    sys.exit(main())
