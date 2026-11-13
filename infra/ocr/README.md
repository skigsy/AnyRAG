# Offline scanned and table PDF ingestion

This experimental Linux amd64 recipe adds OCR and table evidence to a fresh folder index. Any RAG passes PDF bytes over a private Unix socket. The worker sees the submitted PDF and has no source-folder, index, credential, or container-runtime mounts. Use the native `--pdf` reader for text-layer collections. [OCR qualification status](../../status.md#all-in-one-release-gates)

## Build and start

Use Docker with BuildKit on Linux and an approved Ubuntu 24.04 amd64 base pinned by digest. The worker build compiles the Rust geometry helper in a pinned builder stage and acquires the hash-pinned Python dependencies and [SLANet_plus model](https://www.paddleocr.ai/v3.0.3/en/version3.x/module_usage/table_structure_recognition.html) for fallback detection. Run the resulting worker with networking disabled. See [third-party notices](../../THIRD_PARTY_NOTICES.md).

```sh
ubuntu_base=ubuntu@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
docker build --platform linux/amd64 --build-arg UBUNTU_IMAGE="$ubuntu_base" \
  --tag rust-rag-ocr:local infra/ocr
cargo install --locked --path crates/rust-rag --features ocr
worker_image=$(docker image inspect --format '{{.Id}}' rust-rag-ocr:local)
worker_pin=$worker_image
fingerprint=$(docker run --rm --network none "$worker_image" --fingerprint --image-id "$worker_pin")
socket_dir=$(mktemp -d /tmp/rust-rag-ocr.XXXXXXXX)
printf 'Socket directory %s\nFingerprint %s\n' "$socket_dir" "$fingerprint"
docker run --rm --init --name rust-rag-ocr --network none --read-only \
  --cap-drop ALL --security-opt no-new-privileges --pids-limit 64 \
  --cpus 4 --memory 2560m --memory-swap 2560m \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=512m \
  --user "$(id -u):$(id -g)" \
  --mount "type=bind,src=$socket_dir,dst=/run/rust-rag-ocr" \
  "$worker_image" --socket /run/rust-rag-ocr/worker.sock --image-id "$worker_pin"
```

Run the worker in its own terminal or supervise it as a service. The `socket_dir` and worker image fingerprint must be available when writing the profile. Keep the profile outside the source and state directories. Use your real absolute socket path and the 64-character fingerprint from the image. Both peers check the mapped user ID. The profile pins the fingerprint derived from the operator-selected image ID and the worker’s verified artifact manifest.

When transferring an approved worker image to another host, preserve the original `worker_pin` and its verified archive digest. Docker's classic and containerd stores can display different IDs for the same content. Verify the archive's config and ordered root filesystem layers, then use the destination's local image reference as `worker_image` and keep the recorded pin as `--image-id`. This preserves the extraction fingerprint across the transfer. [Docker image stores](https://docs.docker.com/engine/storage/containerd/)

The host needs Landlock ABI 3 or newer and a seccomp-enabled Docker runtime whose profile permits the Landlock and seccomp setup calls. The worker checks that it can start a restricted process before accepting connections. Each document runs in a fresh job directory with filesystem and syscall restrictions. The supervisor kills and reaps the job process group and verifies scratch removal before accepting the next document. If cleanup cannot be verified, the worker exits. Start a fresh container with the same image pin and a fresh tmpfs to resume.

```json
{
  "schema_version": 1,
  "socket": "/absolute/private/socket-directory/worker.sock",
  "fingerprint": "replace-with-the-worker-fingerprint"
}
```

Use separate source and state directories. When configuring a new OCR fingerprint, start with fresh state and reindex from the authoritative folder. A normal query or export can open the existing state when the worker is stopped.

```sh
rust-rag --state-dir "$STATE" init --ocr-profile "$OCR_PROFILE"
rust-rag --state-dir "$STATE" ingest "$SOURCE" --sync --ocr-profile "$OCR_PROFILE" --json
rust-rag --state-dir "$STATE" query 'What is the inspection target for this station?' --json
```

The worker accepts unencrypted PDFs up to 16 MiB and 12 pages per file. Raster recognition uses English. The table detector looks for repeated rows with 2–16 columns and requires visible rules for two-column tables. Split longer documents before ingest. Check `failures` and `ocr_deferred_sources` before treating a sync as complete. A failed extraction leaves the previous revision available. A successful replacement or source removal retires the previous citations. The `ocr` field records the rendition fingerprint, displayed page regions, and exact Unicode offsets in the canonical transcription. Table support lists the row key and ordered header regions. `header_grouping_ambiguous` means the worker kept the verified leaf evidence and withheld unresolved parent grouping.

When a printed group label and its separator ground a row scope, table support also returns `row_group` with the label's literal region. Repeated row keys bind only where their group label distinguishes them. Unresolved keys and values remain searchable as text.

## Shared Linux deployment

Build the service image with `--build-arg WITH_OCR=1` or use a native build with `--features ocr,shared`. Follow the [private VM deployment recipe](../private-vm.md), add the absolute in-container OCR profile path as `ocr_profile`, and set `pdf` to `false`. The OCR worker and service must share the same mapped UID. For the standard service image use UID 10001, a host socket directory owned by that UID with mode `0700`, and a read-only mount of the directory into the service. Mount the OCR profile read-only into the service. Use folder sources and the local typed graph if required. The worker can process one PDF at a time, and a long sync can require multiple runs. The service still uses its normal access and restore controls.

Stop the worker to stop OCR processing. Keep or delete the private index according to your retention policy. The source folder stays unchanged. Backups and exports have separate retention.
