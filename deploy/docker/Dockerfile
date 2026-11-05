# syntax=docker/dockerfile:1
#
# Every MMX image, built from the repository root. See deploy/README.md.
#
#   docker buildx bake -f deploy/docker/docker-bake.hcl
#
# Rust rebuilds are fast because of how the cache is laid out:
#
#   planner    reduces the workspace to recipe.json, which only changes with the dependencies
#   deps-*     compiles the dependencies, and is reused until recipe.json changes
#   build-*    compiles the workspace on top, and is all that runs after a source change
#
# `rusteron-archive` runs Gradle inside its own crate directory and leaves jars there, so
# registry/src stays in the deps layer beside target/. If it were a cache mount, losing the mount
# would rerun Gradle and every build script even with the layer still cached. The mounts hold what's
# safe to lose: the index, downloaded crates, and Gradle's distribution and dependencies.
#
# Mounts are keyed by architecture so that amd64 and arm64 builds can run at the same time.

ARG RUST_IMAGE=docker.io/library/rust:1.99.1-slim-trixie
ARG DEBIAN_IMAGE=docker.io/library/debian:trixie-slim
ARG NODE_IMAGE=docker.io/library/node:24-slim
ARG TARGETARCH

# ---- Rust toolchain ------------------------------------------------------------------------------

FROM ${RUST_IMAGE} AS toolchain-base
ARG CARGO_CHEF_VERSION=0.1.78
# rusteron needs CMake 3.30+, clang and libclang for bindgen, rustfmt for its generated bindings, a
# JDK for Gradle, and git for the version stamp in Aeron's CMake build.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates clang cmake g++ git libatomic1 libbsd-dev libclang-dev make \
        openjdk-21-jdk-headless pkg-config uuid-dev zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
ENV CARGO_TERM_COLOR=never
ENV GRADLE_OPTS=-Dorg.gradle.daemon=false
WORKDIR /src

# Without RUSTERON_C_MARCH, Aeron's C code is built with -march=native, and the image then crashes
# with SIGILL on any node older than the machine that built it.
FROM toolchain-base AS toolchain-amd64
ARG AMD64_MARCH=x86-64-v3
ENV RUSTERON_C_MARCH=${AMD64_MARCH}

FROM toolchain-base AS toolchain-arm64
ARG ARM64_MARCH=armv8.2-a
ENV RUSTERON_C_MARCH=${ARM64_MARCH}

FROM toolchain-${TARGETARCH} AS toolchain

# ---- Dependency recipe ---------------------------------------------------------------------------

FROM toolchain AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY services services
COPY tools/Cargo.toml tools/Cargo.toml
COPY tools/src tools/src
RUN cargo chef prepare --recipe-path /recipe.json

# ---- Services and tools: the release profile, with features unified across the workspace ---------

FROM toolchain AS deps-release
ARG TARGETARCH
COPY --from=planner /recipe.json /recipe.json
RUN --mount=type=cache,id=mmx-cargo-index-${TARGETARCH},target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,id=mmx-cargo-cache-${TARGETARCH},target=/usr/local/cargo/registry/cache,sharing=locked \
    --mount=type=cache,id=mmx-cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,id=mmx-gradle-${TARGETARCH},target=/root/.gradle,sharing=locked \
    cargo chef cook --release --workspace --locked --recipe-path /recipe.json

FROM deps-release AS sources-release
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY services services
COPY tools/Cargo.toml tools/Cargo.toml
COPY tools/src tools/src

# Building the whole workspace once means one Cargo run per architecture however many images are
# built, and one set of unified features, so the dependencies above are always reused. Incremental
# output sits in a cache mount because it's only an optimization: rustc discards it if it's stale.
# Set CARGO_INCREMENTAL=0 for published images, which gives slightly faster code.
FROM sources-release AS build-release
ARG TARGETARCH
ARG CARGO_INCREMENTAL=1
RUN --mount=type=cache,id=mmx-cargo-index-${TARGETARCH},target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,id=mmx-cargo-cache-${TARGETARCH},target=/usr/local/cargo/registry/cache,sharing=locked \
    --mount=type=cache,id=mmx-cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,id=mmx-incremental-${TARGETARCH},target=/src/target/release/incremental,sharing=locked \
    env CARGO_INCREMENTAL=${CARGO_INCREMENTAL} cargo build --release --workspace --bins --locked
RUN mkdir /out && find target/release -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

FROM sources-release AS test
ARG TARGETARCH
ARG CARGO_INCREMENTAL=1
RUN --mount=type=cache,id=mmx-cargo-index-${TARGETARCH},target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,id=mmx-cargo-cache-${TARGETARCH},target=/usr/local/cargo/registry/cache,sharing=locked \
    --mount=type=cache,id=mmx-cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,id=mmx-incremental-${TARGETARCH},target=/src/target/release/incremental,sharing=locked \
    env CARGO_INCREMENTAL=${CARGO_INCREMENTAL} cargo test --release --workspace --locked

# ---- Engine: built like the benchmark, with fat LTO and logging compiled out ---------------------

