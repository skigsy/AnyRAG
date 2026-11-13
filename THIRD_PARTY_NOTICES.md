# Third-party license information

The optional Excel reader uses the MIT-licensed Calamine crate recorded in the lockfile.

The root [Cargo lockfile](Cargo.lock) records Rust dependencies. Bundled SQLite is public domain. Optional features use lopdf, ureq and rustls, tokenizers and ort, ring and tiny_http, or the AWS SDK. The local dashboard uses httparse, getrandom, subtle, and time. Dependencies retain their own licenses and notices. A binary or image needs an inventory for the features built into it, not the default binary's inventory.

In-process embeddings use separately acquired BGE model files and an operator-supplied ONNX Runtime library. The model's source, license, sizes and checksums are in the [pinned manifest](crates/rust-rag/src/inference/model-manifest.json). No weights or ONNX Runtime shared libraries ship in this source tree. [SQLite copyright](https://sqlite.org/copyright.html) · [BGE model card](https://huggingface.co/BAAI/bge-small-en-v1.5)

Local reranking uses separately acquired Apache-2.0 models. The Ettin bundle helper composes the publisher's quantized encoder and classifier head for local execution. The earlier MiniLM export remains available in existing profiles. [Ettin model card](https://huggingface.co/cross-encoder/ettin-reranker-150m-v1) · [Pinned Ettin revision](https://huggingface.co/cross-encoder/ettin-reranker-150m-v1/commit/cfa56e0d46b3a6ad6a710afbc3f202a5f2290aae) · [MiniLM model card](https://huggingface.co/cross-encoder/ms-marco-MiniLM-L2-v2)

The optional Linux OCR image builds with Ubuntu packages for Poppler and Tesseract, hash-pinned Python packages, and the Apache-2.0 SLANet_plus table model. It retains the packages’ license files. The [image recipe](infra/ocr/Dockerfile) pins the model archive and its extracted files. The [worker lock](infra/ocr/requirements-linux-amd64.lock) lists the Python distributions and their hashes. Inspect the exact image’s dependency and license inventory before distribution. [Ubuntu Poppler source package](https://launchpad.net/ubuntu/+source/poppler) · [SLANet_plus model card](https://huggingface.co/PaddlePaddle/SLANet_plus)
