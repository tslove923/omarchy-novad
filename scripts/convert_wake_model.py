#!/usr/bin/env python3
"""One-time wake-word model prep for novad — run via uv, not installed.

Usage:
    uv run --with onnx --with openwakeword python \\
        scripts/convert_wake_model.py [wakeword_name]

    wakeword_name defaults to "hey_jarvis" and must match one of the
    phrases bundled with the openwakeword PyPI package (alexa,
    hey_mycroft, hey_jarvis, ...); this does not train a new phrase,
    see nova-npu's trainer.py for that (deliberately not ported —
    stays an offline Python tool, see novad's roadmap doc).

Ports the exact transform nova-npu's src/nova/wake/model_converter.py does:
- melspectrogram.onnx / embedding_model.onnx: copied unmodified from the
  openwakeword package (they're already NPU-compatible as-is).
- <wakeword>.onnx: If-node removed (inlines both branches so the model
  always computes primary + verifier scores; threshold logic moves to
  the caller) since NPU doesn't support ONNX control flow.

Output written to ~/.local/share/novad/wake-models/<wakeword>/ as
melspectrogram.onnx, embedding.onnx, wakeword.onnx — matching
novad's src/wake/model_paths.rs lookup.
"""
import shutil
import sys
from pathlib import Path

import onnx
from onnx import TensorProto, helper, numpy_helper
import openwakeword

WAKEWORD_NAME = sys.argv[1] if len(sys.argv) > 1 else "hey_jarvis"
OUT_DIR = Path.home() / ".local/share/novad/wake-models" / WAKEWORD_NAME
OUT_DIR.mkdir(parents=True, exist_ok=True)

pkg_dir = Path(openwakeword.__file__).parent / "resources" / "models"
print(f"openwakeword resources: {pkg_dir}")


def fix_wakeword_model(model_path):
    """Direct port of nova's fix_wakeword_model (model_converter.py)."""
    model = onnx.load(str(model_path))
    graph = model.graph

    if_nodes = [n for n in graph.node if n.op_type == "If"]
    if not if_nodes:
        print("Model has no If nodes, using as-is")
        return model

    if_node = if_nodes[0]

    then_branch = None
    for attr in if_node.attribute:
        if attr.name == "then_branch":
            then_branch = attr.g
    if then_branch is None:
        raise ValueError("If node has no then_branch attribute")

    threshold = 0.5
    for node in graph.node:
        if node.op_type == "Constant" and "GreaterOrEqual" in node.output[0]:
            threshold = float(numpy_helper.to_array(node.attribute[0].t))
    print(f"Extracted verifier threshold: {threshold:.2f}")

    new_nodes = []
    for node in graph.node:
        if node.op_type == "If":
            break
        if node.op_type in ("GreaterOrEqual", "Cast"):
            continue
        new_nodes.append(node)

    for node in then_branch.node:
        new_nodes.append(node)

    all_initializers = list(graph.initializer) + list(then_branch.initializer)

    primary_output = None
    for node in reversed(new_nodes):
        if node.op_type == "Sigmoid" and any(
            "p1" in o or "input.19" in i for o in node.output for i in node.input
        ):
            primary_output = node.output[0]
            break
    if primary_output is None:
        for attr in if_node.attribute:
            if attr.name == "else_branch":
                for node in attr.g.node:
                    if node.op_type == "Identity":
                        primary_output = node.input[0]

    verifier_output = then_branch.output[0].name

    new_graph = helper.make_graph(
        new_nodes,
        "wakeword_no_if",
        inputs=[graph.input[0]],
        outputs=[
            helper.make_tensor_value_info(primary_output, TensorProto.FLOAT, [1, 1]),
            helper.make_tensor_value_info(verifier_output, TensorProto.FLOAT, [1, 1]),
        ],
        initializer=all_initializers,
    )

    new_model = helper.make_model(new_graph, opset_imports=model.opset_import)
    new_model.ir_version = model.ir_version
    onnx.checker.check_model(new_model)
    return new_model


# 1. melspectrogram — copy unmodified
mel_src = pkg_dir / "melspectrogram.onnx"
mel_dst = OUT_DIR / "melspectrogram.onnx"
shutil.copy2(mel_src, mel_dst)
print(f"melspectrogram -> {mel_dst}")

# 2. embedding — copy unmodified
emb_src = pkg_dir / "embedding_model.onnx"
emb_dst = OUT_DIR / "embedding.onnx"
shutil.copy2(emb_src, emb_dst)
print(f"embedding -> {emb_dst}")

# 3. wakeword — find + fix If node
candidates = list(pkg_dir.glob(f"{WAKEWORD_NAME}*.onnx"))
if not candidates:
    print(f"ERROR: no {WAKEWORD_NAME}*.onnx found in {pkg_dir}")
    print("Available models:", [p.name for p in pkg_dir.glob("*.onnx")])
    sys.exit(1)
ww_src = candidates[0]
print(f"wakeword source: {ww_src}")
fixed = fix_wakeword_model(ww_src)
ww_dst = OUT_DIR / "wakeword.onnx"
onnx.save(fixed, str(ww_dst))
print(f"wakeword -> {ww_dst}")

print("\nDone. Files in", OUT_DIR, ":")
for f in sorted(OUT_DIR.iterdir()):
    print(f"  {f.name}  {f.stat().st_size / 1e6:.2f} MB")
