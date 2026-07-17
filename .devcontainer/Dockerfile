# The plain Debian devcontainer base (what devcontainers/rust builds on), so we
# don't inherit the rust image's metadata that force-installs VS Code
# extensions (rust-analyzer, CodeLLDB, Even Better TOML).
FROM mcr.microsoft.com/devcontainers/base:bullseye
# Toolchain deps the base image lacks: a C linker for rustc, plus pkg-config /
# OpenSSL headers commonly needed by Rust crates.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
# Rust via rustup, in the same shared paths the official rust image uses (so
# the cargo-cache volume mount in devcontainer.json keeps working).
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --default-toolchain stable --profile minimal \
        -c rustfmt -c clippy -c rust-src -c rust-analyzer \
    && chown -R vscode:vscode /usr/local/cargo /usr/local/rustup

# Node.js 24 (Claude Code needs Node). NodeSource's "nodistro" repo works on bullseye.
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g npm@11.16.0 \
    && rm -rf /var/lib/apt/lists/*

# Keep ALL Claude state (including what would otherwise be ~/.claude.json) in
# one directory, so a single volume mount persists login + sessions.
ENV CLAUDE_CONFIG_DIR=/home/vscode/.claude

# The native installer is a PER-USER install: it drops the binary in
# ~/.local/bin for whoever runs it. So Claude Code (and the user-scope plugin
# below) are installed as the vscode user, and ~/.local/bin is put on PATH.
# Updates happen via `claude update` / the background auto-updater, NOT apk.
ENV PATH=/home/vscode/.local/bin:$PATH
USER vscode

# Claude Code via Anthropic's native installer. Needs bash + the musl runtime
# deps (libgcc, libstdc++, ripgrep) installed in the first apk layer above.
RUN curl -fsSL https://claude.ai/install.sh | bash

# The rust-analyzer-lsp plugin is user-scope (CLAUDE_CONFIG_DIR), so it's
# installed here as the same vscode user that owns the Claude Code binary.
RUN claude plugin marketplace add anthropics/claude-plugins-official && \
    claude plugin install rust-analyzer-lsp@claude-plugins-official || \
    echo "Warning: plugin install failed, continuing build"
USER root