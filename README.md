# proxy

An intercepting HTTP/HTTPS proxy in Rust that blocks ads and trackers:

- **Network filtering** using standard EasyList / uBlock-Origin filter lists
  (`||host^`, `/path`, typed and party-scoped rules). Always on; a blocked
  whole host drops the connection (network error, like a real ad blocker), a
  blocked path gets an empty 403.
- **Redirect rules** (`$redirect`, `$redirect-rule`) serve uBO's neutered
  stand-in for a blocked resource — a do-nothing analytics script, an empty
  pixel — so the page's own code keeps running instead of tripping over a
  missing file. This is most of what "breaks fewer pages" means. Toggleable at
  runtime.
- **Parameter stripping** (`$removeparam`) removes tracking parameters from the
  request before it is forwarded. Toggleable at runtime.
- **Cosmetic filtering** — injects element-hiding CSS from `##selector` rules
  into HTML pages to hide leftover ad containers, plus the `:style()` unbreak
  rules that put a page's scroll back after a modal is hidden. Rules a
  stylesheet cannot carry (`:has-text()`, `:upward()`, `:remove()`, `:xpath()`)
  ride along as a small evaluator that applies them to the live page. A second
  injected script keeps asking about elements the page builds later. The CSS and
  that script are toggleable at runtime, separately. See
  [Cosmetic filtering](#cosmetic-filtering).
- **`$csp` rules** add Content-Security-Policy directives to a page, letting the
  browser's own policy engine block what a URL pattern cannot name.
- **Scriptlet injection** — resolves uBlock Origin `##+js(…)` rules against the
  full uBO scriptlet library and injects the resulting JS into HTML pages.
  Strips CSP so the injected script runs (a deliberate security tradeoff).
  Toggleable at runtime. See [Scriptlet injection](#scriptlet-injection).
- **CONNECT-stage host blocking** so whole-host rules apply even to
  certificate-pinned domains the proxy can't MITM.
- **Filtering DNS server** — its own resolver with a cache, UDP/TCP/DoT/DoH
  upstreams (failover or load-balance), local rewrites, and optional ECH
  stripping. The proxy resolves through it, so DNS-level blocking and rewrites
  apply to proxied traffic too.
- **Streams** everything it doesn't need to touch; only HTML is buffered (and
  only when there are cosmetic rules to apply).
- **Admin web dashboard** (separate port) with rolling 24 h stats (per-card
  sparklines, top queried / top blocked domains), searchable request and query
  logs kept on disk, an event log, a rule tester, runtime blocklist editing,
  and certificate management.

---

## Architecture

```
                 ┌─────────────────────────────────────────────┐
   client ─────► │  proxy                                      │
   (browser,     │   ├─ CONNECT ─► host rule? ─► drop / MITM    │
    OS proxy)    │   │                 └─ terminate TLS (ca)    │
                 │   ├─ adblock (network decision)              │
                 │   ├─ dns     (resolve the target host)       │
                 │   └─ response:                               │
                 │        ├─ HTML ─► inject cosmetic CSS + JS   │
                 │        └─ else ─► stream through             │
                 └─────────────────────────────────────────────┘
                                     │
                                     ▼  re-originated TLS (webpki + extra roots)
                                  upstream
```

`src/` holds five modules. Each owns its own settings, storage, validation, and
outbound networking, and is reachable from outside only through its `api`
submodule. Nothing is shared between them — no `utils/`, no common config, no
shared error type; duplication is preferred to coupling. The full rules, the
allowed dependency edges, and how they are enforced are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Module    | Owns                                                                                |
|-----------|-------------------------------------------------------------------------------------|
| `adblock` | Filter engine and block decisions (including `$redirect` bodies, `$removeparam` URLs and `$csp` directives), cosmetic CSS and procedural rules, the uBO scriptlet library, blocklist storage/fetching/auto-update, custom filters, the rule tester. |
| `proxy`   | The accept loop, plain-HTTP forwarding, CONNECT/MITM, the signing CA and managed cert store, MITM exclusions, its pooled upstream HTTP client, egress policy (resolver-only, IPv6), injection toggles. |
| `dns`     | The DNS listener, cache, upstream pool (UDP/TCP/DoT/DoH) and health, rewrites, ECH stripping and probing. |
| `stats`   | Counters, the rolling 24 h window, the event log, the request/query logs and their rotating files on disk, body capture storage and decoding, retention. |
| `web`     | Nothing of its own. It instantiates the modules, calls their APIs, and renders the dashboard + JSON API on `admin_listen`. |

`main.rs` is wiring only: it builds each module's config, constructs them, hands
them to each other, and starts the listeners.

---

## Build & run

```bash
cargo build --release
./target/release/proxy
```

> **Configuration.** There are no config files to write. Each module builds its
> base config from built-in defaults, then layers its own persisted settings
> file over them at startup. Those files are written and validated by the owning
> module and edited from the dashboard, not by hand:
>
> | File (under `data/settings/`) | Owner | Holds |
> |---|---|---|
> | `proxy-server.json`       | proxy | proxy listener: `enabled`, `listen` |
> | `proxy-settings.json`     | proxy | egress (`resolver_only`, `disable_ipv6`) and injection (`cosmetic`, `scriptlets`, `runtime`) |
> | `excluded-domains.conf`   | proxy | domains tunneled blind, one per line |
> | `active-ca.json`          | proxy | which managed CA signs leaves |
> | `adblock.json`            | adblock | what a decision may carry: `redirect`, `removeparam`, `csp` |
> | `dns-server.json`         | dns   | DNS listener: `enabled`, `listen` |
> | `dns-settings.json`       | dns   | upstreams, mode, bootstrap, cache size, TTL bounds, ECH |
> | `dns-rewrites.conf`       | dns   | local DNS records |
> | `stats-settings.json`     | stats | `retention_hours`, `log_rotate_hours` |
> | `stats-excluded-domains.conf` | stats | domains kept out of the logs and counters |
>
> Each file is created on first run from that module's defaults. Deleting one
> resets that module to its defaults. `admin_listen` is the one wiring-level knob
> the root owns, not a module: it defaults to `127.0.0.1:8081` and is overridden
> with the `PROXY_ADMIN_LISTEN` environment variable (set it empty to turn the
> dashboard off).

No native dependencies. **You must supply a signing CA**: the proxy reads
`data/certs/ca-cert.pem` / `data/certs/ca-key.pem` and refuses to start if
they're missing — it never generates its own. (Once running, the dashboard's
**Certificates** tab manages further CAs under `data/certs/` and picks which one
is active; the switch takes effect on the next start.) Use a
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

