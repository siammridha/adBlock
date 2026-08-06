- When reporting back to me from now on explain things in plain, simple language. Use short sentences and everyday words. Avoid unnecessary detail, buzzwords, overly formal language, metaphors, analogies, and figures of speech. Be direct, clear, and literal.

- Do not add Co-Authored-By trailers on this repo.
- Use cargo nextest run insted of cargo test.
- For browser and end-to-end testing use agent-browser (the Rust CLI, `cargo install agent-browser`) driving the container's system Chromium at `/usr/bin/chromium`. Chrome for Testing has no Linux arm64 build, so the system Chromium is used with `--no-sandbox`. The browser test is `e2e/browser-test.sh`; it drives the already-running proxy on 127.0.0.1:8080/8081 and restores every setting it touches. Rust `cargo nextest` still covers proxy, adblock and DNS logic — the browser test only adds a browser layer, it does not replace the Rust tests.
- Use task management skills to impliment features.
- Ensure all changes are clean, don't leave behind dangling code, unused variables, or stale configuration.
- Before making any commit, make sure to update all relevant documentation so it accurately reflects the current changes.
- When a question is asked just answer the question so that I can make an informative decission. Do not start planing and exicuting.
- Write brief comments only when they add context that isn't obvious from the code itself. Don't use comments to describe what the code does.
- Treat everything I report seeing as accurate. Never question or contradict my observations. Instead, do everything you can to accurately explain what I saw, including any plausible technical or UI behavior that could account for it.

## Architecture boundaries (non-negotiable)

Modules: `adblock/`, `proxy/`, `dns/`, `stats/`, `tester/`, `webapp/`.

- Each module owns its own settings, storage, validation, and network access.
- Modules interact ONLY through each other's exposed APIs. Never import another module's internals.
- No shared utils, no shared config, no shared state. Duplicate code instead.
- No top-level code implements functionality — root only wires modules together.
- `webapp/` implements nothing. It instantiates modules and calls their APIs. It never validates input; the owning module validates and returns success/error.
- `proxy/` never changes a request or a response. No URL rewriting, no filtering headers, no HTML edits, no script or style injection, no stand-in bodies. It passes each request and each response to the `adblock/` API and forwards exactly what comes back. Setting hop-by-hop headers, `Host`, and TLS termination is the proxy's own job and is fine.
- `adblock/` applies every rule and makes every change to a request or response body or URL. The switches for those changes (`$redirect`, `$removeparam`, `$csp`, cosmetic filtering, scriptlets) live in `adblock/`, not in the caller.
- `tester/` is the rule-type test page. It calls no module, not even `adblock/` — every verdict is reached in the browser, so the page reports on whichever blocker is active (this proxy, an extension, or none). The web app serves it and nothing else.

Before adding code, ask: which module owns this? Put it there. Full rationale: @docs/ARCHITECTURE.md