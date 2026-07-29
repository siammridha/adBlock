# Architecture Audit — closed

The audit of 2026-07-19 found seven violations of [ARCHITECTURE.md](ARCHITECTURE.md)
and laid out seven steps to fix them. All seven are done. This page records what
changed and what is left, so the audit does not have to be redone from scratch.

Re-audited: 2026-07-29.

## What changed

| Was | Now |
| --- | --- |
| `support/` — one config schema, one error type, one persistence helper, used by everything | Gone. Each module has its own `config.rs`, `error.rs`, and `persist.rs`. |
| `net/` — one HTTP client and one URL parser, used by everything | Gone. Proxy has `http_client.rs` + `target.rs`, adblock has `fetch.rs`, DNS speaks DoH itself in `upstream.rs`. |
| `net/egress.rs` held proxy settings and a shared `DnsSlot` | `proxy/egress.rs`. Proxy holds the DNS handle directly, injected at wiring time. |
| `web/runtime.rs` started/stopped/rebound both listeners and owned `server-settings.json` | `proxy/control.rs` and `dns/control.rs`, each behind its module's settings API, each with its own settings file. Web only routes `/api/server/config`. |
| Web parsed and validated commands (blocklists, rewrites, upstreams, exclusions) | `adblock/commands.rs`, `dns/commands.rs`, `proxy/certs.rs`, `proxy/exclusions.rs`, `stats/exclude.rs`. Web hands over raw bytes and renders the result. |
| `stats::StaticInfo` carried the proxy's CA PEM; web served `/ca-cert.pem` from stats | Proxy's `CertStore` owns it; web serves the download from the proxy API. |
| Body capture: written by proxy, decoded by proxy code called from web | Stats owns storage and decoding (`stats/decode.rs`, exposed as `BodyDecode`). |
| `config.adblock.data_dir` defined every module's storage layout; `main.rs` created the directories and named the files | Each module takes the data root and derives its own paths; each creates its own directories. |
| Every submodule public; deep cross-module imports everywhere | Each module exposes an `api` facade. Sibling modules and web import only through it. |
| Nothing enforced any of it | `tests/boundaries.rs` fails on any cross-module path that skips a facade or takes a disallowed edge. CI runs it via `scripts/check-boundaries.sh`. |

TOML base-config files were dropped along the way (they were the last place a
shared file format sat across modules). Each module now starts from built-in
defaults and layers its own persisted settings file over them.

## Scorecard

| ARCHITECTURE.md rule | Status |
| --- | --- |
| Modules interact only through exposed APIs | OK — enforced by `tests/boundaries.rs` |
| No shared utils/config/state | OK — `support/` and `net/` are gone |
| No top-level code implements functionality | OK — `main.rs` is wiring |
| Validation lives with the data owner | OK — commands parse and validate inside the owning module |
| Web App implements nothing | OK |
| Allowed dependency directions | OK — enforced |
| Explicit public API surface per module | OK — one `api` module each |
| Graph enforced by linter/build | OK — CI job "Module boundary lint" |

## Known, deliberate exceptions

These are wiring decisions, not violations, but they are the places to look
first if the rules seem to be bending:

- **The data root.** `main.rs` picks `data/` and hands it to every module. Each
  module then derives and creates its own subdirectories and names its own
  files. The root chooses where the tree starts; it does not define the layout.
- **`admin_listen`.** It belongs to the web app, which owns no settings, so the
  root holds it: built-in default plus the `PROXY_ADMIN_LISTEN` environment
  variable, validated in `main.rs`.
- **`web` has no `api` facade.** It implements nothing and nothing imports it,
  so there is nothing to hide.
- **Duplication is the point.** Three HTTP clients, three persistence helpers,
  five error types. Do not "clean this up" into a shared module.
