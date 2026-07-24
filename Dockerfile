# syntax=docker/dockerfile:1
# Multi-stage build for proxy (ad/tracker-blocking proxy, no native deps).
# The build context is pruned by .dockerignore (keeps target/, CA material, etc.
# out of the builder and the image).

# ---- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release

# ---- LLRT (scriptlet-refresh runtime) --------------------------------------
# The admin UI's "Update from uBO" button and the hourly auto-updater
# regenerate data/scriptlets/scriptlets.json by running tools/convert-ubo-scriptlets.mjs,
# which must *evaluate* uBO's ESM modules — a JS job. We ship AWS LLRT (a ~14 MB
# QuickJS runtime) instead of full Node (~40-150 MB). Arch-matched to the build
# platform; override the pin with --build-arg LLRT_VERSION=...
FROM debian:bookworm-slim AS llrt
ARG TARGETARCH
ARG LLRT_VERSION=v0.8.1-beta
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && case "$TARGETARCH" in \
         arm64) A=arm64 ;; \
         amd64) A=x64 ;; \
         *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && curl -fsSL -o /usr/local/bin/llrt \
         "https://github.com/awslabs/llrt/releases/download/${LLRT_VERSION}/llrt-container-${A}" \
    && chmod +x /usr/local/bin/llrt \
    && rm -rf /var/lib/apt/lists/*

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim
WORKDIR /app

# ca-certificates: REQUIRED. Upstream TLS verification trusts this store; an
# empty one falls back to the compiled-in Mozilla bundle only (public sites
# work, but any private/corporate CA presents as UnknownIssuer).
# tar: used by the scriptlet updater to unpack the uBO tarball (the download
# itself goes through the proxy's own HTTP client, so no curl is needed here).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates tar gzip \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/proxy /usr/local/bin/proxy
# LLRT + the converter script: together they let the scriptlet library be
# refreshed from uBO at runtime, inside the container.
COPY --from=llrt /usr/local/bin/llrt /usr/local/bin/llrt
COPY tools /app/tools
# Ship the pre-generated scriptlet library so scriptlets work out of the box.
# It lives under data/ now (the old top-level lists/ dir is gone). Everything
# else the proxy persists (blocklists, settings, logs, certs) is created at
# runtime under /app/data. Blocklists download on first start.
COPY data/scriptlets/scriptlets.json /app/data/scriptlets/scriptlets.json

# Each module reads its own base-config file under
# /app/data/settings/<module>-base.toml (or its built-in defaults).
# The container needs one non-default value: bind on all interfaces instead of
# loopback, so a published port / reverse proxy can reach the proxy, admin UI,
# and DNS. Bake the two modules that have a listener; adblock and stats use
# their defaults. A named data volume inherits these on first run, like the
# scriptlet library above.
RUN mkdir -p /app/data/settings \
 && printf '[server]\nlisten = "0.0.0.0:8080"\nadmin_listen = "0.0.0.0:8081"\n\n[tls]\nca_cert = "data/certs/ca-cert.pem"\nca_key = "data/certs/ca-key.pem"\n' \
      > /app/data/settings/proxy-base.toml \
 && printf '[dns]\nlisten = "0.0.0.0:53"\n' \
      > /app/data/settings/dns-base.toml

# proxy + admin dashboard.
EXPOSE 8080 8081

# Persist across restarts: blocklists, settings, logs, scriptlets, and managed
# CAs all live under this one data root.
VOLUME ["/app/data"]

# --- MITM signing CA (REQUIRED) --------------------------------------------
# The proxy signs per-host leaf certs with ca_cert/ca_key (default paths
# /app/data/certs/ca-cert.pem, /app/data/certs/ca-key.pem) and REFUSES TO START
# if they're missing — it never generates its own CA. Bind-mount a signer at run
# time, e.g. a step-ca (or other private-PKI) intermediate so forged leaves
# chain to a root your devices already trust:
#   -v ./ca-cert.pem:/app/data/certs/ca-cert.pem:ro -v ./ca-key.pem:/app/data/certs/ca-key.pem:ro
# (A self-signed root works too — mount it the same way and install it in each
# client's trust store.)

# --- Upstream (egress) trust -----------------------------------------------
# To trust a private/corporate CA on the *upstream* side (fixes UnknownIssuer
# when your network does TLS inspection) — distinct from the MITM CA above —
# mount the PEM and rebuild the store. ENTRYPOINT is "proxy", so override it to
# get a shell (otherwise the args are passed to proxy, not run as a command):
#   docker run --entrypoint sh \
#     -v "$(pwd)/corp-ca.crt:/usr/local/share/ca-certificates/corp.crt" ... proxy:latest \
#     -c 'update-ca-certificates && proxy'
#
# Note: /app/data is a VOLUME. A single-file bind mount at
# /app/data/certs/ca-cert.pem still lands correctly on top of that volume.

# proxy reads each module's base-config file under /app/data/settings, or its
# built-in defaults.
ENTRYPOINT ["proxy"]
