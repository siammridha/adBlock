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
COPY config.toml /app/config.toml
# Ship the pre-generated scriptlet library so scriptlets work out of the box.
# Everything else the proxy persists (blocklists, settings, logs, certs) is
# created at runtime under /app/data. Blocklists download on first start.
COPY lists/scriptlets.json /app/data/scriptlets/scriptlets.json

# config.toml binds 127.0.0.1 for local dev; inside a container loopback is
# unreachable from a published port or a reverse proxy. Rebind to 0.0.0.0.
RUN sed -i 's/127\.0\.0\.1:/0.0.0.0:/g' /app/config.toml

# proxy + admin dashboard.
EXPOSE 8080 8081

# Persist across restarts: blocklists, settings, logs, scriptlets, and managed
# CAs all live under this one data root.
VOLUME ["/app/data"]

# --- MITM signing CA (REQUIRED) --------------------------------------------
# The proxy signs per-host leaf certs with ca_cert/ca_key (config.toml →
# /app/ca-cert.pem, /app/ca-key.pem) and REFUSES TO START if they're missing —
# it never generates its own CA. Bind-mount a signer at run time, e.g. a step-ca
# (or other private-PKI) intermediate so forged leaves chain to a root your
# devices already trust:
#   -v ./ca-cert.pem:/app/ca-cert.pem:ro -v ./ca-key.pem:/app/ca-key.pem:ro
# (A self-signed root works too — mount it the same way and install it in each
# client's trust store.)

# --- Upstream (egress) trust -----------------------------------------------
# To trust a private/corporate CA on the *upstream* side (fixes UnknownIssuer
# when your network does TLS inspection) — distinct from the MITM CA above —
# mount the PEM and rebuild the store:
#   docker run -v ./corp-ca.crt:/usr/local/share/ca-certificates/corp.crt ... \
#     sh -c 'update-ca-certificates && proxy /app/config.toml'

ENTRYPOINT ["proxy"]
CMD ["/app/config.toml"]
