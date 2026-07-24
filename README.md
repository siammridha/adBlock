# proxy

An intercepting HTTP/HTTPS proxy in Rust that blocks ads and trackers:

- **Network filtering** using standard EasyList / uBlock-Origin filter lists
  (`||host^`, `/path`, typed and party-scoped rules). Always on when
  `adblock.enabled = true`; blocked requests drop the connection (network
  error, like a real ad blocker).
- **Cosmetic filtering** — injects element-hiding CSS from `##selector` rules
  into HTML pages to hide leftover ad containers.
- **Scriptlet injection** (opt-in) — resolves uBlock Origin `##+js(…)` rules
  against the full uBO scriptlet library and injects the resulting JS into HTML
  pages. Strips CSP so the injected script runs (a deliberate security
  tradeoff). See [Scriptlet injection](#scriptlet-injection).
- **CONNECT-stage host blocking** so whole-host rules apply even to
  certificate-pinned domains the proxy can't MITM.
- **Streams** everything it doesn't need to touch; only HTML is buffered (and
  only when there are cosmetic rules to apply).
- **Admin web dashboard** (separate port) with rolling 24 h stats (per-card
  sparklines, top queried / top blocked domains), a searchable request log,
  an event log, a rule tester, runtime blocklist editing, and a one-click
  root-CA download.

---

## Architecture

```
                 ┌─────────────────────────────────────────────┐
   client ─────► │  proxy::server                              │
   (browser,     │   ├─ CONNECT ─► host rule? ─► drop / MITM    │
    OS proxy)    │   │                 └─ terminate TLS (ca)    │
                 │   ├─ adblock::AdBlocker (network decision)   │
                 │   └─ response:                               │
                 │        ├─ HTML ─► inject cosmetic CSS        │
                 │        └─ else ─► stream through             │
                 └─────────────────────────────────────────────┘
                                     │
                                     ▼  re-originated TLS (webpki + OS roots)
                                  upstream
```

Module map (`src/`):

| Module          | Responsibility                                                        |
|-----------------|-----------------------------------------------------------------------|
| per-module config | Each of `proxy`/`adblock`/`dns`/`stats` owns its base settings and loads them from its own `data/settings/<module>-base.toml`, validating them itself; every key has a built-in default. |
| `proxy::server`   | IO shell: accept loop, plain-HTTP forwarding, CONNECT/MITM.           |
| `proxy::pipeline` | Pure per-request/response/CONNECT decisions (target, type, inject-or-stream, deny/tunnel/MITM). |
| `proxy::ca`       | Signing CA load (required on disk); per-host leaf minting; TLS config cache.|
| `http_client`     | The one shared upstream HTTP(S) client (proxy forwarding + list downloads). |
| `adblock`         | `adblock` engine: block decisions + cosmetic CSS + list mgmt/storage. |
| `exclusions`      | Runtime-managed domains that bypass MITM (blind tunnel).              |
| `dns`             | Filtering DNS resolver: cache, DoH/DoT upstreams, rewrites, settings. |
| `state`           | Shared metrics + the live UI stream (no server-side request log).     |
| `history`         | Rolling 24 h stats: sparkline buckets + top queried/blocked domains.  |
| `runtime`         | Live start/stop/rebind of the proxy + DNS listeners, with persisted server overrides. |
| `persist`         | The persistence models: line-file sets (`PersistedSet`) and JSON override stores (`OverrideStore`). |
| `web`             | Admin dashboard + JSON API, served on `admin_listen` (one submodule per sub-surface). |
| `maintenance`     | Blocklist fetch (download→install→report) + the hourly auto-updater.  |

---

## Build & run

```bash
cargo build --release
./target/release/proxy
```

> **Configuration.** Each module (`proxy`, `adblock`, `dns`, `stats`) loads its own
> base settings from `data/settings/<module>-base.toml` and validates them
> itself; every key has a built-in default, so a module with no file just uses
> its defaults. The tables are `[server]`/`[tls]`/`[performance]` for proxy,
> `[adblock]`, `[dns]`, and `[logging]`. `admin_listen` (in `[server]`) is the one
> wiring-level knob validated by the root, not a module.

No native dependencies. **You must supply a signing CA**: the proxy reads
`data/certs/ca-cert.pem` / `data/certs/ca-key.pem` (paths in `[tls]`) and
refuses to start if they're missing — it never generates its own. Use a
private-PKI intermediate (e.g. step-ca) so leaves chain to a root your devices
already trust:

```bash
step certificate create "proxy Signing CA" data/certs/ca-cert.pem data/certs/ca-key.pem \
  --profile intermediate-ca --ca root_ca.crt --ca-key root_ca_key
```

…or supply a self-signed root you create once. **Install the CA (or, for an
intermediate, its root) in your OS/browser trust store**, then point your
client's HTTP+HTTPS proxy at `127.0.0.1:8080`.

Excluded domains are tunneled blind — no TLS termination — which is what you
want for cert-pinned apps (banking, etc.). Manage them live in the dashboard's
**Excluded domains** tab; the set persists to
`data/settings/excluded-domains.conf` and survives restarts.

