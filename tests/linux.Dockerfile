# Isolated minimum-toolchain validation; no host credentials, sockets or state.
# docker build --progress=plain -f tests/linux.Dockerfile -t mustr-validation .
FROM node:22.19.0-bookworm-slim AS node
FROM ghcr.io/astral-sh/uv:0.8.15 AS uv
FROM rust:1.89.0-bookworm

COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
COPY --from=uv /uv /usr/local/bin/uv
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && apt-get update \
    && apt-get install -y --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY roles ./roles
COPY pi/package.json pi/package-lock.json pi/tsconfig.json pi/index.ts pi/transport.mjs ./pi/
RUN npm --prefix pi ci --ignore-scripts --engine-strict
COPY pi/test ./pi/test
COPY tests/*.mjs tests/*.py ./tests/
RUN cargo test --locked \
    && npm --prefix pi run typecheck \
    && npm --prefix pi test
