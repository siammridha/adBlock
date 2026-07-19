# Architecture Audit — 2026-07-19

This is an audit of the code in `src/` against the rules in
[ARCHITECTURE.md](ARCHITECTURE.md). It covers three things: how each piece of
functionality is implemented today, where the rules are broken, and what to do
about it.

## Verdict in one paragraph

The module split exists and the call directions are right. Nothing imports the
web module. Adblock does not import proxy or DNS. Proxy resolves through DNS,
which the architecture now allows, and DNS never calls back into proxy. Most
validation lives in the owning module. But there are two shared modules
(`support/` and `net/`) that every module depends on, the web module
implements real functionality (service start/stop, settings persistence,
input validation), modules use each other's internals freely, and nothing
enforces any of the rules in the build.

---

## 1. How things are implemented today

### Module inventory

| Area | Where it lives today | Owner per ARCHITECTURE.md | OK? |
| --- | --- | --- | --- |
| Filter rules, blocklists, rule tester | `src/adblock/` (mod, store, curation, maintenance, updater, scriptlets) | Adblock | Yes |
| Blocklist/scriptlet downloading | `src/net/http_client.rs`, driven by `adblock/maintenance.rs` and `adblock/updater.rs` | Adblock | No — fetching is shared code |
| HTTP(S) proxying, MITM, CA | `src/proxy/` (server, pipeline, ca, certs, blackhole, html) | Proxy | Yes |
| Excluded hosts | `src/proxy/exclusions.rs` | Proxy | Yes |
| Proxy egress settings (`proxy-settings.json`) | `src/net/egress.rs` | Proxy | No — lives outside proxy |
| Request/response body capture | `src/proxy/capture.rs` writes into `stats` record types; decode is in proxy, called by web | Stats (log data) | Split across three modules |
| DNS server, upstreams, cache, rewrites | `src/dns/` | DNS | Yes |
| DNS runtime settings + validation | `src/dns/settings.rs` | DNS | Yes |
| Counters, event log, request/query logs, log files | `src/stats/` | Stats | Yes |
| Stats exclusion list | `src/stats/exclude.rs` | Stats | Yes |
| Admin HTTP routes + dashboard | `src/web/` | Web App | Yes |
| Proxy/DNS start, stop, rebind + `server-settings.json` | `src/web/runtime.rs` | Proxy / DNS | No — web implements it |
| Config file schema for ALL modules | `src/support/config.rs` | Each module | No — one shared schema |
| Error type for ALL modules | `src/support/error.rs` | Each module | No — shared |
| Persistence helpers (`OverrideStore`, `PersistedSet`) | `src/support/persist.rs` | Each module | No — shared |
| Data directory layout (`blocklists/`, `settings/`, `logs/`, `certs/`, `scriptlets/`) | `AdblockConfig` methods + `main.rs` creates the dirs and picks the file names | Each module | No — adblock config owns everyone's storage |

### What is genuinely good

- **Call directions.** The grep of every `crate::` import shows: web calls all
  four modules; proxy and DNS call adblock; adblock, proxy, and DNS call stats;
  proxy resolves hostnames through DNS (allowed, one-directional). No module
  imports web. Adblock imports neither proxy nor DNS. DNS never calls proxy.
  This matches the allowed graph.
- **Validation mostly sits with the owner.** DNS setting rules
  (`min_ttl <= max_ttl`, upstream list not empty) are checked in
  `dns/settings.rs`. Rewrite entries are validated in `dns/rewrites.rs`.
  Filter rules are parsed by adblock. Stats retention is validated in stats.
  The web handlers pass raw input down and render the error that comes back.
- **`main.rs` is close to pure wiring.** It loads config, builds each module,
  and hands them to each other. Its only real sins are creating every module's
  directories and choosing every module's settings file names.

---

## 2. Violations, ranked

### V1. `support/` is a shared module used by everything

Rule broken: "No shared functionality, no shared settings, no shared state."

- `support::config` is one global config schema. `DnsConfig`, `AdblockConfig`,
  `TlsConfig`, `LoggingConfig` all live here, outside the modules that own
  them. It also validates DNS settings (`config.rs:154-163`), which is DNS's
  job.