The image bakes the scriptlet library and two listener settings files
(`proxy-server.json`, `dns-server.json`) that bind on `0.0.0.0`, and sets
`PROXY_ADMIN_LISTEN=0.0.0.0:8081`, so published ports work. It does **not**
contain a CA — bind-mount one at run time (the container exits immediately
without it):

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

To change the baked settings, mount your own files over
`/app/data/settings/proxy-server.json` (or `dns-server.json`), or mount a whole
data dir at `/app/data` that already contains them — a bind-mounted empty dir
hides the baked files, and the modules would then write loopback defaults. Move
the dashboard with `-e PROXY_ADMIN_LISTEN=…`. Behind a reverse proxy (Traefik +
step-ca), don't publish ports — attach to its network and route a
TCP-passthrough entrypoint to `proxy:8080`.

---

## Admin web dashboard

With `admin_listen` set (default `127.0.0.1:8081`), open
`http://127.0.0.1:8081/`. Stats refresh every 2 s over SSE. Tabs:

- **Traffic cards** — requests, blocked, errors, DNS queries/blocked/cached,
  each covering the **last 24 hours** with a sparkline (10-minute buckets; the
  window is in-memory and restarts with the proxy). Blocked requests count as
  requests, so the block percentage is a real share. A **reset stats** button
  zeroes every counter, the 24 h window, and the DNS upstream health table.
