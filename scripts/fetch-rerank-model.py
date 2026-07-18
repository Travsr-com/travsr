#!/usr/bin/env python3
"""Fetch + fix up the RFC-021 cross-encoder model for travsr-rerank.

Produces model_fp16.onnx + tokenizer.json in --out, ready for
TractReranker::load() / TRAVSR_RERANK_TEST_MODEL_DIR. Not run automatically;
the model is a release-time asset (RFC-021 Phase 5), not vendored in git.

Requires: pip install onnx onnxconverter_common (no torch needed - we start
from an already-exported fp32 ONNX graph, not a PyTorch checkpoint).

Why the fp16 conversion isn't a plain `onnx -> fp16` cast:
  1. The upstream pre-exported model_fp16.onnx (Xenova/ms-marco-MiniLM-L-6-v2)
     fails to load in tract 0.21: the pooler's Gemm node ends up with
     mismatched F16/F32 facts during graph optimization.
  2. Converting our own fp16 from the fp32 graph with the 2 Gemm nodes
     (pooler dense + classifier head, ~590KB total) excluded from the fp16
     cast avoids that - those nodes stay F32 with auto-inserted bridging
     Cast ops, everything else is F16.
  3. That still leaves one manually-authored Cast node
     (/bert/Cast, int64 attention_mask -> float for the extended-attention-
     mask trick) whose `to` attribute onnxconverter_common leaves as FLOAT
     even though every downstream consumer became F16. tract's analyser
     rejects the resulting F16/F32 conflict. Patched by hand below - it's a
     single attribute flip, numerically inert (the mask arithmetic already
     tolerates fp16 range: the -10000.0 sentinel and 1e-12 epsilons in this
     graph get fp16-truncated regardless, standard practice for fp16
     transformer inference).

Do NOT extend op_block_list to also exclude "Cast" as a way to dodge step 3
generically - that was tried and made onnxconverter_common pathologically
slow (20+ min, never finished) on this graph. The targeted attribute patch
is fast and exact.
"""
from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

REPO = "Xenova/ms-marco-MiniLM-L-6-v2"
BASE_URL = f"https://huggingface.co/{REPO}/resolve/main"
TOKENIZER_FILES = ["tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "vocab.txt", "config.json"]
FP32_ONNX_PATH = "onnx/model.onnx"

# The one node whose `to` attribute must be patched from FLOAT to FLOAT16
# after conversion (see module docstring, point 3).
PATCH_NODE_NAME = "/bert/Cast"


def download(url: str, dest: Path) -> None:
    # Shells out to curl rather than urllib: some Python installs (notably
    # python.org's macOS build) ship without a configured CA bundle, which
    # makes urllib fail TLS verification out of the box. curl uses the
    # system trust store and just works.
    print(f"downloading {url} -> {dest}")
    subprocess.run(["curl", "-sL", "--fail", "--max-time", "60", "-o", str(dest), url], check=True)


def convert_and_patch(fp32_path: Path, out_path: Path) -> None:
    import onnx
    from onnx import TensorProto
    from onnxconverter_common import float16

    model = onnx.load(str(fp32_path))
    converted = float16.convert_float_to_float16(
        model,
        keep_io_types=True,
        op_block_list=["Gemm"],
    )

    patched = 0
    target_output = None
    for node in converted.graph.node:
        if node.name == PATCH_NODE_NAME:
            for attr in node.attribute:
                if attr.name == "to":
                    attr.i = TensorProto.FLOAT16
                    patched += 1
            target_output = node.output[0]
    if patched != 1:
        raise RuntimeError(
            f"expected to patch exactly 1 node named {PATCH_NODE_NAME!r}, patched {patched}. "
            "The upstream ONNX export likely changed - re-diagnose with tract's error message "
            "rather than assuming this script is still correct."
        )

    # Drop the stale (pre-patch) value_info hint for that node's output so
    # tract's analyser re-infers F16 instead of trusting a cached F32 hint.
    if target_output is not None:
        kept = [vi for vi in converted.graph.value_info if vi.name != target_output]
        del converted.graph.value_info[:]
        converted.graph.value_info.extend(kept)

    onnx.save(converted, str(out_path))
    print(f"saved {out_path} ({out_path.stat().st_size / 1_000_000:.1f} MB)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True, help="output directory (model_fp16.onnx + tokenizer.json land here)")
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    work = args.out / "_fp32_source"
    work.mkdir(exist_ok=True)

    for f in TOKENIZER_FILES:
        download(f"{BASE_URL}/{f}", args.out / f)

    fp32_path = work / "model.onnx"
    download(f"{BASE_URL}/{FP32_ONNX_PATH}", fp32_path)

    convert_and_patch(fp32_path, args.out / "model_fp16.onnx")

    print(f"\nready: {args.out}")
    print("point TRAVSR_RERANK_TEST_MODEL_DIR (or TractReranker::load) at this directory.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
