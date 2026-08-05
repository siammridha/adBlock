# Architecture

This document defines the module boundaries for this project. It is the reference
for code review, and for any AI coding agent working in this repository.

## The problem this solves

The codebase is already split into separate modules, but responsibilities have been
bleeding across boundaries: validation happening in the Web App, settings shared
between modules, helper code drifting to the top level. This document sets the rules
going forward so the direction is clear.

> **Note:** The examples below are not an exhaustive feature list. They illustrate
> how code should be structured, not everything each module does.

> **Terminology:** "API" in this document means a programmatic interface — the set of
> functions/methods a module exposes to in-process callers. It does not mean HTTP
> endpoints or routes. Whether any of it is later surfaced over HTTP is a separate
> concern and does not change these boundaries.

---

## Modules

| Module      | Owns                                                          |
| ----------- | ------------------------------------------------------------- |
| **Adblock** | Filtering decisions, blocklists, custom filters, rule testing |
| **Proxy**   | HTTP(S) proxying, excluded hosts, TLS certificates            |
| **DNS**     | DNS resolution, rewrite list                                  |
| **Stats**   | Logs, log files, aggregated metrics                           |
| **Tester**  | The rule-type test page, its filter list, test assets and switch |
| **Web App** | Nothing. It instantiates modules and calls their APIs.        |

---

## Core principles

### 1. Every module owns its full vertical slice

Data, settings, persistence, validation, and any network access a module needs all
live inside that module. A module is not a library of logic that someone else
configures — it is self-contained and responsible for itself.

### 2. Modules communicate only through exposed APIs

No module reaches into another module's internals. If module A needs something from
module B, B exposes an API for it. Everything else in B is private.

### 3. No shared functionality, no shared settings, no shared state

If two modules need the same capability, each implements its own copy. Duplication is
acceptable; coupling is not. There is no `common/`, no `shared/`, no `utils/` that
modules import from.

This includes outbound networking: if every module needs to fetch something from the
internet, every module implements its own fetching. It includes configuration: there
is no global settings object that modules read from or write to.

### 4. No top-level code implements functionality

The root level wires modules together and nothing more. If you are writing logic at
the top level, it belongs inside a module.

### 5. Validation belongs to the owner of the data

The module that owns a piece of data is the module that decides whether it is valid.
Callers submit raw input and receive success or a structured error. Callers never
pre-validate.

### 6. The Proxy moves bytes. It never edits them.

The Proxy does not change a request or a response in any way. It does not rewrite
URLs, add or drop headers for filtering reasons, edit HTML, inject scripts or styles,
or swap a body for a stand-in. Every one of those is applying a filter rule, and
applying filter rules is Adblock's job.

The Proxy hands the request to Adblock and hands the response to Adblock, and it
forwards whatever comes back. If a request or response needs to be different, Adblock
returns the different version. The Proxy has no opinion about why.

This means the switches that decide whether a rewrite happens — `$redirect`,
`$removeparam`, `$csp`, cosmetic filtering, scriptlet injection, and anything added
later — all belong to Adblock. The Proxy does not ask "should I inject?", because the
Proxy never injects.

Headers the Proxy sets to do its own job — hop-by-hop headers, connection handling,
`Host` for the upstream request, TLS termination — are not filtering and are fine.
The line is simple: if the reason for the change is a filter rule, Adblock makes it.

---

## Adblock

**Exposes to Proxy and DNS:** four things — ask what happens to a request, ask
whether the answer is Adblock's own, hand over the request, hand over the response.

Asking answers one question: what happens to this request? Blocked, or not. A block
carries the stand-in body to serve instead (`$redirect`), which Adblock has already
decoded; the caller only serves it.

Some requests are Adblock talking to itself. The scripts it puts into a page have
questions it has to answer afterwards — which generic cosmetic rules a name it just
grew selects, where the picture detector's weights are — and those go to the page's
own address, so they arrive at the caller like any other request. Adblock recognises
its own paths, off the URL, before any rule is consulted, and answers them. The
caller asks once, hands over the request body it has already collected, and forwards
what comes back without going upstream. It does not know which paths are Adblock's,
and it never decides to intercept one.

The caller hands over the request as it arrived and does not describe it. What kind
of resource this is, which page it came from, whether it might be a beacon — filter
rules match on all three (`$script`, `$image`, `$domain=`, `$1p`/`$3p`, `$ping`), so
Adblock reads them off the request itself. It names the resource type in its answer,
for the caller to log.

Handing over the request lets Adblock make it into the request that goes upstream —
the `$removeparam` cleaned URL, and asking for a body it can read. The caller does
not rewrite anything; it sends what it gets back.

Handing over the response works the same way: Adblock returns the response to send
on. That is where cosmetic rules, scriptlets, the live-DOM runtime and `$csp` headers
are applied — inside Adblock, on the way through. The caller does not ask for rules
and does not apply them. It passes bytes in and forwards the bytes that come out.
Adblock also answers whether it needs a response body at all, so the caller knows
whether to buffer it or stream it.

Because the callers never apply anything, every switch that turns a rewrite on or off
lives here: `$redirect`, `$removeparam`, `$csp`, cosmetic filtering, scriptlet
injection, the live-DOM runtime. Adblock decides whether to make a change; the caller
never decides whether to ask for one.

**Exposes to the Web App:** APIs for custom filter management, the rule tester,
blocklist management, and its own settings.

**Calls:** the Stats API, to submit whatever data it needs to report.

**Owns internally:**

- Fetching blocklists, scriptlets, and any other remote resources
- Parsing and compiling filter rules
- Reading a request's resource type and source page off the wire
- Applying rules to requests and responses: URL cleaning, stand-in bodies, HTML
  editing, script and style injection, filtering-related headers