- **Top domains** — the most queried (allowed traffic only) and most blocked
  domains of the last 24 h, HTTP and DNS combined, with one-click
  block/unblock.
- **Requests** — searchable log of every forwarded request (method, status,
  type, URL). New rows stream in live; older ones page back through the log
  files on disk. Blocked requests show status `BLK`; opened tunnels show `OPEN`
  and flip to closed when the tunnel ends. Clicking a row opens a drawer with
  the captured headers and bodies (compressed bodies decode on demand), each
  copyable.
- **Queries** — the same, for DNS: name, type, answer, upstream, cache hit,
  block decision.
- **Rewrites** — local DNS records the resolver answers itself.
- **Settings** (DNS) — upstream servers and mode (failover / load-balance),
  bootstrap resolvers, cache size, TTL bounds, and the ECH switches, with live
  per-upstream health.
- **Errors / Activity** — colour-coded event streams.
- **Custom filters** — the hand-maintained rule list.
- **Blocklists** — view/add/edit/delete lists, and refresh them from source.
- **Excluded** — domains tunneled blind, no MITM.
- **Settings** (proxy) — resolver-only egress, IPv6 off, and the cosmetic-CSS,
  scriptlet, and live-DOM runtime injection switches.
- **Settings** (adblocker) — whether a block decision may serve a `$redirect`
  stand-in body, whether `$removeparam` strips tracking parameters, and whether
  `$csp` directives are added to the page.
- **Scriptlets** — the loaded uBO scriptlet library (searchable), plus a live
  feed of which scriptlets fired into which pages.
- **Rule tester** — check any URL against the current rules without sending
  traffic; reports the outcome and matching rule.
- **Statistics** — log retention, rotation, and the domains kept out of the
  logs entirely.
- **Certificates** — the managed CAs, which one is active, and a **Download**
  link so a client can trust it.
- **Setup** — the proxy address and the root-CA download.

JSON API (all on `admin_listen`):

| Endpoint                 | Method   | Purpose                                        |
|--------------------------|----------|------------------------------------------------|
| `/api/stream`            | GET      | SSE live feed: `stats` (2 s), `request`, `attach` (captured headers/bodies), `event`, `dns` |
| `/api/stats`             | GET      | info + metrics + uptime + the 24 h `window` (totals, series, top domains) |
| `/api/stats/reset`       | POST     | zero all counters, the 24 h window, and DNS upstream health |
| `/api/stats/config`      | POST     | log retention and rotation hours               |
| `/api/stats/exclusions`  | GET/POST | domains kept out of the logs and counters      |
| `/api/requests`          | GET      | page back through the request log (`before=`, `limit=`) |
| `/api/request?seq=`      | GET      | one request's captured headers, bodies, and scriptlets |
| `/api/request/body`      | GET      | decode one captured body on demand (`seq=`, `slot=req\|resp`) |
| `/api/queries`           | GET      | page back through the DNS query log (`before=`, `limit=`) |
| `/api/errors`            | GET      | the error log; `/api/errors/clear` (POST) empties it |
| `/api/scriptlets`        | GET      | loaded scriptlet library + recent injections   |
| `/api/scriptlet?name=`   | GET      | one scriptlet's decoded JS source              |
| `/api/scriptlets/update` | POST     | refresh the scriptlet library from uBO master  |
| `/api/blocklists`        | GET/POST | list / add-append-replace-delete               |
| `/api/blocklist?name=`   | GET      | one list's raw rule text (for editing)         |
| `/api/adblock`           | GET      | adblock settings: `redirect`, `removeparam`, `csp` |
| `/api/adblock/config`    | POST     | set `redirect`, `removeparam`, `csp`           |
| `/api/check`             | POST     | test a URL against rules (`{url,type?,source?}`)|
| `/api/cosmetic`          | POST     | generic cosmetic CSS for class/id names a live page grew (`{url,classes,ids}`); CORS-open, called by the injected runtime, not the dashboard |
| `/api/exclusions`        | GET/POST | domains that bypass MITM (add / delete)        |
| `/api/server`            | GET      | proxy + DNS listener status (enabled / listen / running) |
| `/api/server/config`     | POST     | start/stop or rebind the proxy & DNS listeners live (persisted) |
| `/api/proxy`             | GET      | proxy settings: egress + injection             |
| `/api/proxy/config`      | POST     | set `resolver_only`, `disable_ipv6`, `cosmetic`, `scriptlets`, `runtime` |
| `/api/dns`               | GET      | resolver status: upstreams + health, cache, settings |
| `/api/dns/flush`         | POST     | clear the DNS response cache                   |
| `/api/dns/test`          | POST     | test a domain against the DNS filter (`{domain}`) |
| `/api/dns/rewrites`      | GET/POST | operator-defined local records (add / delete)  |
| `/api/dns/upstreams`     | POST     | add / delete / enable / disable an upstream server |
| `/api/dns/config`        | POST     | live upstream/cache/ECH settings; `{"reset": true}` returns to defaults |
| `/api/dns/ech-probe`     | POST     | probe now whether ECH is reachable             |
| `/api/certs`             | GET/POST | managed CAs: list / import, generate, activate, delete |
| `/api/cert?name=`        | GET      | download one managed CA                        |
| `/ca-cert.pem`           | GET      | download the active CA                         |

