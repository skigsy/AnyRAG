# syntax=docker/dockerfile:1
ARG UBUNTU_IMAGE
ARG RUST_IMAGE=rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e
FROM ${RUST_IMAGE} AS geometry
WORKDIR /src
COPY table_geometry.rs table_lanes.rs table_rules.rs ./
RUN rustc --edition=2024 -O --test table_geometry.rs -o /table-geometry-test \
    && /table-geometry-test \
    && rustc --edition=2024 -O -C strip=symbols table_geometry.rs -o /rust-rag-ocr-geometry

FROM ${UBUNTU_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libgl1 libglib2.0-0t64 libgomp1 libseccomp2 \
        fonts-dejavu-core=2.37-8 poppler-utils=24.02.0-1ubuntu9.11 \
        python3=3.12.3-0ubuntu2.1 python3-venv=3.12.3-0ubuntu2.1 \
        tesseract-ocr=5.3.4-1build5 tesseract-ocr-eng=1:4.1.0-2 tesseract-ocr-osd=1:4.1.0-2 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/rust-rag-ocr
COPY requirements-linux-amd64.lock .
RUN python3 -m venv /opt/ocr-venv \
    && grep '^setuptools==' requirements-linux-amd64.lock > /tmp/ocr-build.lock \
    && /opt/ocr-venv/bin/pip install --no-cache-dir --require-hashes --no-deps -r /tmp/ocr-build.lock \
    && /opt/ocr-venv/bin/pip install --no-cache-dir --require-hashes --no-deps --no-build-isolation -r requirements-linux-amd64.lock \
    && /opt/ocr-venv/bin/pip check \
    && rm /tmp/ocr-build.lock

ADD --checksum=sha256:68526c5c6573a8a17cdc14805e026f11c2545e42d89a1317bb58d3e1e4b58318 \
    https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/paddle3.0.0/SLANet_plus_infer.tar /tmp/table-model.tar
COPY worker.py serve.py .
COPY --from=geometry /rust-rag-ocr-geometry /usr/local/bin/rust-rag-ocr-geometry
RUN mkdir -m 0755 model \
    && for name in inference.json inference.pdiparams inference.yml; do \
        tar -xOf /tmp/table-model.tar "./SLANet_plus_infer/$name" > "model/$name" || exit; \
    done \
    && printf '%s\n' \
        'aba91cd956f4a1648b0f89df94c3cf8acaa066b47e2b689d109843b40d8545a2  model/inference.json' \
        '012986b0e2bfe90410618bfb4b7cf4fcb6c978caefd84c63c2934f8387b09ab8  model/inference.pdiparams' \
        '8a6372d3269a6f112fe13a2da7952a84da6e112c10a3146cbb43de5bd01d19fa  model/inference.yml' \
        | sha256sum --strict --check \
    && rm /tmp/table-model.tar \
    && LC_ALL=C dpkg-query -W -f='${binary:Package}\t${Version}\n' > packages.txt \
    && sha256sum worker.py serve.py packages.txt requirements-linux-amd64.lock model/* \
        /usr/local/bin/rust-rag-ocr-geometry /usr/bin/pdftoppm /usr/bin/pdftotext /usr/bin/pdfimages /usr/bin/tesseract \
        /usr/bin/rm /usr/bin/chmod \
        /usr/share/tesseract-ocr/5/tessdata/eng.traineddata \
        /usr/share/tesseract-ocr/5/tessdata/osd.traineddata > artifacts.sha256 \
    && install -d -o 10001 -g 10001 -m 0700 /run/rust-rag-ocr

ENV HOME=/tmp XDG_CACHE_HOME=/tmp/cache HF_HUB_OFFLINE=1 \
    PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK=True \
    PADDLE_PDX_LOCAL_FONT_FILE_PATH=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
    OMP_THREAD_LIMIT=4 OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1
USER 10001:10001
ENTRYPOINT ["/opt/ocr-venv/bin/python", "-B", "/opt/rust-rag-ocr/serve.py"]
CMD ["--socket", "/run/rust-rag-ocr/worker.sock"]