- Custom filter storage and management
- The rule tester
- Its own page-facing endpoints: which paths are reserved, what each one answers,
  and validating whatever a page sends to one
- Its own settings and their persistence

**Validation example.** When the Web App submits a new custom rule, the Web App does
**not** check whether the rule is well-formed. It passes the raw string to the Adblock
API. Adblock parses it and responds with success or an error describing why the rule
is invalid. The Web App renders that response.

---

## Proxy

**Exposes to the Web App:** APIs to read and update excluded hosts, proxy settings,
and certificate state.

**Calls:** the Adblock API for request decisions and for response rewriting; the DNS
API to resolve hostnames; the Stats API to report traffic and blocking events.

The Proxy changes nothing itself. It carries bytes between the client and the upstream
server, and it runs every request and every response past Adblock on the way. Whatever
Adblock returns is what the Proxy sends. See core principle 6.

Proxy resolves through the DNS module rather than implementing its own resolver, so
that rewrites, upstream configuration, and DNS-level blocking apply consistently to
proxied traffic. This is a call into DNS's exposed API like any other — Proxy does not
reach into DNS internals, and DNS never calls back into Proxy.

**Owns internally:**

- Connection handling, TLS termination, and forwarding
- The excluded-hosts list and its persistence
- Certificate generation, storage, and rotation — Proxy is currently the only module
  that needs TLS for HTTPS, and certificate handling stays here even if that changes
- Its own settings and their persistence

---

## DNS

**Exposes to the Web App:** APIs to change settings and to read/update the rewrite
list.

**Calls:** the Adblock API for block decisions; the Stats API to report queries and
blocking events.

**Owns internally:**

- The rewrite list and its persistence
- Upstream resolver configuration
- Its own settings and their persistence

---

## Stats

**Exposes to the Web App:** APIs to query, display, and manage logs and metrics —
including retention settings and clearing data.

**Calls:** the Adblock, Proxy, and DNS APIs to collect logs and other data.

**Owns internally:**

- All log data and log files
- Storage format, rotation, and retention — including the stored form of a
  captured body: how much of it is kept, how one that cannot be shown inline is
  described, and how it is packed to be decoded later. A caller hands over the
  bytes it saw and the encoding they arrived under, and nothing else
- Aggregation and querying
- Its own settings and their persistence

---

## Tester

A page that reports which adblock rule types are actually being enforced, and by
implication which are not. It exists so the parity claims in
`docs/UBO_PARITY.html` can be checked against a running browser rather than read.

**Exposes to the Web App:** hand it a request, get back a response or nothing —
plus its own on/off switch, which the Web App reads and writes like any other
module's setting. Off, the tester answers nothing, including its own pages. The
Web App mounts it and renders what comes back.

**Calls:** nothing. This is the point of the module, not an accident. Every
verdict is reached inside the browser, from what the page can see and from which
test assets reached the server, so the same page reports on this project's proxy,
on a browser extension with the proxy switched off, or on no blocker at all. The
moment the tester asked Adblock anything, it would stop being able to test
anything else.

**Owns internally:**

- The test page and its fixtures
- The filter list it serves for blockers to subscribe to, written for the host
  that asked for it
- The test assets, and the record of which of them arrived
- Its own on/off switch and its persistence

---

## Web App

The Web App implements no functionality of its own. Its entire scope:

1. Instantiate the other modules.
2. Call their exposed APIs to retrieve and update data.
3. Render the responses.

It does not validate input. It does not transform or reinterpret module data beyond
presentation. It does not hold state that belongs to a module. It does not talk to
the filesystem or the network on a module's behalf.

If you find yourself writing business logic in the Web App, stop and ask which module
owns it.

---

## Allowed dependency directions

```
Web App  ──────►  Adblock, Proxy, DNS, Stats, Tester

Proxy    ──────►  Adblock (request decisions, response rewriting)
DNS      ──────►  Adblock (block decisions)

Proxy    ──────►  DNS     (hostname resolution)

Adblock  ──────►  Stats
Proxy    ──────►  Stats
DNS      ──────►  Stats

Stats    ──────►  Adblock, Proxy, DNS (log collection)
```

Anything not listed above is not allowed. In particular:

- Nothing imports the Web App.
- The Tester imports nothing. Only the Web App imports it.
- Adblock does not import Proxy or DNS.
- DNS does not import Proxy. The Proxy → DNS dependency is one-directional.
- Nothing touches another module's internals — only its exposed API.

---

## Enforcement

- Each module has an explicit public API surface. Everything not part of it is private
  and unreachable from outside the module — enforced by the language's visibility
  rules where possible (unexported symbols, `internal/` packages, non-`pub` items),
  by lint rules where not.
- The allowed dependency graph above is enforced by the linter/build, not by
  convention. A violating import fails CI.
- New shared helpers, shared config, or top-level logic are blocked in review by
  default. If it looks shared, it gets duplicated into the modules that need it.

---

## Review checklist

Before merging, confirm:

- [ ] No new code at the top level that does anything other than wiring.
- [ ] No new shared utility, helper, or settings object used by more than one module.
- [ ] No use of another module's internals — only its exposed API.
- [ ] Validation lives with the module that owns the data, not the caller.
- [ ] The Proxy does not edit any request or response. No URL rewriting, no header
      changes for filtering, no HTML editing, no injection, no stand-in bodies.
      Adblock returns the changed version; the Proxy forwards it.
- [ ] The Web App changes are limited to API calls and rendering.
- [ ] Any new network access, storage, or settings live inside the owning module.