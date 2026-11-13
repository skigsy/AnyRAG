# Rehearse the operator workflow

Run this walkthrough on a fresh Linux checkout to verify personal search, local graph review, and shared Graph RAG from end to end. It uses invented documents, temporary storage and identities, and deterministic HTTP responses over loopback. It needs no model weights, cloud account, or real documents.

## Start from source on Linux

Install Git, Rust via rustup, a C compiler with standard build tools, Python 3.10 or newer, and OpenSSL. The checkout selects the Rust version in `rust-toolchain.toml`. Cargo may fetch locked build dependencies. The rehearsal itself uses only loopback network access.

~~~sh
git clone https://github.com/skigsy/AnyRAG.git
cd AnyRAG
cargo build --locked --features shared --bin rust-rag
receipt_dir="$(mktemp -d)"
python3 -B scripts/rehearse_operator.py \
  --binary target/debug/rust-rag --receipt "$receipt_dir/operator.json"
cat "$receipt_dir/operator.json"
~~~

Use the same script on Windows with `python`, `target/debug/rust-rag.exe`, an unused receipt path, and `--openssl` if OpenSSL is outside your path. For a verified standalone binary, supply its path and `--personal-only` to run the first step.

## What the receipt checks

1. **Personal search.** Index three Markdown notes, report the skipped CSV, explain the highest ranked passage, and reopen its exact source revision.
2. **Local graph.** Verify the standard extractor labels its graph assertion unverified. Switch to source-first review, verify pending links stay out of traversal, approve a cited link, refuse an approval after a raw edit, and dismiss the stale proposal.
3. **Signed team Graph RAG.** Start the shared service with short-lived fixture identities. Publish a routine catalog declaration and reopen its citation. Stage prose and a high-impact declaration, approve one and dismiss the other through the signed review API, and verify a repeated decision is idempotent.
4. **Access and recovery.** Revoke the reviewer, edit one published source, and delete the other. Restore a backup, verify reads are blocked during recovery, reconcile with current files and grants, and confirm the old citations stay retired.

The process exits nonzero on failure. The receipt reports the binary and runner hashes, elapsed time, and each completed check. Any unexecuted checks are outside the recorded result. Temporary notes, indexes, model responses, and identity material are separate from the receipt.

The [pinned model rehearsal](setup.md#rehearse-a-cited-company-answer-on-linux) measures a different part of the stack using a local model. Use the [private deployment runbook](../infra/private-vm.md) to configure trusted identity, TLS, data, and backups. Use the [evidence guide](evidence.md) to distinguish the tested behavior from corpus quality and deployment claims.
