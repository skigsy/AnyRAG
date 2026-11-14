# Portable shared service on a private Linux VM

Use this recipe on an existing Linux host, including an Azure VM. It requires a Docker-compatible runtime, a dedicated host-attached disk, a trusted OIDC issuer, and a private HTTPS listener. Run one service instance per state directory, with all its collection stores on the same host. Put state on an attached block device, and keep policy history and backups on independently managed storage. [Any RAG architecture](../docs/architecture.md) · [Azure Linux VM data disks](https://learn.microsoft.com/en-us/azure/virtual-machines/linux/disks-format-mount-data-disks-linux)

## Build an image

Use a clean approved checkout and Docker BuildKit. The default image builds the Rust workspace with shared serving, PDF, and Word support, bundles SQLite, and runs as UID 10001. Start a local rehearsal with the base image digests in the Dockerfile. Build the optional AWS feature when the deployment needs it.

~~~sh
docker build --tag rust-rag:local .
~~~

The image context allowlists Rust source and packaging files. For a release, record both base image digests and the resulting image digest. Pass explicit `RUST_IMAGE` and `RUNTIME_IMAGE` build arguments only when using other approved, digest-pinned bases. Use the verified digest for promotion.

### Load a reviewed release archive

Download `rust-rag-linux-amd64.tar`, `linux-image.json`, and `SHA256SUMS.txt` from the [v1.1.2 release](https://github.com/skigsy/AnyRAG/releases/tag/v1.1.2). With Docker and `jq` installed, run this from their folder. Check both transferred files before loading. Different Docker image stores may report the reviewed image ID or its Linux amd64 config digest. [Verify package provenance](../docs/evidence.md#verify-package-provenance)

```sh
archive=rust-rag-linux-amd64.tar
metadata=linux-image.json

RUST_RAG_IMAGE=$(
  for name in "$archive" "$metadata"; do
    entry=$(awk -v name="$name" '$2 == name { print }' SHA256SUMS.txt)
    test "$(printf '%s\n' "$entry" | wc -l)" -eq 1 || exit 1
    printf '%s\n' "$entry" | sha256sum --check --strict >&2 || exit 1
  done
  loaded_tag=$(jq -er '.image_tag | select(test("^any-rag-local:1\\.1\\.2-[a-f0-9]{12}$"))' "$metadata") || exit 1
  reviewed_id=$(jq -er '.image_id | select(test("^sha256:[a-f0-9]{64}$"))' "$metadata") || exit 1
  image_config=$(jq -er '.config_digest | select(test("^sha256:[a-f0-9]{64}$"))' "$metadata") || exit 1
  docker load --input "$archive" >&2 || exit 1
  image_id=$(docker image inspect --format '{{.Id}}' "$loaded_tag") || exit 1
  case "$image_id" in
    "$reviewed_id"|"$image_config") ;;
    *) printf '%s\n' 'Loaded image differs from the reviewed image.' >&2; exit 1 ;;
  esac
  platform=$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "$image_id") || exit 1
  test "$platform" = linux/amd64 || exit 1
  printf '%s\n' "$image_id"
) || exit 1
```

Run the commands below or the [personal Linux quickstart](../docs/setup.md#try-personal-search-in-the-linux-image) in that shell. They use the verified local image ID and disable pulls. A source-built rehearsal uses `rust-rag:local` by default. Record the archive SHA-256, observed image ID, and `source_commit` from the checked metadata in the deployment receipt.

## Prepare trusted inputs

1. Mount the attached block volume at `/var/lib/rust-rag`. Provision it for service UID and GID `10001` with mode `0700`. Use encrypted storage if the data requires it. Keep the image read-only and writable temporary data on a bounded tmpfs.
2. Copy [the shared profile](shared.profile.example.json) to `/etc/rust-rag/profile.json` outside the repo and update the issuer, audience, JWKS URL, and source locations. Set `allow_proxy_http: true` for the isolated container listener, and publish only to the host loopback behind your TLS proxy. Give the service account read access to this file, source collections, and any approved model credential files. Keep sources mounted read-only. Provision each collection as a separate non-overlapping path.
3. Set up the policy directory on durable storage separate from index state. Use mode `0700` on the policy and backup roots. On Linux, Any RAG rejects group or other permissions on these directories. Give UID and GID `10001` access to the roots and policy files, and mount policy read-only in the container. Publish `versions/1.json` from a trusted host-side administrator, then atomically replace `current.json` with those exact bytes. Increase `version` for every policy change and publish its version file before replacing `current.json`.

```json
{
  "schema_version": 1,
  "version": 1,
  "issuer": "https://identity.example.com/",
  "administrators": ["replace-with-admin-subject"],
  "subjects": {"replace-with-reader-subject": ["reader"]},
  "collections": {"reference": {"roles": ["reader"], "subjects": [], "endpoints": []}}
}
```

Set the policy `issuer` to the exact issuer configured in the service profile. Use its `sub` value in the server-owned grants, with signed API access tokens for the configured issuer and audience. An administrator grant allows maintenance operations. A document read grant is assigned separately. In Azure, an operator can use a tenant-specific identity provider and map subject identities in this manifest. A missing collection policy denies access to its documents.

For the signed checks below, prepare a maintenance subject, an allowed reader, and a denied reader. Add a second allowed reader for the restore control so that one reader can keep access while the other is revoked. Obtain RS256 access tokens with the exact configured `iss` and `aud`, a current `exp`, and a `kid` present in the trusted JWKS. For a self-contained lifecycle check with temporary identities and a controlled model, run the repository's synthetic rehearsal. [Synthetic lifecycle rehearsal](../docs/setup.md#rehearse-private-graph-rag-with-synthetic-sources)

Remote `jwks_url` and model URLs use HTTPS with the build's bundled WebPKI roots. If the issuer uses a private CA, the portable service also accepts an absolute `identity.jwks_file` path in place of `jwks_url`. Have the operator fetch and verify the issuer's public key set over a trusted connection and atomically rotate the mounted copy. Validate key rotation before production use. Use a literal loopback address for HTTP test endpoints.

### Synthetic identity and TLS on a disposable VM

For the local company rehearsal, use Python 3.10+, OpenSSL, and nginx on the disposable VM. If the nginx package starts a default service, mask `nginx.service` before installing it on this test guest. The commands below launch a separate proxy bound to loopback. Prepare the image, storage, and model first so the test access tokens are fresh when you need them.

```sh
umask 077
fixture=$(mktemp -d)
python3 -B scripts/rehearse_company.py --write-fixture-identity "$fixture/identity"
sudo install -d -m 0755 /etc/rust-rag/public-keys
sudo install -m 0444 "$fixture/identity/jwks.json" /etc/rust-rag/public-keys/jwks.json
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj '/CN=127.0.0.1' \
  -addext 'subjectAltName=IP:127.0.0.1' -keyout "$fixture/tls.key" -out "$fixture/tls.crt"
```

In the example profile and policy use issuer `https://issuer.example.invalid` and profile audience `rust-rag-rehearsal`. Replace `jwks_url` with `jwks_file` set to `/run/rust-rag/identity/jwks.json` and mount only the public key directory as shown in the [model layout](#verify-a-generated-graph-rag-answer). Assign `admin` to maintenance and give `reader` and `reader-control` access to `reference`. For the restricted destination check, give `witness` access to a separate `restricted` collection with no allowed model destinations. Leave `denied` without grants. The generated tokens expire after ten minutes. Keep them in the private fixture directory and copy only `jwks.json` into the mounted key directory.

```sh
mkdir "$fixture/temp"
cat > "$fixture/nginx.conf" <<'NGINX'
pid nginx.pid;
error_log nginx-error.log warn;
events {}
http {
    access_log off;
    client_body_temp_path temp/client;
    proxy_temp_path temp/proxy;
    fastcgi_temp_path temp/fastcgi;
    uwsgi_temp_path temp/uwsgi;
    scgi_temp_path temp/scgi;
    server {
        listen 127.0.0.1:8443 ssl;
        ssl_certificate tls.crt;
        ssl_certificate_key tls.key;
        location / {
            proxy_pass http://127.0.0.1:8787;
            proxy_http_version 1.1;
            proxy_read_timeout 135s;
        }
    }
}
NGINX
nginx -p "$fixture/" -c "$fixture/nginx.conf" -t
nginx -p "$fixture/" -c "$fixture/nginx.conf"
export CURL_CA_BUNDLE="$fixture/tls.crt"
RUST_RAG_URL=https://127.0.0.1:8443
```

After starting the container in the next section, verify the proxy with `curl --fail --silent --show-error "$RUST_RAG_URL/health"`. For each signed check, set `RUST_RAG_TOKEN=$(cat "$fixture/identity/reader.jwt")` using the file for the requested role. Use the `rust_rag_request` helper below so the token reaches curl through standard input. Keep shell tracing disabled. At the end run `nginx -p "$fixture/" -c "$fixture/nginx.conf" -s quit`, unset the token and CA variables, and remove the disposable fixture and mounted public key file. This path uses file-backed fixture keys. For deployment use the trusted issuer and private TLS certificate described above. [nginx command-line options](https://nginx.org/en/docs/switches.html) · [Proxy read timeout](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout)

## Start and verify

The host has a private TLS reverse proxy forwarding to loopback port `8787`. The backup directory is on separate protected storage and writable only by the deployment service. The following mounts match the example profile.

If readers request generated answers, set the proxy's upstream read timeout above the answer endpoint's configured `timeout_seconds`. Check an actual answer through the proxy after deploying a local CPU model. Retrieval-only requests can succeed while a shorter proxy timeout cuts off generation.

```sh
docker run --rm --init --name rust-rag --pull=never \
  --read-only --tmpfs /tmp:rw,nosuid,nodev,noexec,size=128m \
  --cap-drop=ALL --security-opt=no-new-privileges \
  --publish 127.0.0.1:8787:8787 \
  --mount type=bind,src=/var/lib/rust-rag,dst=/var/lib/rust-rag \
  --mount type=bind,src=/etc/rust-rag/profile.json,dst=/run/rust-rag/config/profile.json,readonly \
  --mount type=bind,src=/srv/rust-rag/policy,dst=/run/rust-rag/policy,readonly \
  --mount type=bind,src=/srv/rust-rag/reference,dst=/srv/rust-rag/reference,readonly \
  --mount type=bind,src=/var/backups/rust-rag,dst=/var/backups/rust-rag \
  "${RUST_RAG_IMAGE:-rust-rag:local}"
```

`/health` measures process liveness. Use authenticated status and a sample retrieval to establish readiness. Before starting the container, run the command above with the same mounts and append `service-doctor --profile /run/rust-rag/config/profile.json --json` after the image name. The doctor validates the mounted configuration. The signed HTTP checks below exercise the running service. For a synthetic smoke test, put a Markdown file containing “The Ternmarker is in drawer seven.” in the reference collection. Use one reader with a grant and one without.

The following shell helper reads the access token from an existing protected session variable and gives it to curl through stdin. Set `RUST_RAG_URL` to the private HTTPS URL and select the token for the role named in each step. Disable shell tracing while working with tokens.

```sh
rust_rag_request() {
  method=$1
  path=$2
  shift 2
  curl --silent --show-error --fail-with-body --config - \
    --request "$method" "$RUST_RAG_URL$path" "$@" <<EOF
header = "Authorization: Bearer $RUST_RAG_TOKEN"
EOF
}

# Administrator
rust_rag_request POST /v1/admin/sync
rust_rag_request GET /v1/admin/status

# Authorized reader, after sync completes
rust_rag_request POST /v1/query --json '{"question":"Ternmarker","mode":"lexical"}'
rust_rag_request GET /v1/evidence/replace-with-chunk-id
```

Sync admission returns HTTP 202. Poll the status endpoint until `sync.status` is `complete` before querying. Investigate a terminal partial or failed sync using its per-collection status. Choose a `chunk_id` from the query's `evidence` array and use it in the citation URL. The citation response contains one object under `evidence`. Compare its `text` with the source between `start` and `end`, and check `revision` against the source SHA-256. The offsets count Unicode scalar values and the end is exclusive. Repeat both calls with the denied reader. The query omits the restricted passage and the exact citation returns HTTP 404. Publish a new policy version revoking the authorized reader and repeat the two calls with the original token to check that the current grant takes effect before the next sync.

### Verify a generated Graph RAG answer

Run the signed answer check against a source-built shared executable before assembling the container and proxy, or use the HTTPS mode below to check the deployed service. [Cited company answer rehearsal](../docs/setup.md#rehearse-a-cited-company-answer-on-linux)

For a typed graph, configure separate endpoint names for extraction and answering. Both may point to the same approved model server. Use the chat endpoint example to create `relations.json` and `answer.json`, and mount each read-only at its profile path. Keep the extraction predicates in the relations config. Grant both endpoint names in the policy only for collections whose text those endpoints may process. [Chat endpoint example](model-endpoint.example.json)

For a loopback HTTP model, run the model in the same isolated network namespace as the service. The container's `127.0.0.1` belongs to that namespace. Use an approved HTTPS endpoint for a model in a separate namespace. The pinned local Qwen replay used `response_format: "schema"` for both endpoints. [Pinned model replay](../docs/evidence.md#reproduce-the-mixed-graph-quality-run)

The CPU reference uses the Qwen3-4B Q4_K_M GGUF (about 2.5 GB) and llama.cpp b12207 Ubuntu x64 CPU archive. Verify SHA-256 `7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5` for the weights and `3dbcc99e866441ba2fd541561c90d09581ed10f4c56d0cca2461c0b1f311efe7` for the archive. The Ubuntu guest needed `libgomp1`. The replay used four CPU threads, a 4,096-token context, one server slot, and no GPU layers. [Pinned weights](https://huggingface.co/Qwen/Qwen3-4B-GGUF/blob/bc640142c66e1fdd12af0bd68f40445458f3869b/Qwen3-4B-Q4_K_M.gguf) ·

On a dedicated test VM, create a private bridge and a small anchor container that owns the host loopback publication. The app and the CPU model join its network namespace. The following commands use the selected image, `nftables`, `nsenter` and `setpriv`.

```sh
docker network create --driver bridge rust-rag-local-model
docker run --detach --name rust-rag-local-net --pull=never \
  --network rust-rag-local-model --publish 127.0.0.1:8787:8787 \
  --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --entrypoint /bin/sleep "${RUST_RAG_IMAGE:-rust-rag:local}" infinity
anchor_pid="$(docker inspect --format '{{.State.Pid}}' rust-rag-local-net)"
test "$anchor_pid" -gt 0
sudo nsenter --target "$anchor_pid" --net -- nft -f - <<'NFT'
table inet rust_rag_local {
  chain output {
    type filter hook output priority filter; policy drop;
    oifname "lo" accept
    ct state established,related accept
  }
}
NFT
```

Use the packaged binary and libraries from a trusted directory readable by UID 10001. Run the model in a separate terminal so you can stop it directly.

```sh
llama_dir=/opt/llama-b12207
anchor_pid="$(docker inspect --format '{{.State.Pid}}' rust-rag-local-net)"
test "$anchor_pid" -gt 0
sudo nsenter --target "$anchor_pid" --net -- \
  setpriv --reuid=10001 --regid=10001 --clear-groups --no-new-privs \
  env LD_LIBRARY_PATH="$llama_dir" "$llama_dir/llama-server" \
  --model /path/to/Qwen3-4B-Q4_K_M.gguf --alias local-qwen \
  --host 127.0.0.1 --port 8081 --threads 4 --ctx-size 4096 \
  --parallel 1 --n-gpu-layers 0 --seed 42 --jinja --reasoning off
```

For this shared-namespace layout, set both endpoint URLs to `http://127.0.0.1:8081/v1/chat/completions` and their model to `local-qwen`. Set `response_format` to `schema` and size `timeout_seconds` for the CPU. The replay used 120 seconds for the model and 135 seconds for the proxy read timeout. Replace the profile's `identity.jwks_url` with `identity.jwks_file` set to `/run/rust-rag/identity/jwks.json`. Put the verified public keys in `/etc/rust-rag/public-keys/jwks.json` on the host. A directory mount lets the issuer's verified JWKS be replaced atomically.

Use the main Docker start command with `--network container:rust-rag-local-net` in place of its `--publish` flag. Add these three options before the image name.

```text
--mount type=bind,src=/etc/rust-rag/relations.json,dst=/run/rust-rag/config/relations.json,readonly
--mount type=bind,src=/etc/rust-rag/answer.json,dst=/run/rust-rag/config/answer.json,readonly
--mount type=bind,src=/etc/rust-rag/public-keys,dst=/run/rust-rag/identity,readonly
```

When finished with this test layout, stop the app and model, remove the `rust-rag-local-net` anchor, and remove the `rust-rag-local-model` bridge. The attached state, backup and policy disks can be kept for the recovery checks.

```json
{
  "endpoints": {
    "relations": "/run/rust-rag/config/relations.json",
    "answer": "/run/rust-rag/config/answer.json"
  },
  "graph": {"mode": "typed", "extractor": "relations"}
}
```

#### Run the signed fixture through the deployed HTTPS listener

Use a disposable VM with fresh index state and empty source and policy directories. Generate the two source collections and the starting policy as well as the identities. In the [synthetic identity setup](#synthetic-identity-and-tls-on-a-disposable-vm), replace the `--write-fixture-identity` command with the following and use `"$fixture/company/identity"` for the token and public-key paths.

```sh
python3 -B scripts/rehearse_company.py --write-portable-fixture "$fixture/company"
sudo install -d -m 0755 /srv/rust-rag/reference /srv/rust-rag/restricted
sudo install -m 0444 "$fixture/company/reference/"*.md /srv/rust-rag/reference/
sudo install -m 0444 "$fixture/company/restricted/"*.md /srv/rust-rag/restricted/
# /srv/rust-rag/policy must be on the dedicated policy storage.
sudo install -d -o 10001 -g 10001 -m 0700 /srv/rust-rag/policy /srv/rust-rag/policy/versions
sudo install -o 10001 -g 10001 -m 0400 "$fixture/company/policy/versions/1.json" /srv/rust-rag/policy/versions/1.json
sudo install -o 10001 -g 10001 -m 0400 "$fixture/company/policy/current.json" /srv/rust-rag/policy/current.json
```

Add a `restricted` folder source with root `/srv/rust-rag/restricted` to the service profile and mount that path read-only alongside `reference`. Use the graph and endpoint configuration above. Once the proxy and model are ready, run the check before the first sync. It initiates sync, verifies both source passages and their citation IDs, and checks that the service refuses model processing for the restricted collection.

```sh
python3 -B scripts/rehearse_company.py --https-port 8443 \
  --fixture-identity "$fixture/company/identity" --fixture-ca "$fixture/tls.crt" \
  --receipt "$fixture/company-answer.json"
```

Save the redacted receipt separately from the fixture keys. The [edit, deletion, and restore checks](#verify-edits-deletion-and-restore) exercise the separate lifecycle gate. Use the manual calls below to inspect individual responses.

#### Inspect the answer manually

Index two reference notes containing “GA-9164 uses WQ-7945.” and “WQ-7945 requires KS-5815.” Wait for `sync.status` to reach `complete` and `sync.enrichment.reference.graph.pending` to reach zero. With a reader token for that collection, send the question through the TLS proxy.

```sh
rust_rag_request POST /v1/query --json '{
  "question": "Which catalog identifier does the equipment used by GA-9164 ultimately need? Cite both source passages.",
  "mode": "lexical", "graph": true, "limit": 5, "answer_endpoint": "answer"
}'
```

Check for `answer.status` equal to `answered`, catalog ID `KS-5815`, and two citation IDs. `answer.cited_evidence` lists the collection, source, revision and range for each cited ID, and `answer.delivery` lists which passages were sent to the model and which were omitted and why, without passage text. Resolve each ID through `GET /v1/evidence/<id>` and compare the quote and revision with the original notes. The service accepts only IDs or labels for passages sent in that request. A rejected citation returns `answer.status` equal to `unavailable` with the delivery manifest. The operator checks whether the text of the answer follows from the passages. A reader who can access a collection whose policy excludes the answer destination should get `MODEL_DESTINATION_DENIED` when the selected evidence includes a passage from that collection.

### Verify edits, deletion, and restore

1. With an administrator token, run `POST /v1/admin/backup` and retain the returned `backup_id` in protected storage. Keep the backup location separate from index state.
2. Restore the reader's grant with a higher policy version. Edit the synthetic source, run sync, and wait for `sync.status` to report `complete`. The new quote resolves and the saved old citation returns 404.
3. Delete the synthetic source at its authoritative location and sync again. Confirm the query omits the passage and the last citation returns 404.
4. Revoke the test reader again. Restore the earlier backup with `POST /v1/admin/restore` and a JSON body containing the saved `backup_id`. Confirm reads are held in maintenance. Run a complete sync against current sources and policy and check that `sync.inventory_reconciled` is true. Call `POST /v1/admin/reopen` as an administrator. Verify the current grant denies the test reader. With a separate reader who retains a document grant, check that the deleted citation returns 404 and an unchanged control citation still resolves. The old backup has a separate retention policy.

Shared sync holds an exclusive gate while updating evidence and projections. Query and evidence calls may return `409 WRITER_BUSY` with `Retry-After` until that work ends. A sync rejected as busy has not started. Model failures can yield lexical results in hybrid mode or cited passages without an answer. Read `effective_mode`, `answer.status`, and the response warnings to distinguish those cases. Vector-only requests require an embedding provider.

While an [OCR worker](ocr/README.md) parses a document, readers can query the last committed index under the current policy. Other maintenance writes still return busy. Sync checks the policy again before publishing the result, and a failed extraction keeps the previous revision.

Admin sync status includes per-collection graph and vector progress, plus any provider halt. After fixing an item failure, send `POST /v1/admin/retry-enrichment` with `{"collection":"reference","lane":"graph"}`, then sync again. Use `"vector"` for embeddings. A retry respects active leases and provider rate limits.

### Review shared graph relationships

Use the [v1.0.0 Linux image](#load-a-reviewed-release-archive) or a current source build with shared service support. Use a folder-backed collection with [typed graph enabled](#enable-company-graph-rag) and a configured extractor. Merge the following keys into the collection grant in a new access policy version, preserving any other required read roles and endpoint grants. Give catalog write access only to people or systems authorized to declare current relationships. Each trusted path is an exact collection-relative filename. In this example the issuer's reviewer subject maps to `maintenance-reviewer` in the top-level `subjects` policy map. The collection grants that role both read and review permissions. [Rehearse signed review and recovery on synthetic data](../docs/rehearsal.md)

```json
{
  "roles": ["maintenance", "maintenance-reviewer"],
  "endpoints": ["relations"],
  "graph_review_roles": ["maintenance-reviewer"],
  "graph_publication": {
    "trusted_paths": ["catalog/equipment.md"],
    "routine_predicates": ["uses"],
    "high_impact_predicates": ["requires", "owned_by"],
    "single_valued_predicates": ["owned_by"]
  }
}
```

Use a dedicated Markdown file for declarations. Put one subject, predicate and object on each line, in that order. Keep the block unindented. Delete an obsolete declaration and sync to retire its graph link.

~~~markdown
```anyrag-relations
Mixer-17 | uses | Filter-24
Mixer-17 | requires | Safety-Relay-8
```
~~~

The `uses` link can publish during sync if the declaration is valid, the path is trusted and the relation has no conflict. The `requires` link goes to review. Model-derived links, including routine predicates found in prose, go to review. Reverse declarations and conflicting targets on a configured single-valued predicate go to review. A malformed declaration block cannot publish links automatically. The graph uses exact active source spans. Unrecognized or invalid evidence stays out of traversal.

Keep declaration files separate from prose you want the model to extract. Chunks with valid declaration rows use the deterministic lane. The catalog supports up to 1,024 declaration files, 8,192 declarations per collection and 16 per indexed passage. Spread larger catalogs across collections. Those limits apply to declarations. The ordinary document index has its own sync limits.

An administrator syncs the collection after changing the current policy or catalog. The service withholds graph traversal for a collection whose publication rules still need reconciliation. Ordinary retrieval can still return source passages. A sync can publish declared links even while the model is unavailable or egress to it is disallowed. Removing a rule or a source retires the links it previously authorized on reconciliation. After a restore, follow the existing sync and reopen sequence.

As a signed reviewer, use the same private HTTPS helper from above:

```sh
rust_rag_request POST /v1/graph-review/list \
  -H 'Content-Type: application/json' \
  --data '{"collection":"reference","limit":20}'
```

Open the cited source and check the quote, relation direction and current context. Use `GET /v1/evidence/<chunk_id>` to reopen the indexed passage under the same read grant. Copy the proposal ID, revision, fingerprint and policy version from the response into the decision. Reuse the same idempotency key only when retrying that exact decision. Use `"dismiss"` to clear a proposal without publishing it. If the response includes `next_cursor`, send that value as `after` to request the next page.

```json
{
  "collection": "reference",
  "proposal_id": "<proposal_id>",
  "decision": "approve",
  "revision": "<revision>",
  "fingerprint": "<fingerprint>",
  "policy_version": 7,
  "idempotency_key": "maintenance-review-42"
}
```

Send that object with `POST /v1/graph-review/decide`. A stale revision, conflicting retry or policy change returns a conflict. Reload the queue before deciding again. The current index records the rule and policy version for automatic links, or the reviewer identity and policy version for confirmed links. Pending proposals and dismissed links stay outside graph search. This workflow applies to the portable typed graph in folder collections. Deployments without a publication policy continue to use direct model extraction.

In a graph query, `graph_support.assertion_status` identifies each supporting link as `declared`, `reviewed`, or `extracted_unverified`. Check `graph_support.path_contains_unverified_assertion` when a result depends on multiple links. The status describes publication provenance. The cited passage and current source context still determine whether the relationship is appropriate for the answer.

### Look up a reviewed relationship

Use the signed `POST /v1/relationship` route for an application that already knows the start entity, the requested final relation, the allowable path length, and any operating setting that must apply. The portable Linux image includes this endpoint. Ordinary natural-language queries still use `/v1/query`.

```sh
rust_rag_request POST /v1/relationship \
  -H 'Content-Type: application/json' \
  --data '{"root":"Mixer-17","terminal_predicates":["owned_by"],"min_steps":2,"max_steps":3,"scope":["night shift"],"limit":12}'
```

The service returns one directed path with its terminal `value`, exact relation quotes, and the chunk ID and revision for each supporting citation. Reopen each with `GET /v1/evidence/<chunk_id>`. The limit counts distinct relation and context citations. The other possible results are `no_supported_value`, `outside_requested_scope`, `ambiguous`, and `limit_reached`. Handle them explicitly before using a result for a decision.

For a link that depends on a setting or a source-backed name, place a `reviewed_relationships` entry in the collection grant of an administrator policy version. Each entry has `subject`, `predicate`, `object`, one `source` object, a `context` array, and an optional `scope` array of exact strings. The source and context objects each contain `chunk_id`, `revision`, and a literal `quote`. Put both exact entity names in the source quote, and cite the operating condition in context. Keep a conditional link out of the ordinary unqualified approval lane. Request scope selects applicability independently of collection permissions. Edits or deletion of any cited chunk retire the binding on sync. Review the replacement evidence before adding a new binding.

For model-generated answers, set `"explicit_answer_state": true` in the selected chat endpoint configuration. The structured response keeps `insufficient_evidence` for missing values, with optional `absence_evidence` when the model cites the recorded absence. An explicit negative can still be `answered`. Check the underlying citations when using model answers in automation.

### Upgrade or roll back the service

Keep the previous image digest, its profile and state, and a verified backup. Test the new image with a separate copy of the state and keep it off the client listener. If `service-doctor` reports a different deployment, provision a fresh state directory and run a complete sync against authoritative sources and current policy. A new graph extractor can change the binding even when the endpoint and model are unchanged. Backups from the previous binding stay with its compatible image and profile. Verify required graph paths and exact citations before switching traffic.

To roll back, close the client listener or remove the service from its proxy upstream. Start the previous image with its compatible profile and state. Restore its verified backup, run a complete sync against current sources and policy, and explicitly reopen. Check current citations, deleted sources, and denied identities before restoring client traffic. Keep the previous and new indexes separate throughout the cutover.

The restored service rebuilds work it needs from current sources. Budget time and provider calls for fresh graph extraction or embeddings, since the previous index does not reuse enrichment produced only by the new image. Time the sync and first verified citation on a representative corpus before scheduling the cutover.

### Re-review graph links after an extractor change

Back up the existing state and retain its matching image and policy. When an extractor change changes the deployment binding, start the new build with separate state and sync current sources and policy before routing traffic to it.

After sync, eligible routine declarations from trusted files publish again under the current policy. Model-derived links, high-impact declarations, and conflicts return to the review queue if they are proposed again. Approve them using their new proposal IDs and current source evidence. Old approval records and queued decisions belong to the old graph. Reopen one citation for each step of a known path and check the expected review queue before reopening graph traffic. For rollback, use the old image and its matching backup.

### Enable company Graph RAG

Use `rust-rag plan company --json` to confirm the shared build and deployment recipe. Add the following keys to the shared profile. Configure the mounted endpoint file from the [model example](model-endpoint.example.json) with the approved model and relation vocabulary. Pin the model and runtime for a qualified deployment.

```json
{
  "endpoints": {
    "relations": "/run/rust-rag/config/relations.json"
  },
  "graph": {
    "mode": "typed",
    "extractor": "relations"
  }
}
```

In the next policy version, add `"relations"` to `collections.<name>.endpoints` for each collection allowed to send text to that endpoint. Mount any key as an owner-readable file and reference its container path with `api_key_file`. Run the profile doctor again, start the service, then sync and check graph progress. Once the required passages have been processed, send a query with `"graph": true`. Inspect the exact anchors, supporting chunk IDs, and graph coverage. For cited generation, configure a named answer endpoint and grant its use separately.