FROM toolchain AS deps-engine
ARG TARGETARCH
COPY --from=planner /recipe.json /recipe.json
RUN --mount=type=cache,id=mmx-cargo-index-${TARGETARCH},target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,id=mmx-cargo-cache-${TARGETARCH},target=/usr/local/cargo/registry/cache,sharing=locked \
    --mount=type=cache,id=mmx-cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,id=mmx-gradle-${TARGETARCH},target=/root/.gradle,sharing=locked \
    cargo chef cook --profile dist --package matching-engine --features silent --locked \
        --recipe-path /recipe.json

FROM deps-engine AS build-engine
ARG TARGETARCH
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY services services
COPY tools/Cargo.toml tools/Cargo.toml
COPY tools/src tools/src
RUN --mount=type=cache,id=mmx-cargo-index-${TARGETARCH},target=/usr/local/cargo/registry/index,sharing=locked \
    --mount=type=cache,id=mmx-cargo-cache-${TARGETARCH},target=/usr/local/cargo/registry/cache,sharing=locked \
    --mount=type=cache,id=mmx-cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db,sharing=locked \
    cargo build --profile dist --package matching-engine --features silent --locked

# ---- Runtime images ------------------------------------------------------------------------------

FROM ${DEBIAN_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libatomic1 libbsd0 libuuid1 \
        util-linux zlib1g \
    && rm -rf /var/lib/apt/lists/*
COPY --chmod=755 deploy/docker/mmx-lib.sh /usr/local/lib/mmx/mmx-lib.sh
COPY --chmod=755 deploy/docker/mmx-entrypoint.sh /usr/local/bin/mmx-entrypoint
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/mmx-entrypoint"]

# Any Rust service. Adding one to the workspace only needs a line in docker-bake.hcl.
FROM runtime AS service
ARG SERVICE
COPY --from=build-release /out/${SERVICE} /usr/local/bin/service
RUN if ldd /usr/local/bin/service | grep "not found"; then exit 1; fi
CMD ["/usr/local/bin/service"]

FROM runtime AS matching-engine
COPY --from=build-engine /src/target/dist/matching-engine /usr/local/bin/matching-engine
RUN if ldd /usr/local/bin/matching-engine | grep "not found"; then exit 1; fi
CMD ["/usr/local/bin/matching-engine"]

# Every developer tool, for inspecting a running cluster.
FROM runtime AS tools
COPY --from=build-release /out/ /usr/local/bin/
RUN if ldd /usr/local/bin/sbx-tap | grep "not found"; then exit 1; fi
ENTRYPOINT []
CMD ["sleep", "infinity"]

# The local copy of this jar matches the SHA-1 Maven Central publishes, and this is its SHA-256.
FROM ${DEBIAN_IMAGE} AS aeron-jar
ARG AERON_VERSION=1.54.0
ARG AERON_JAR_SHA256=eed9fad510778af55c69cbe5b43ddc7ba754f79f8d592268c63fc3ad9d25707b
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && curl --fail --silent --show-error --location --output /aeron-all.jar \
        "https://repo1.maven.org/maven2/io/aeron/aeron-all/${AERON_VERSION}/aeron-all-${AERON_VERSION}.jar" \
    && echo "${AERON_JAR_SHA256}  /aeron-all.jar" | sha256sum --check -

# A cluster member, or with the `driver` command, the media driver beside a gateway or consumer.
FROM ${DEBIAN_IMAGE} AS node
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openjdk-21-jre-headless util-linux \
    && rm -rf /var/lib/apt/lists/*
COPY --from=aeron-jar /aeron-all.jar /opt/aeron/aeron-all.jar
COPY --chmod=755 deploy/docker/mmx-lib.sh /usr/local/lib/mmx/mmx-lib.sh
COPY --chmod=755 deploy/docker/mmx-node.sh /usr/local/bin/mmx-node
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/mmx-node"]
CMD ["cluster"]

# ---- Node.js applications ------------------------------------------------------------------------

FROM ${NODE_IMAGE} AS npm-deps
ARG APP
WORKDIR /app
COPY ${APP}/package.json ${APP}/package-lock.json ./
RUN --mount=type=cache,id=mmx-npm,target=/root/.npm,sharing=locked npm ci

# VITE_ variables are baked in at build time.
FROM npm-deps AS remix-build
ARG APP
ARG VITE_DOMAIN
ARG VITE_API_URL
ARG VITE_AUTH_URL
ARG VITE_WS_URL
ARG VITE_DOCS_URL
COPY ${APP}/ ./
RUN npm run build && npm prune --omit=dev

# `web` and `docs`.
FROM ${NODE_IMAGE} AS remix
ENV NODE_ENV=production PORT=3000
WORKDIR /app
COPY --from=remix-build /app/package.json ./
COPY --from=remix-build /app/node_modules ./node_modules
COPY --from=remix-build /app/build ./build
USER 10001:10001
EXPOSE 3000
CMD ["node_modules/.bin/remix-serve", "./build/server/index.js"]

# `auth`'s npm build also runs its formatter, so this calls the compiler directly.
FROM npm-deps AS auth-build
COPY auth/ ./
RUN npx tsc && npx tsc-alias && npm prune --omit=dev

FROM ${NODE_IMAGE} AS auth
ENV NODE_ENV=production
WORKDIR /app
COPY --from=auth-build /app/package.json ./
COPY --from=auth-build /app/node_modules ./node_modules
COPY --from=auth-build /app/dist ./dist
USER 10001:10001
CMD ["node", "dist/main.js"]
