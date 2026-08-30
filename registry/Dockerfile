# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS builder
WORKDIR /workspace

COPY Cargo.toml Cargo.lock README.md ./
COPY src ./src
COPY registry/Cargo.toml registry/build.rs ./registry/
COPY registry/src ./registry/src
COPY registry/schema ./registry/schema
COPY registry/seed ./registry/seed

RUN cargo build --locked --release -p agentstack-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 agentstack \
    && useradd --uid 10001 --gid agentstack \
         --create-home --home-dir /var/lib/agentstack \
         --shell /usr/sbin/nologin agentstack \
    && install -d -o agentstack -g agentstack /var/lib/agentstack/blobs

COPY --from=builder \
    /workspace/target/release/agentstack-server \
    /usr/local/bin/agentstack-server

WORKDIR /var/lib/agentstack
USER agentstack:agentstack

ENV AGENTSTACK_BLOB_DIR=/var/lib/agentstack/blobs

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/agentstack-server"]
CMD ["serve"]