Bind it to `127.0.0.1` only (the default) — it exposes controls. Set
`PROXY_ADMIN_LISTEN=""` to disable it entirely (which also turns off the
live-DOM cosmetic runtime, since it has nothing left to ask).

`/api/cosmetic` is the one endpoint that answers cross-origin, because the page
calling it sits on someone else's domain. It only reads filter rules — it
changes nothing and exposes no controls. Every other endpoint stays same-origin.

---

## Blocking behavior

- **A blocked whole host drops the connection**, so the client sees a network
  error — how real ad blockers behave, and what block-tester pages score as
  blocked. A blocked *path* on an otherwise-fine host gets an empty 403
  instead, so the other requests sharing that connection survive. There is no
  live on/off toggle.
- **`$redirect` rules answer 200 with a stand-in body** rather than blocking
  outright. The bodies are the uBO resources already in
  `data/scriptlets/scriptlets.json`, so nothing extra is downloaded, and they
  load whether or not scriptlet injection is switched on. A `$redirect-rule`
  only supplies a body — it never blocks on its own, so the stand-in is used
  only once something else has decided to block. Every type uBO ships is
  served, not only the JavaScript ones: the transparent pixel, the silent mp4
  and mp3, the empty frame, stylesheet and text file.
- **`$csp` rules add a Content-Security-Policy header** to the page. It is
  appended rather than substituted, so the site's own policy keeps applying
  alongside it — two CSP headers are enforced together. A page that gets an
  inline script from us has its own CSP stripped first, so a `$csp` rule that
  bans inline scripts will also stop our scriptlets on that page.
- **`$removeparam` rewrites the forwarded URL only.** The site receives the
  cleaned URL; the browser is never told, so the parameter stays in its address
  bar. (uBO redirects the browser instead, which also fixes the address bar but
  turns a misfiring rule into a redirect loop.)
- All three have a switch in the adblocker **Settings** tab, on by default.
  They are adblock's own, not the proxy's: the proxy asks one thing —
  "blocked?" — and adblock volunteers the stand-in body, the cleaned URL and the
  CSP directives inside the answer, so adblock is what decides whether to offer
  them. Off, a `$redirect` rule blocks plainly, a `$removeparam` rule leaves the
  URL alone, and a `$csp` rule adds no header.
- **Beacons are matched twice.** A `navigator.sendBeacon()` call is a POST with
  `Sec-Fetch-Mode: no-cors` and `Sec-Fetch-Dest: empty` — byte-for-byte what a
  no-cors `fetch()` sends, with no header separating them (uBO is simply told
  which is which by the browser). A request of exactly that shape is matched as
  the fetch it appears to be, and if nothing matched, asked about once more as a
  `ping`, so `$ping` rules apply without `$xhr` rules losing anything. The
  tradeoff: a genuine no-cors `fetch` POST can also be caught by a `$ping` rule.
