FROM rust:1.97-bookworm
ARG TARGETARCH
WORKDIR /heapwork
COPY Cargo.toml Cargo.lock ./
COPY guest/Cargo.toml guest/Cargo.toml
COPY guest/src guest/src
COPY agent/Cargo.toml agent/Cargo.toml
COPY agent/src agent/src
COPY instrument/Cargo.toml instrument/Cargo.toml
COPY instrument/src instrument/src
COPY host/Cargo.toml host/Cargo.toml
COPY host/src host/src
RUN --mount=type=cache,id=heapwork-target-${TARGETARCH},target=/heapwork/target \
    --mount=type=cache,id=heapwork-registry,target=/usr/local/cargo/registry \
    touch host/src/*.rs && CARGO_BUILD_JOBS=2 cargo build --release --locked -p host \
    && mkdir -p /heapwork/bin && cp target/release/heapwork /heapwork/bin/heapwork
ENV HEAPWORK_WASM=/heapwork/coder.async.wasm HEAPWORK_STATE=/session
ENTRYPOINT ["/heapwork/bin/heapwork"]