---

## Run with Docker

Build the image (multi-stage; `.dockerignore` keeps `target/` and CA material out):

```bash
docker build -t proxy:latest .
```

The image bakes the scriptlet library and two module base files
(`proxy-base.toml`, `dns-base.toml`) that bind the listeners to `0.0.0.0` so
published ports work. It does **not** contain a CA — bind-mount one at run time
(the container exits immediately without it):

```bash
docker run -d --name proxy \
  -p 8080:8080 -p 8081:8081 \
  -v "$(pwd)/ca-cert.pem:/app/data/certs/ca-cert.pem:ro" \
  -v "$(pwd)/ca-key.pem:/app/data/certs/ca-key.pem:ro" \
  -v proxy-data:/app/data \
  proxy:latest
```

- **CA mounts** are required — point them at your step-ca intermediate in
  production. Use absolute paths (`$(pwd)/…`); `-v` rejects bare relative paths.
- **`-p 8080`** proxy, **`-p 8081`** admin dashboard (drop `8081` to keep it
  private).
- **`proxy-data`** named volume persists everything the proxy writes — blocklists,
  settings, logs, and managed CAs — under `/app/data` (blocklists download on
  first start).

Verify:

```bash
docker logs -f proxy    # want: "proxy listening 0.0.0.0:8080"
curl -x http://localhost:8080 --cacert data/certs/ca-cert.pem -I https://example.com
```

If the container exits at once, `docker logs proxy` shows
`CA cert/key not found … provide a signing CA` — the mounts didn't land.

To change the baked settings, mount your own base files over
`/app/data/settings/proxy-base.toml` (or `dns-base.toml`), or mount a whole data
dir at `/app/data` that already contains them — a bind-mounted empty dir hides
the baked files, so it must supply its own. Behind a reverse proxy (Traefik +
step-ca), don't publish ports — attach to its network and route a
TCP-passthrough entrypoint to `proxy:8080`; see `docker-compose-template-dev.yml`.

---

## Admin web dashboard

With `server.admin_listen` set (default `127.0.0.1:8081`), open
`http://127.0.0.1:8081/`. It auto-refreshes every 2s and shows:

- **Traffic cards** — requests, blocked, errors, DNS queries/blocked/cached,
  each covering the **last 24 hours** with a sparkline (10-minute buckets;
  history is in-memory and restarts with the proxy). Blocked requests count
  as requests, so the block percentage is a real share. A **reset stats**
  button zeroes every counter, the 24 h window, and the DNS upstream health
  table.
- **Top domains** — the most queried (allowed traffic only) and most blocked
  domains of the last 24 h, HTTP and DNS combined, with one-click
  block/unblock.
- **Blocklists** — view/add/edit/delete lists; all custom rules live in one
  persisted `custom` list.
- **Rule tester** — check any URL against the current rules without sending
  traffic; reports the outcome and matching rule.
- **Scriptlets** — the loaded uBO scriptlet library (searchable), plus a live
  feed of which scriptlets fired into which pages.
- **Requests** — searchable live log of every forwarded request (method,
  status, type, URL), streamed over SSE while the page is open. Blocked
  requests show status `BLK`; opened tunnels show `OPEN`. The server keeps no
  history: records exist only in the open page, and with no dashboard
  connected nothing is recorded at all.
- **Activity / Errors** — colour-coded event streams.
- **Setup panel** — proxy address and a **Download root CA** link.

JSON API (all on `admin_listen`):

| Endpoint               | Method   | Purpose                                        |
|------------------------|----------|------------------------------------------------|
| `/api/stream`          | GET      | SSE live feed: `stats` (2 s), `request`, `attach` (captured headers/bodies), `event`, `dns` |
| `/api/stats`           | GET      | info + metrics + uptime + the 24 h `window` (totals, series, top domains) |
| `/api/stats/reset`     | POST     | zero all counters, the 24 h window, and DNS upstream health |
| `/api/scriptlets`      | GET      | loaded scriptlet library + recent injections   |
| `/api/blocklists`      | GET/POST | list / add-append-replace-delete               |
| `/api/blocklist?name=` | GET      | one list's raw rule text (for editing)         |
| `/api/scriptlet?name=` | GET      | one scriptlet's decoded JS source              |
| `/api/scriptlets/update` | POST   | refresh the scriptlet library from uBO master  |
| `/api/exclusions`      | GET/POST | domains that bypass MITM (add / delete)        |
| `/api/check`           | POST     | test a URL against rules (`{url,type?,source?}`)|
| `/api/server`          | GET      | proxy + DNS listener status (enabled / listen / running) |
| `/api/server/config`   | POST     | start/stop or rebind the proxy & DNS listeners live (persisted) |
| `/api/dns`             | GET      | resolver status: upstreams + health, cache, settings |
| `/api/dns/flush`       | POST     | clear the DNS response cache                   |
| `/api/dns/test`        | POST     | test a domain against the DNS filter (`{domain}`) |
| `/api/dns/rewrites`    | GET/POST | operator-defined local records (add / delete)  |
| `/api/dns/config`      | POST     | live upstream/cache settings; `{"reset": true}` returns to base config |
| `/ca-cert.pem`         | GET      | download the root CA                           |

