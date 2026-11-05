# BEGIN PINNED AQ-GIT BUILD STAGES
# Verified release executable and pinned Git runtime for all consumer images.
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS aq-git-build
WORKDIR /src
COPY vendor/aq-git-source.tar.gz /tmp/source.tar.gz
RUN echo 'f64891f03e2d28263cf3af20b38123935ae08d6917699becb9d77e76af2fd3c6  /tmp/source.tar.gz' | sha256sum -c -
COPY vendor/aq-git-v0.2.0-linux-x86_64 /src/target/release/aq-git
RUN echo '3feca63cffa5065566f2c604fd00d4ad92e1bc367c62ec69bedd42c2b37f35d2  /src/target/release/aq-git' | sha256sum -c - && chmod 755 /src/target/release/aq-git
FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS git-build
RUN apt-get update && apt-get install -y --no-install-recommends libssl-dev libcurl4-openssl-dev libexpat1-dev gettext zlib1g-dev && rm -rf /var/lib/apt/lists/*
COPY vendor/aq-git-0.2.0/git-2.56.0.tar.xz /tmp/git.tar.xz
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

FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /build
ARG AQ_STATE_FORMAT
ARG AQ_SOURCE_REVISION=unknown
ENV AQ_SOURCE_REVISION=${AQ_SOURCE_REVISION}
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY vendor ./vendor
COPY protocol ./protocol
# The registry and target directory are BuildKit cache mounts, so rebuilds
# (another version or revision) recompile only the workspace crates. The
# cached target directory is not part of the image: copy the binaries out.
# The cache is shared by every build of this Dockerfile on the host (other
# branches and worktrees), and their workspace crates have identical artifact
# hashes, so cargo's mtime check can reuse another tree's build. Clean every
# workspace crate (dependencies stay cached) so the image is built from this
# context only.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=locked,target=/build/target \
    cargo clean --release -p aq-protocol -p aq-worker-protocol \
        $(for f in crates/*/Cargo.toml; do sed -n 's/^name *= *"\([^"]*\)".*/-p \1/p' "$f" | head -n 1; done) \
    && AQ_IMAGE_STATE_FORMAT="${AQ_STATE_FORMAT}" cargo build --locked --release --bin aq-server \
    && mkdir -p /out \
    && cp target/release/aq-server /out/

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcurl4 libexpat1 libssl3 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 aq && useradd --uid 10001 --gid aq --create-home --home-dir /home/aq aq

FROM runtime AS server
ARG AQ_SOURCE_REVISION=unknown
ARG AQ_VERSION=unknown
# The release's state format and newest worker protocol. release.yml passes
# them from the migration catalog and SUPPORTED_WORKER_PROTOCOLS and
# checks the pushed labels against the manifest; the updater refuses a
# server image whose state-format label disagrees with its manifest.
# Required: python3 scripts/migration_catalog.py (build.rs validates the value).
ARG AQ_STATE_FORMAT
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
COPY vendor/aq-dashboard-v0.1.0.tar.gz /tmp/dashboard.tar.gz
RUN echo '162f2cd29e7123570f4568df68ff2ffffa1c97417c1f51369fcd55b1c7bd2465  /tmp/dashboard.tar.gz' | sha256sum -c - \
    && mkdir -p /usr/share/agent-q/dashboard \
    && tar -xzf /tmp/dashboard.tar.gz -C /usr/share/agent-q/dashboard \
    && rm /tmp/dashboard.tar.gz
COPY vendor/dashboard-candidate.json /usr/share/agent-q/dashboard-candidate.json
ENV AQ_DASHBOARD_DIR=/usr/share/agent-q/dashboard
ENV AQ_VAULT_DIR=/vault
USER 10001:10001
EXPOSE 8080 8081
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=5 CMD ["aq-server", "healthcheck"]
ENTRYPOINT ["aq-server"]
CMD ["serve", "--vault", "/vault"]
