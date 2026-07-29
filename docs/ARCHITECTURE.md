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

---

## Adblock

**Exposes to Proxy and DNS:** an API to ask whether a given domain or path should be
blocked, and to retrieve cosmetic filtering rules. A block decision carries what
to do about it as well as the verdict: a stand-in body to serve instead
(`$redirect`), or a cleaned URL to forward (`$removeparam`). Adblock decodes
both; the caller only picks the response.

**Exposes to the Web App:** APIs for custom filter management, the rule tester,
blocklist management, and its own settings.

**Calls:** the Stats API, to submit whatever data it needs to report.

**Owns internally:**

- Fetching blocklists, scriptlets, and any other remote resources
- Parsing and compiling filter rules
- Custom filter storage and management
- The rule tester
- Its own settings and their persistence

**Validation example.** When the Web App submits a new custom rule, the Web App does
**not** check whether the rule is well-formed. It passes the raw string to the Adblock
API. Adblock parses it and responds with success or an error describing why the rule
is invalid. The Web App renders that response.

---

## Proxy

**Exposes to the Web App:** APIs to read and update excluded hosts, proxy settings,
and certificate state.

**Calls:** the Adblock API for block decisions and cosmetic rules; the DNS API to
resolve hostnames; the Stats API to report traffic and blocking events.

Proxy resolves through the DNS module rather than implementing its own resolver, so
that rewrites, upstream configuration, and DNS-level blocking apply consistently to
proxied traffic. This is a call into DNS's exposed API like any other — Proxy does not
reach into DNS internals, and DNS never calls back into Proxy.

**Owns internally:**

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
- Storage format, rotation, and retention
- Aggregation and querying
- Its own settings and their persistence

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
Web App  ──────►  Adblock, Proxy, DNS, Stats

Proxy    ──────►  Adblock (block decisions)
DNS      ──────►  Adblock (block decisions)

Proxy    ──────►  DNS     (hostname resolution)

Adblock  ──────►  Stats
Proxy    ──────►  Stats
DNS      ──────►  Stats

Stats    ──────►  Adblock, Proxy, DNS (log collection)
```

Anything not listed above is not allowed. In particular:

- Nothing imports the Web App.
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
- [ ] The Web App changes are limited to API calls and rendering.
- [ ] Any new network access, storage, or settings live inside the owning module.