Bind it to `127.0.0.1` only (the default) — it exposes controls. Set
`admin_listen = ""` to disable it entirely.

---

## Blocking behavior

- **Blocked requests drop the connection**, so the client sees a network
  error — how real ad blockers behave, and what block-tester pages score as
  blocked. There is no 403 mode and no live on/off toggle.
- **Cosmetic (`##`) rules** apply only to uncompressed HTML. The proxy requests
  `Accept-Encoding: identity` for document loads so it can inject the hide-CSS;
  compressed HTML streams through unmodified.
- **Cert-pinned / bypassed hosts.** Some hosts (e.g. Apple's own `*.apple.com`)
  are pinned by the client, so MITM fails; `||host^` rules are enforced at the
  CONNECT stage instead. Traffic that skips the proxy entirely (HTTP/3/QUIC, OS
  proxy-bypass lists) can't be filtered here — block those via DNS, `/etc/hosts`,
  or an app-layer firewall on the client.

---

## Scriptlet injection

uBlock Origin neutralizes many ads/anti-adblock with **scriptlets** — small JS
snippets invoked by `##+js(name, args…)` filter rules (e.g.
`##+js(set-constant, adConfig, {})`). Network + cosmetic filtering can't do this;
it needs code running in the page.

It's **on by default** (`inject_scriptlets = true`, `scriptlet_resources =
"data/scriptlets/scriptlets.json"`). Because it strips Content-Security-Policy site-wide,
you can opt out in the adblock base file (`data/settings/adblock-base.toml`):

```toml
[adblock]
inject_scriptlets = false
```

`scriptlet_resources` is a JSON array of `adblock::resources::Resource`
objects (the adblock-rust format). A pre-generated `data/scriptlets/scriptlets.json` ships
in the repo (and the Docker image), so scriptlets work out of the box.

### Refreshing the library

The library is uBO-derived and updates over time. It's regenerated from a
uBlock Origin checkout by `tools/convert-ubo-scriptlets.mjs`, which *evaluates*
uBO's ESM modules (importing them fires each `registerScriptlet(...)`), reads
every scriptlet's canonical source via `fn.toString()`, and emits the full
library — scriptlets, their `.fn` dependencies, trusted scriptlets, and
web-accessible stubs. Two ways to run it:

- **From the CLI** (any machine with Node):

  ```bash
  git clone --depth 1 https://github.com/gorhill/uBlock.git /tmp/ubo
  node tools/convert-ubo-scriptlets.mjs /tmp/ubo data/scriptlets/scriptlets.json
  ```

- **At runtime**, from the dashboard's **Scriptlets → "Update from uBO"** button,
  or automatically by the hourly auto-updater (when `data/scriptlets/scriptlets.json` is
  older than `adblock.auto_update_hours`). The proxy downloads the uBO tarball
  through its own HTTP client, extracts it with `tar`, and runs the converter.
  That last step needs a JS runtime: the proxy uses **[LLRT](https://github.com/awslabs/llrt)**
  (AWS's ~14 MB QuickJS runtime, shipped in the Docker image) if it's on `PATH`,
  otherwise **Node**. Override the choice with `PROXY_JS_RUNTIME=/path/to/bin`.

  > uBO's scriptlets are self-registering ESM modules — the name→source→dependency
  > catalog only exists *after the JS runs* — so this conversion step is
  > inherently a JS job; there's no static file to read. LLRT lets it happen
  > inside a slim container without bundling full Node.

How injection works:

- For each HTML page, matching `##+js(…)` rules are resolved to JS (dependencies
  inlined, args substituted) and injected as a `<script>` right after `<head>`,
  so it runs before the page's own scripts.
- **CSP is stripped** on every HTML response injected into, because a strict
  `Content-Security-Policy` would otherwise refuse the inline script. This is a
  real, site-wide security downgrade — it's the tradeoff for the feature being on
  by default (set `inject_scriptlets = false` to opt out).
- The **Scriptlets** dashboard tab and the `scriptlets injected` log lines show
  which scriptlets fired into which page, for debugging.

---

## Testing

```bash
cargo test               # unit + integration
```

Covers ad-block match/allow/disabled, runtime list add/rebuild, list-store
reconciliation (in-memory store), the blocklist fetch flow (canned downloader,
no network), the request/response/CONNECT pipeline decisions (target, type,
cosmetic injectability, deny/tunnel/MITM), the IO shell driven in-process through its seams (canned
upstream/DNS/clock), request-log recording + capture (buffered and streaming),
the admin API end-to-end in-process, the upstream HTTP client (loopback +
redirects), cosmetic CSS generation, scriptlet resolution + naming, and config
validation/exclusions.

## License

MIT OR Apache-2.0.