- `support::persist` (`OverrideStore`, `PersistedSet`) is imported by proxy,
  dns, stats, net, and web.
- `support::error` is one error type shared by all modules.

### V2. `net/` is a shared networking module used by everything

Rule broken: "if every module needs to fetch something from the internet,
every module implements its own fetching."

- `net::http_client::HttpClient` is used by adblock (blocklists, scriptlets),
  proxy (upstream requests), and dns (DoH upstream).
- `net::target` (host/port/URL parsing) is used by proxy and web.

### V3. The Proxy → DNS call is routed through shared code, not proxy's own

Rule broken: "Its own settings and their persistence" (Proxy) and "No shared
functionality, no shared settings, no shared state."

The Proxy → DNS edge itself is now allowed ("Proxy resolves through the DNS
module rather than implementing its own resolver"). The problem is where the
code sits. The resolving side lives in `net/egress.rs`, a shared module,
instead of inside proxy:

- `EgressPolicy` persists `proxy-settings.json` — proxy settings living in
  `net/`.
- The `DnsSlot` handle inside it (a shared `RwLock` slot holding the DNS
  service) is shared mutable state wired at the top level, rather than proxy
  holding the DNS API handle itself.
- The doc says the resolution call must be "a call into DNS's exposed API like
  any other"; today it goes through this shared intermediary that also serves
  adblock's HTTP client.

### V4. The web module implements functionality

Rule broken: "The Web App implements no functionality of its own."

- `web/runtime.rs` (505 lines) starts, stops, and rebinds the proxy and DNS
  listeners, constructs `DnsService` instances, owns and persists
  `server-settings.json`, and validates listen addresses. This is service
  lifecycle management — it belongs to proxy and DNS, behind their APIs.
- Command parsing in `web/blocklists.rs`, `web/dns.rs`, `web/exclusions.rs`
  validates input: it rejects a delete without a name, rejects a rewrite
  without both fields, rejects a bad `upstream_mode` with the message
  "upstream_mode must be failover…" (`web/dns.rs:146-151`), trims and filters
  upstream lists, and normalizes domains (lowercase, strip trailing dot) in
  `check_dns_rule` before calling adblock. Per the rules, the owning module
  should accept the raw input and return these errors.
- `web/logs.rs` calls `proxy::capture::decode_captured` to decompress stored
  bodies — business logic invoked from web via another module's internals.

### V5. No explicit API surface; internals are used freely

Rule broken: "Each module has an explicit public API surface. Everything not
part of it is private and unreachable from outside the module."

`lib.rs` makes every submodule public, so nothing is unreachable. Actual deep
imports today: web imports `adblock::maintenance`, `adblock::updater`,
`proxy::certs`, `proxy::exclusions`, `proxy::capture`, `stats::history`;
proxy and dns import `stats::history`; `main.rs` imports deep paths from five
modules. No module marks anything private (`pub(crate)`) toward its siblings,
and there is no defined API facade anywhere.

### V6. Nothing enforces the rules

Rule broken: "The allowed dependency graph is enforced by the linter/build."

There is no CI check, no lint, and no crate boundary. Every one of the
violations above compiles cleanly today.

### V7. Stats holds other modules' data; storage layout is centralized

- `stats::StaticInfo` stores the proxy's CA PEM and listen addresses, and the
  web serves `/ca-cert.pem` out of stats (`web/meta.rs`). The CA belongs to
  proxy; web should get it from proxy's API.
- `config.adblock.data_dir` defines the storage layout for every module
  (`logs/` for stats, `certs/` for proxy, one shared `settings/` dir for all).
  `main.rs` creates the directories and picks settings file names
  (`excluded-domains.conf`, `proxy-settings.json`, `server-settings.json`,
  `active-ca.json`) for the modules. Each module should own its storage.
- Body-capture data is written by proxy into stats record types and decoded by
  proxy code called from web. One module should own that format end to end.

---

## 3. What to do

Ordered so each step stands alone. Rough size in parentheses.

**Step 1 — Move egress into proxy (small).**
Move `net/egress.rs` to `proxy/egress.rs`. It persists proxy settings and is
used on the proxy's request path. Proxy then holds the DNS API handle directly
(injected at wiring time) instead of sharing a `DnsSlot` through `net/`. This
turns the hidden Proxy → DNS call into the plain API call the doc describes.

**Step 2 — Dissolve `net/` (medium).**
Copy `http_client.rs` (and the parts of `target.rs` each user needs) into
adblock, proxy, and dns. Yes, three copies — the architecture explicitly
prefers duplication over sharing. Delete `src/net/`.

**Step 3 — Dissolve `support/` (medium).**
- Each module gets its own config struct and validates its own section.
  Root-level code parses the TOML file and hands each module its raw section —
  that is wiring, so it may stay at the root.
- Copy `persist.rs` into each module that uses it.
- Each module defines its own error type. The root maps them for exit codes.

**Step 4 — Move runtime control out of web (medium).**
- Proxy exposes a settings API (enabled, listen address, plus a status read).
  Applying a settings update is what starts, stops, or rebinds the listener —
  that logic lives inside proxy, not in the caller. Proxy validates the input
  and persists its own settings.
- DNS exposes the same kind of settings API for itself, with the same rule:
  updating the settings is the thing that starts, stops, or rebinds the DNS
  server.
- `server-settings.json` splits into each module's own settings file.
- Web keeps only the `/api/server/config` route. It hands the raw update to
  the two settings APIs and renders whatever comes back. It never decides
  when to start or stop anything.

**Step 5 — Move command parsing into the owning modules (medium).**
`BlocklistCommand` → adblock. `RewriteCommand`, `DnsConfigCommand` → dns.
`ExclusionCommand` → proxy. Stats-config parsing → stats. Web hands the module
the raw body bytes (or parsed JSON value) and renders the structured
success/error that comes back. The existing web tests mostly survive as module
API tests.

**Step 6 — Fix data ownership (small).**
- Drop `ca_pem` from `StaticInfo`; web serves `/ca-cert.pem` from a proxy API.
- Give each module a `data_dir` in its own config; each module creates its own
  directories and names its own files. `main.rs` stops doing both.
- Pick one owner for body capture. Simplest: proxy captures and submits
  finished records through the stats API, and stats owns storage *and* the
  decode-on-demand logic; web calls a stats API to decode.

**Step 7 — Explicit API surface per module + enforcement (medium, do last).**
The Enforcement section asks for the language's visibility rules where
possible, lint rules where not. Chosen approach: stay in one crate.

- Each top-level module gets an `api` module (`adblock::api`, `proxy::api`,
  `dns::api`, `stats::api`) that re-exports its public surface. Everything
  else in the module becomes private to the module (private submodules, or
  `pub(in crate::module)` where a submodule needs a sibling).
- Rust visibility alone cannot stop one module from importing another
  module's `pub(crate)` items, so the dependency graph gets a lint: a CI
  check that fails on any cross-module `use` path that is not
  `<module>::api` and not an edge on the allowed graph. A simple script or
  `cargo-modules` in CI is enough.
- A Cargo workspace with one crate per module would enforce all of this via
  the compiler, but it is a bigger restructuring and is not required by the
  doc. Not chosen for now.

---

## 4. Rule-by-rule scorecard

| ARCHITECTURE.md rule | Status |
| --- | --- |
| Modules interact only through exposed APIs | Broken (V5) |
| No shared utils/config/state | Broken (V1, V2, V3) |
| No top-level code implements functionality | Mostly OK (dir creation + file naming in `main.rs`, V7) |
| Validation lives with the data owner | Mostly OK (web command parsing, V4) |
| Web App implements nothing | Broken (V4) |
| Allowed dependency directions | OK — all edges, including Proxy → DNS, are on the allowed list; but the Proxy → DNS call goes through shared `net/` code instead of a direct API call (V3) |
| Explicit public API surface per module | Broken (V5) |
| Graph enforced by linter/build | Missing (V6) |