- **Cosmetic (`##`) rules** apply only to uncompressed HTML. The proxy requests
  `Accept-Encoding: identity` for document loads so it can inject the hide-CSS;
  compressed HTML streams through unmodified. Text *encoding* is not a
  constraint — splicing happens on raw bytes, so a windows-1252 or Shift_JIS
  page, or one with a single bad byte, is filtered like any other.
- **Cert-pinned / bypassed hosts.** Some hosts (e.g. Apple's own `*.apple.com`)
  are pinned by the client, so MITM fails; `||host^` rules are enforced at the
  CONNECT stage instead. Traffic that skips the proxy entirely (HTTP/3/QUIC, OS
  proxy-bypass lists) can't be filtered here — block those via DNS, `/etc/hosts`,
  or an app-layer firewall on the client.

---

## Cosmetic filtering

Cosmetic rules hide the leftovers that request blocking can't: the empty ad
slot, the placeholder, the "disable your ad blocker" overlay. Three kinds come
out of the engine, and they need different amounts of work.

**Hide rules** (`example.com##.ad-banner`) become one `display:none !important`
line each. Site-specific ones ship in full, because the URL says which site it
is.

**Unbreak rules** (`example.com##body:style(overflow: auto !important)`) are
pure CSS too, and go in right after the hide rules so they win on a shared
element. These matter more than their small count suggests: hiding a modal
without restoring the page's scroll leaves it frozen, which is worse than not
filtering at all.

**Operator rules** (`:has-text()`, `:upward()`, `:xpath()`, `:matches-css()`,
`:matches-attr()`, `:matches-path()`, `:min-text-length()`) select by something
a stylesheet cannot express, and **action rules** (`:remove()`,
`:remove-attr()`, `:remove-class()`) do something CSS cannot do — CSS can make
an element invisible, it cannot take it out of the document, and some
anti-adblock code checks for presence rather than visibility.

Both need a live page, so adblock hands back a small evaluator
(`src/adblock/procedural_runtime.js`) with the page's own rules already in it
and the proxy injects it — like the cosmetic CSS and the scriptlets, the proxy
never reads what it is injecting. What a rule means is adblock's to decide.
The evaluator walks each rule's operator chain against the real DOM, applies
the action, and re-runs on
a debounced `MutationObserver` for content that arrives later. Every action
checks the page's current state before changing anything, so a pass with
nothing to do makes no edit — which is both what stops its own edits from
waking it in a loop, and what lets it re-apply a rule the page has undone.
Unlike the class/id lookup below it asks the proxy nothing, so it works in
every browser.

The engine is built with its `css-validation` feature on so it classifies these
correctly — without it the engine treats them as plain CSS and they end up
emitted into pages as invalid rules. Rules the evaluator is injected for count
as an inline script, so those pages have their CSP stripped like any other
injection.

### Generic rules and pages that build themselves

Generic rules (`##.adsbox`, matching anywhere) number in the hundreds of
thousands, so they can't all be sent. Instead the proxy scans the served HTML
for class and id names and sends only the generic rules that match one.

That works on a page that arrives as finished HTML. It fails on a page that
arrives nearly empty and builds itself in JavaScript, which is most large sites
now — by the time the ad container exists, the one chance to look has passed.

So the proxy also injects a small script that watches for new elements, batches
up any class or id name it hasn't asked about, posts them to `/api/cosmetic` on
the admin server, and applies the CSS that comes back. Names are deduplicated,
sent at most 500 per request, and the whole thing stops after 20 rounds so a
page that rewrites itself forever can't ask forever.

Two things to know about it:

- It has its own switch, **Live-DOM runtime**, next to the cosmetic-CSS one and
  on by default — it costs a script tag and a request per page, so it is worth
  turning off separately from the CSS. It is injected only when that switch is
  on *and* the admin server is running (`PROXY_ADMIN_LISTEN` non-empty). The
  endpoint URL is built from the configured admin address, with a wildcard bind
  treated as loopback — so a browser on a *different machine* than the proxy
  can't reach it, and only the one-shot scan applies there.
- Because it is an inline script, it strips CSP on the pages it goes into, the
  same way scriptlet injection does. That means CSP can now be stripped with
  scriptlet injection switched off.

---

## Scriptlet injection

uBlock Origin neutralizes many ads/anti-adblock with **scriptlets** — small JS
snippets invoked by `##+js(name, args…)` filter rules (e.g.
`##+js(set-constant, adConfig, {})`). Network + cosmetic filtering can't do this;
it needs code running in the page.

It's **on by default**. Because it strips Content-Security-Policy site-wide, it
has its own switch in the dashboard's proxy **Settings** tab, next to the
cosmetic-CSS and live-DOM runtime switches; all three persist to
`data/settings/proxy-settings.json`. What gets injected is a proxy decision; the
rules themselves come from adblock.

Adblock loads the library from `data/scriptlets/scriptlets.json` — a JSON array
of `adblock::resources::Resource` objects (the adblock-rust format). A
pre-generated copy ships in the repo and in the Docker image, so scriptlets work
out of the box.

The same file holds the `$redirect` stand-in bodies, which apply whether or not
scriptlet injection is on. So the file is loaded whenever it exists; the
**Scriptlet injection** switch controls injection, not loading.

### Refreshing the library

The library is uBO-derived and updates over time. It's regenerated from a
uBlock Origin checkout by `tools/convert-ubo-scriptlets.mjs`, which *evaluates*
uBO's ESM modules (importing them fires each `registerScriptlet(...)`), reads
every scriptlet's canonical source via `fn.toString()`, and emits the full
library — scriptlets, their `.fn` dependencies, trusted scriptlets, and
web-accessible stubs. The stubs include the non-JavaScript stand-ins
`$redirect` serves (`1x1.gif`, `noop-1s.mp4`, `noop.txt`, …), read as bytes so
the binary ones survive. Two ways to run it:

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
- **CSP is stripped** on every HTML response an inline script goes into, because
  a strict `Content-Security-Policy` would otherwise refuse it. This is a real,
  site-wide security downgrade — the tradeoff for the feature being on by
  default. Note it is keyed on *any* injected script, so the live-DOM cosmetic
  runtime triggers it too; turning **Scriptlet injection** off no longer
  guarantees the CSP survives.
- The **Scriptlets** dashboard tab and the `scriptlets injected` log lines show
  which scriptlets fired into which page, for debugging.

---

## Testing

```bash
cargo test                    # unit + integration + the boundary lint
./scripts/check-boundaries.sh # just the boundary lint
```

Covers ad-block match/allow/disabled, `$redirect` body decoding and
`$removeparam` URL rewriting, runtime list add/rebuild, list-store
reconciliation (in-memory store), the blocklist fetch flow (canned downloader,
no network), the request/response/CONNECT pipeline decisions (target, type,
cosmetic injectability, deny/tunnel/MITM), the IO shell driven in-process through its seams (canned
upstream/DNS/clock), request-log recording + capture (buffered and streaming),
DNS upstream parsing/health and listener control, the admin API end-to-end
in-process, the upstream HTTP client (loopback + redirects), cosmetic CSS
generation (hide, `:style()`, and splitting operator rules out to the
procedural evaluator), `$csp` header application, beacon re-matching, injection
into non-UTF-8 pages, the live-DOM runtime and its endpoint, scriptlet
resolution + naming, and config validation/exclusions.

`tests/boundaries.rs` is the module-boundary lint: it fails on any cross-module
`use` that skips a module's `api` facade or takes an edge the architecture
doesn't allow. CI runs it on every push and PR alongside the build and tests.

## License

MIT OR Apache-2.0.
