# BEGIN PINNED AQ-GIT BUILD STAGES
# Standalone pinned source build, also usable as a stage in server/worker images.
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS aq-git-build
WORKDIR /src
COPY vendor/aq-git-source.tar.gz /tmp/source.tar.gz
RUN echo '8f6a2f8ce04361c3cabaff5de7274505e28a042235da18cc346086f47e4c6cd5  /tmp/source.tar.gz' | sha256sum -c - && tar -xzf /tmp/source.tar.gz -C /src
RUN cargo build --locked --release -p aq-git-cli
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS git-build
RUN apt-get update && apt-get install -y --no-install-recommends libssl-dev libcurl4-openssl-dev libexpat1-dev gettext zlib1g-dev && rm -rf /var/lib/apt/lists/*
COPY vendor/aq-git-0.1.0/git-2.56.0.tar.xz /tmp/git.tar.xz
RUN echo '26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3  /tmp/git.tar.xz' | sha256sum -c - && mkdir /git && tar -xf /tmp/git.tar.xz -C /git --strip-components=1
WORKDIR /git
RUN make -j4 prefix=/opt/aq-git-runtime RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease all && make prefix=/opt/aq-git-runtime RUNTIME_PREFIX=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease install
# END PINNED AQ-GIT BUILD STAGES

# Release image: only the signed release manifest and its signature bundle.
# Built with a context holding just those two files:
#   docker build --target release -f Dockerfile <dir with both files>
FROM scratch AS release
ARG AQ_SOURCE_REVISION=unknown
ARG AQ_VERSION=unknown
LABEL org.opencontainers.image.title="agent-q-release" \
      org.opencontainers.image.revision="${AQ_SOURCE_REVISION}" \
      org.opencontainers.image.version="${AQ_VERSION}"
COPY manifest.json manifest.sigstore.json /

FROM rust:1.96.1-bookworm AS build
WORKDIR /build
ARG AQ_SOURCE_REVISION=unknown
ENV AQ_SOURCE_REVISION=${AQ_SOURCE_REVISION}
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY vendor ./vendor
COPY protocol ./protocol
# The registry and target directory are BuildKit cache mounts, so rebuilds
# (another version or revision) recompile only the workspace crates. The
# cached target directory is not part of the image: copy the binaries out.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=locked,target=/build/target \
    cargo clean --release -p aq-server -p aq-release -p aq-protocol -p aq-worker-protocol \
    && cargo build --locked --release --bin aq-server \
    && mkdir -p /out \
    && cp target/release/aq-server /out/

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcurl4 libexpat1 libssl3 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 aq && useradd --uid 10001 --gid aq --create-home --home-dir /home/aq aq

FROM runtime AS server
ARG AQ_SOURCE_REVISION=unknown
ARG AQ_VERSION=unknown
# The release's state format and newest worker protocol. release.yml passes
# them from the source (STATE_FORMAT_VERSION, SUPPORTED_WORKER_PROTOCOLS) and
# checks the pushed labels against the manifest; the updater refuses a
# server image whose state-format label disagrees with its manifest.
ARG AQ_STATE_FORMAT=12
ARG AQ_PROTOCOL_MAX=1
LABEL org.opencontainers.image.title="agent-q-server" \
      org.opencontainers.image.revision="${AQ_SOURCE_REVISION}" \
      org.opencontainers.image.version="${AQ_VERSION}" \
      io.agent-q.protocol-max="${AQ_PROTOCOL_MAX}" \
      io.agent-q.state-format="${AQ_STATE_FORMAT}"
COPY --from=build /out/aq-server /usr/local/bin/aq-server
COPY --from=aq-git-build /src/target/release/aq-git /usr/local/bin/aq-git
COPY --from=git-build /opt/aq-git-runtime /opt/aq-git-runtime
RUN ln -s /opt/aq-git-runtime/bin/git /usr/bin/git
COPY vendor/git-candidate.json /usr/share/agent-q/git-candidate.json
ENV PATH="/opt/aq-git-runtime/bin:${PATH}"
ENV AQ_VAULT_DIR=/vault
USER 10001:10001
EXPOSE 8080 8081
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=5 CMD ["aq-server", "healthcheck"]
ENTRYPOINT ["aq-server"]
CMD ["serve", "--vault", "/vault"]
