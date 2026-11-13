"""Compose a checksum-pinned Ettin 150M bundle from local publisher files."""

import argparse
import ctypes
import errno
import json
import os
import sys
import tempfile
from hashlib import sha256
from pathlib import Path


PINS = {
    "encoder.onnx": (150_628_482, "27ac73363fd16d308fd7f91044df323f53af390ef775ac51e56e551c1adcee7d"),
    "dense.safetensors": (2_359_384, "bc39a6bdfd371a47a08fbf84781bc7224b846ec0e15213b560d2abbbfb323b75"),
    "norm.safetensors": (6_296, "a119c63bd3b89934c89f2394e2278014e663e01625dc322c603ddbf24b10325c"),
    "output.safetensors": (3_220, "cececab2925f12c9c7541d5d438202a6eec4b503658e0e64790f2d0414ede6a8"),
    "tokenizer.json": (3_583_327, "28c5e078e4c52aa37cf0e6de1a212878f3dbd58dd1c70466298efe0b6b86db35"),
    "model.onnx": (152_997_689, "b29f789c1c22abf20c0a47892762dbbfaceda72605b3089eb2685ddfbb213bfa"),
}


def verify(name: str, data: bytes) -> bytes:
    size, digest = PINS[name]
    if len(data) != size or sha256(data).hexdigest() != digest:
        raise ValueError(f"{name} does not match the pinned size and SHA-256")
    return data


def read_pinned(path: Path) -> bytes:
    if not path.is_file():
        raise ValueError(f"expected a local file named {path.name}")
    with path.open("rb") as source:
        return verify(path.name, source.read(PINS[path.name][0] + 1))


def compose(source: Path) -> bytes:
    import numpy as np
    import onnx
    from onnx import TensorProto, helper, numpy_helper

    def read_weights(name: str) -> dict:
        data = read_pinned(source / name)
        size = int.from_bytes(data[:8], "little")
        metadata = json.loads(data[8 : 8 + size])
        body = memoryview(data)[8 + size :]
        weights = {}
        for key, tensor in metadata.items():
            if tensor["dtype"] != "F32":
                raise ValueError(f"unexpected tensor type in {name}")
            lo, hi = tensor["data_offsets"]
            weights[key] = np.frombuffer(body[lo:hi], dtype="<f4").reshape(tensor["shape"])
        return weights

    dense = read_weights("dense.safetensors")
    norm = read_weights("norm.safetensors")
    output = read_weights("output.safetensors")
    model = onnx.load_model_from_string(read_pinned(source / "encoder.onnx"))
    graph = model.graph
    if (
        len(graph.output) != 1
        or graph.output[0].name not in ("last_hidden_state", "token_embeddings")
        or not any(item.domain == "" and item.version >= 17 for item in model.opset_import)
        or any(tensor.data_location == TensorProto.EXTERNAL for tensor in graph.initializer)
    ):
        raise ValueError("unexpected encoder graph")
    raw = graph.output[0].name
    constants = {
        "rr_cls_index": np.array(0, dtype=np.int64),
        "rr_dense_weight": dense["linear.weight"].T.copy(),
        "rr_sqrt2": np.array(2**0.5, dtype=np.float32),
        "rr_one": np.array(1, dtype=np.float32),
        "rr_half": np.array(0.5, dtype=np.float32),
        "rr_norm_weight": norm["norm.weight"],
        "rr_norm_bias": norm["norm.bias"],
        "rr_output_weight": output["linear.weight"].T.copy(),
        "rr_output_bias": output["linear.bias"],
    }
    for name, value in constants.items():
        graph.initializer.append(numpy_helper.from_array(value, name))

    def node(op: str, inputs: list[str], result: str, **attributes) -> None:
        graph.node.append(helper.make_node(op, inputs, [result], **attributes))

    node("Gather", [raw, "rr_cls_index"], "rr_cls", axis=1)
    node("MatMul", ["rr_cls", "rr_dense_weight"], "rr_projected")
    node("Div", ["rr_projected", "rr_sqrt2"], "rr_scaled")
    node("Erf", ["rr_scaled"], "rr_erf")
    node("Add", ["rr_erf", "rr_one"], "rr_shifted")
    node("Mul", ["rr_shifted", "rr_half"], "rr_factor")
    node("Mul", ["rr_projected", "rr_factor"], "rr_gelu")
    node(
        "LayerNormalization",
        ["rr_gelu", "rr_norm_weight", "rr_norm_bias"],
        "rr_norm",
        axis=-1,
        epsilon=1e-5,
    )
    node("MatMul", ["rr_norm", "rr_output_weight"], "rr_score")
    node("Add", ["rr_score", "rr_output_bias"], "logits")
    del graph.output[:]
    graph.output.append(helper.make_tensor_value_info("logits", TensorProto.FLOAT, ["batch_size", 1]))
    try:
        onnx.checker.check_model(model)
    except onnx.checker.ValidationError as error:
        raise ValueError(f"the composed ONNX graph did not validate: {error}") from error
    return verify("model.onnx", model.SerializeToString(deterministic=True))


def publish(staged: Path, output: Path) -> None:
    if os.name == "nt":
        # Windows rename fails if the destination already exists.
        staged.rename(output)
        return
    if sys.platform != "linux":
        raise OSError("atomic bundle publication requires Linux or Windows")
    # POSIX rename can replace an empty destination directory. Use Linux's
    # no-replace operation to keep a destination created concurrently intact.
    try:
        rename = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as error:
        raise OSError("atomic bundle publication requires Linux renameat2") from error
    rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    at_fdcwd, rename_noreplace = -100, 1
    if rename(at_fdcwd, os.fsencode(staged), at_fdcwd, os.fsencode(output), rename_noreplace):
        code = ctypes.get_errno()
        message = (
            "the destination needs a filesystem with atomic no-replace directory rename"
            if code in (errno.EINVAL, errno.ENOSYS, errno.EOPNOTSUPP)
            else os.strerror(code)
        )
        raise OSError(code, message, output)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="Requires NumPy and ONNX. The source directory contains encoder.onnx, "
        "dense.safetensors, norm.safetensors, output.safetensors and tokenizer.json.",
    )
    parser.add_argument("--source", type=Path, required=True, help="directory of the five pinned source files")
    parser.add_argument("--output", type=Path, required=True, help="new bundle directory under an existing parent")
    args = parser.parse_args()
    try:
        output = args.output.parent.resolve(strict=True) / args.output.name
        if os.path.lexists(output):
            raise FileExistsError(f"output already exists: {output}")
        tokenizer = read_pinned(args.source / "tokenizer.json")
        model = compose(args.source)
        with tempfile.TemporaryDirectory(prefix=".ettin-bundle-", dir=output.parent) as temp:
            staged = Path(temp) / "bundle"
            staged.mkdir(mode=0o700)
            (staged / "tokenizer.json").write_bytes(tokenizer)
            (staged / "model.onnx").write_bytes(model)
            publish(staged, output)
    except (OSError, ValueError, ImportError) as error:
        parser.exit(1, f"Ettin bundle preparation failed: {error}\n")
    for name in ("model.onnx", "tokenizer.json"):
        print(f"{PINS[name][1]}  {name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
