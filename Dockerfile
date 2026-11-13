# syntax=docker/dockerfile:1
ARG RUST_IMAGE=rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e
ARG RUNTIME_IMAGE=debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

FROM ${RUST_IMAGE} AS build
ARG WITH_AWS=0
ARG WITH_OCR=0
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/rust-rag/Cargo.toml crates/rust-rag/Cargo.toml
COPY crates/rust-rag/src/ crates/rust-rag/src/
RUN case "$WITH_AWS" in \
        0) features=pdf,docx,shared,dashboard ;; \
        1) features=pdf,docx,aws; apt-get update \
            && apt-get install -y --no-install-recommends cmake \
            && rm -rf /var/lib/apt/lists/* || exit 1 ;; \
        *) echo 'Set WITH_AWS to 0 or 1.' >&2; exit 2 ;; \
    esac \
    && case "$WITH_OCR" in \
        0) : ;; \
        1) features="$features,ocr" ;; \
        *) echo 'Set WITH_OCR to 0 or 1.' >&2; exit 2 ;; \
    esac \
    && cargo build --release --locked --bin rust-rag --features "$features"

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 rustrag \
    && useradd --system --uid 10001 --gid rustrag --no-create-home --home-dir /var/lib/rust-rag rustrag \
    && install -d -o rustrag -g rustrag -m 0700 /var/lib/rust-rag
COPY --from=build /build/target/release/rust-rag /usr/local/bin/rust-rag
COPY LICENSE THIRD_PARTY_NOTICES.md /usr/share/doc/rust-rag/

USER 10001:10001
WORKDIR /var/lib/rust-rag
RUN rust-rag capabilities --json
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s \
    CMD ["curl", "--fail", "--silent", "--max-time", "2", "--output", "/dev/null", "http://127.0.0.1:8787/health"]
ENTRYPOINT ["rust-rag"]
# Non-loopback binding requires an operator profile that explicitly sets allow_proxy_http.
CMD ["serve", "--profile", "/run/rust-rag/config/profile.json", "--bind", "0.0.0.0:8787"]
