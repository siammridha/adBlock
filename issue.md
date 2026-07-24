# PRD + Issues

Generated with the `to-prd` and `to-issues` skills. This one file plays the role
of the "issue tracker": a short PRD at the top, then the work broken into
independently-grabbable vertical slices.

---

# PRD

## Problem Statement

I use the admin UI to inspect proxy and DNS traffic, and I run the whole thing
from a single `config.toml`. A few things get in my way:

- Log times are shown in 24-hour format. I want to read them as a normal
  12-hour clock.
- In the request log, long URLs get cut off, but so do the other columns. I
  can't see the full URL, and I also can't always see the full text of the
  smaller columns.
- When I open a captured body that is binary (an image, a font, a wasm file), the
  UI just says "[binary body]". I can't get the actual bytes.
- When a request is blocked, the request's headers and body are not always kept.
  I want the full request stored even for blocked requests, so I can see what was
  attempted.
- All the base configuration lives in one shared `config.toml`. That is a shared
  config file, which cuts across the module boundaries this project cares about.
- When one request is blocked, the whole connection is dropped. If only that one
  request (path) was blocked and the host itself is fine, dropping the connection
  breaks the other requests the page is making on that same connection.

## Solution

- Format log times on the UI side as a 12-hour `hh:mm:ss` clock. The server keeps
  storing UTC timestamps; only the display changes.
- In the request log table, let only the URL column wrap. Every other column
  shows its full text (no cut-off ellipsis).
- Let any captured body be decoded, including binary ones. The user can get the
  full decoded bytes (download or hex view) instead of a placeholder.
- Store the full request headers and request body even when the request is
  blocked.
- Move each module's base config values out of `config.toml` and into that
  module's own settings file, and give each module its own loader. No shared
  config file. Ownership stays inside each module.
- After a blocked request, only drop the connection when the host itself is
  blocked. If just the path/resource was blocked, return a normal blocked
  response and keep the connection open.

## User Stories

1. As an operator reading the request log, I want times shown on a 12-hour
   clock, so that they match how I normally read the time.
2. As an operator reading the DNS query log, I want the same 12-hour time format,
   so that both logs look consistent.
3. As an operator, I want the stored timestamp to stay UTC on the server, so that
   logs are unambiguous regardless of the viewer's time zone.
4. As an operator scanning the request log, I want long URLs to wrap so I can read
   the whole URL, so that I don't have to open each row to see the full path.
5. As an operator, I want the non-URL columns (time, method, status, type) to
   show their full text, so that nothing important is hidden behind an ellipsis.
6. As an operator inspecting a request, I want to view or download a binary
   response body, so that I can examine images, fonts, or other non-text
   payloads.
7. As an operator, I want a clear way to get the raw decoded bytes of any body,
   so that "binary" is no longer a dead end in the UI.
8. As an operator reviewing blocked traffic, I want the full request headers kept
   for blocked requests, so that I can see exactly what the client sent.
9. As an operator reviewing blocked traffic, I want the request body kept for
   blocked requests too, so that I can see posted data that was blocked.
10. As a maintainer, I want each module's base configuration to live inside that
    module, so that there is no shared config file crossing module boundaries.
11. As a maintainer, I want each module to load its own config, so that ownership
    of settings stays with the module that owns the data.
12. As an operator, I want the root/entry point to only wire modules together, so
    that no configuration logic leaks to the top level.
13. As a user browsing a normal site, I want a single blocked sub-resource not to
    kill my whole connection, so that the rest of the page keeps loading.
14. As an operator, I want a fully blocked host to still have its connection
    dropped, so that host-level blocking behavior does not change.
15. As a user opening the admin UI, I want the app to be named "adBlocker" in the
    browser tab and header, so that the product name reflects what it is, not just
    "proxy".

## Implementation Decisions

- **Time format is UI-only.** Records already store `ts_ms` as milliseconds since
  the UNIX epoch (UTC). No server change is needed beyond confirming this stays
  true. The UI's single `fmtTime` helper is the one place to change; it feeds the
  request log, the DNS query log, and the error log times. Format: 12-hour clock
  with AM/PM, `hh:mm:ss`.
- **Request log is a virtualized, fixed-height list.** Rows are absolutely
  positioned at `index * ROW_H` with a fixed 30px height. Letting the URL wrap to
  multiple lines conflicts with this. Decision to make inside the wrapping issue:
  either keep fixed-height rows but make them tall enough for a wrapped URL (raise
  `ROW_H` and keep it in sync with the CSS), or move the request log to
  variable-height rows. Fixed taller rows is the smaller change and is the
  default unless it looks bad.
- **Only the URL column wraps.** The table is a CSS grid (`.rqcols`). The current
  rule ellipsis-truncates every cell (`.vrow > span`). Change so the URL cell
  wraps and the other columns size to their content and show full text.
- **Binary decode lives in the Stats module.** Stats owns the stored capture
  records and the decode path (`decode_captured` / `decode_captured_body`). Today
  it returns a `[binary body — N bytes]` placeholder when the decoded bytes
  contain a null. Decision: extend the decode result so binary bytes come back as
  real bytes (for the Web App to offer as a download and/or a hex view), instead
  of a placeholder. The Web App only renders the result; it does not decode.
- **Blocked-request capture lives in the Proxy module.** When a forwarded request
  is blocked, the deny path currently attaches request headers but never reads the
  request body (the block happens before the body is collected). Decision: on the
  blocked path, read and attach the request body as well as the headers, using the
  same capture mechanism as forwarded requests. "Full" here means the body is
  captured (it is not today); existing size caps still apply unless we decide
  otherwise in the issue.
- **Config ownership moves fully into modules.** Today `config.toml` holds
  `[server]`, `[tls]`, `[adblock]`, `[dns]`, `[logging]`, `[performance]`, parsed
  centrally by the root config wiring. Decision: each module owns a base-settings
  file under its own data dir and exposes its own loader. The root passes each
  module only what it needs to find its files (e.g. the data dir) and then wires
  the results together — no central `Config` that parses every section. Keep a
  one-time read of the old `config.toml` for backward compatibility if a
  deployment still has one, then let each module own its file. This is the largest
  change and can be split per module if one issue is too big.
- **Connection drop becomes host-aware.** The proxy's forward path returns an
  error (`BlockedDropped`) for a blocked request, which drops the connection.
  Decision: distinguish a host-level block from a path-level block. If the host
  itself is blocked, keep dropping the connection. If only the path/resource is
  blocked, return a synthetic blocked HTTP response (for example a 403 or an empty
  204) so the connection stays alive and later requests on it still work.

## Testing Decisions

- Good tests check outward behavior, not internal wiring. Prefer the seams that
  already have tests.
- **Proxy forward/deny path** (`Proxy::with_seams` + `SharedState::observe`): the
  existing `server.rs` tests already drive `handle_forward` and read back the
  recorded request (headers, body, status, `BlockedDropped`). Use this seam for
  the blocked-body capture and the connection-drop behavior. For the connection
  change, assert `Ok(response)` for a path-level block vs `Err(BlockedDropped)`
  for a host-level block.
- **Stats decode** (`decode_captured` unit tests and `decode_captured_body`):
  already covered for gzip/brotli/identity. Add a binary case and assert the bytes
  come back in full rather than a placeholder.
- **Capture** (`render` / `attach_body` unit tests): existing tests cover
  truncation and binary placeholder text. Extend for the blocked-request path.
- **Config**: each module already validates its own section. Add per-module tests
  that the module loads its base values from its own file. Keep an end-to-end
  check that the app still starts with the same effective settings.
- **UI (time format, column wrapping)**: there are no JS unit tests; the repo has
  an end-to-end smoke test (`.claude/skills/test-proxy/smoke.sh`). These two are
  verified visually in the dashboard plus the smoke test still passing. Keep the
  changes small and localized.

## Out of Scope

- Redesigning the request log UI beyond time format and URL wrapping.
- Changing how bodies are captured on the wire (size caps, compression handling)
  except where the blocked-request path needs it.
- Time-zone selection UI. Times render in the viewer's local zone; the stored
  value stays UTC.
- Any change to DNS resolution, blocklist logic, or certificate handling.

## Further Notes

- Two issues touch the same blocked/deny path in the proxy: `issue 5` (keep the
  connection open on a path-level block) and `issue 6` (capture the full blocked
  request). They are independent in behavior but edit nearby code, so whoever
  takes the second should rebase on the first. Doing `issue 6` first is a fine
  default.
- The config move is the riskiest work and cuts across every module, so it is
  split into per-module issues (`issue 7`–`issue 10`). `Issue 7` (Proxy) goes
  first because it also establishes the shared pattern — root only wires, each
  module loads its own base-config file — and the one-time `config.toml`
  compatibility read. `Issues 8`–`10` build on that, and the last one removes the
  now-empty central config once every section has moved.

---

# Issues

## Issue 1 — Rename the web UI app name from "proxy" to "adBlocker"

### What to build
Change the app-level branding in the admin UI from "proxy" to "adBlocker". This is
the browser tab title and the main header title. Do not rename the **Proxy** module
section in the sidebar or the "Proxy service" settings panel — those name the real
proxy sub-module and stay "Proxy". Only the overall app/product name changes.

### Acceptance criteria
- [ ] The browser tab title reads "adBlocker" instead of "proxy".
- [ ] The main header title reads "adBlocker".
- [ ] The Proxy module section and the "Proxy service" settings panel keep their
      "Proxy" names.
- [ ] No functional behavior changes; end-to-end smoke test still passes.

### Blocked by
- None - can start immediately.

---

## Issue 2 — Show log times on a 12-hour clock (UI only)

### What to build
Change the UI so log times render as a 12-hour `hh:mm:ss` clock with AM/PM. This
covers the request log, the DNS query log, and the error log, all of which share
one time-formatting helper. The server keeps storing UTC epoch-millisecond
timestamps; only the display changes.

### Acceptance criteria
- [ ] Request log and DNS query log times show as a 12-hour clock (for example
      `3:04:09 PM`).
- [ ] Error log times use the same 12-hour format.
- [ ] Server-stored timestamps are unchanged and remain UTC epoch milliseconds
      (verified against a stored record).
- [ ] End-to-end smoke test still passes.

### Blocked by
- None - can start immediately.

---

## Issue 3 — Request log: only the URL column wraps; all other columns show full text

### What to build
In the request log table, let the URL column text wrap to as many lines as it
needs so the whole URL is visible. Every other column (time, method, status, type)
shows its full text with no ellipsis cut-off. The request log is a virtualized
list with fixed-height rows, so this issue must also decide how wrapping fits that:
either make the fixed row height tall enough for a wrapped URL (and keep it in sync
between the CSS and the row-height constant), or switch the list to variable-height
rows. Prefer the fixed taller row unless it looks wrong.

### Acceptance criteria
- [ ] Long URLs wrap and are fully readable in the request log without opening the
      row.
- [ ] Time, method, status, and type columns show their full text with no
      ellipsis truncation.
- [ ] The virtualized list still scrolls correctly (row positions and heights stay
      consistent).
- [ ] End-to-end smoke test still passes.

### Blocked by
- None - can start immediately.

---

## Issue 4 — Decode any captured body, including binary

### What to build
Let the admin UI get the full decoded content of any captured body, including
binary ones. Today the Stats decode path returns a `[binary body — N bytes]`
placeholder when the decoded bytes are not text. Change it so binary bodies come
back as real bytes, and give the Web App a way to present them (download and/or hex
view). Decoding stays owned by Stats; the Web App only renders the result and does
not decode.

### Acceptance criteria
- [ ] Opening a request whose response body is binary (for example an image, font,
      or wasm) lets the user get the full decoded bytes, not a placeholder.
- [ ] Text bodies still display as text exactly as before.
- [ ] The decode size cap still guards against decompression blow-up.
- [ ] Stats owns the decode; the Web App only renders / offers the download.
- [ ] Unit test covers a binary body decoding to full bytes.

### Blocked by
- None - can start immediately.

---

## Issue 5 — Keep the connection open after a blocked request when the host is not blocked

### What to build
Change the proxy so a blocked request only drops the connection when the host
itself is blocked. If only the specific path/resource was blocked and the host is
otherwise allowed, return a synthetic blocked HTTP response (for example a 403 or
an empty 204) instead of dropping, so the connection stays open and the page's
other requests on that same connection still work.

### Acceptance criteria
- [ ] A blocked sub-resource on an allowed host returns a synthetic blocked
      response and does not drop the connection.
- [ ] A request to a fully blocked host still drops the connection as before.
- [ ] The blocked request is still counted and recorded the same way.
- [ ] Test via the proxy forward seam asserts `Ok(response)` for a path-level block
      and the drop for a host-level block.

### Blocked by
- None - can start immediately. (Touches the same deny path as Issue 6; prefer
  doing Issue 6 first.)

---

## Issue 6 — Store full request headers and body even when a request is blocked

### What to build
When a forwarded request is blocked, capture the full request headers and the
request body onto the record, the same way forwarded requests are captured. Today
the blocked path attaches headers but never reads the body (the block happens
before the body is collected), so blocked requests have no body. Fix that so a
blocked request's detail view shows both its headers and its body.

### Acceptance criteria
- [ ] A blocked request's record has its full request headers captured.
- [ ] A blocked request's record has its request body captured and viewable in the
      detail drawer.
- [ ] Non-blocked (forwarded) request capture is unchanged.
- [ ] CONNECT-level host blocks (which have no request body) still record headers
      where available and do not error.
- [ ] Test via the proxy forward seam asserts a blocked request's stored headers
      and body.

### Blocked by
- None - can start immediately. (Touches the same deny path as Issue 5; prefer
  doing this one first to reduce conflicts.)

---

The config move is split into one issue per module. Issue 7 (Proxy) goes first: it
establishes the shared pattern — the root only wires, each module loads its own
base-config file — and the one-time `config.toml` compatibility read that the other
modules reuse. Issues 8–10 each move one module's section on top of that pattern.
The last of the four to land removes the now-empty central config.

## Issue 7 — Config move: Proxy module (server, tls, performance) + shared pattern

### What to build
Move the Proxy module's base config values (`[server]`, `[tls]`, `[performance]`)
out of the shared `config.toml` and into a Proxy-owned base-config file, loaded by
a Proxy loader. This issue also lays the pattern the other config issues follow:
the root/entry point stops parsing one central config and instead hands each module
what it needs to find its own file, then wires the results together. Include the
one-time read of an existing `config.toml` for backward compatibility, shaped so
the other modules can reuse the same approach. `admin_listen` currently lives in
`[server]` but belongs to wiring — keep it validated at the root as it is today.

### Acceptance criteria
- [ ] Proxy loads its `server`/`tls`/`performance` base values from its own file
      via a Proxy loader.
- [ ] The root no longer parses these sections from a central config; it only wires
      Proxy together.
- [ ] Proxy still validates its own settings (for example the listen address).
- [ ] A one-time `config.toml` compatibility read is in place and reusable by the
      other config issues.
- [ ] The architecture boundary lint / build still passes.
- [ ] The app starts with the same effective proxy behavior as before, verified
      end-to-end.

### Blocked by
- None - can start immediately. (Foundational for Issues 8, 9, and 10.)

---

## Issue 8 — Config move: Adblock module (adblock)

### What to build
Move the Adblock module's base config values (`[adblock]`) out of `config.toml`
into an Adblock-owned base-config file with an Adblock loader, following the
pattern established in Issue 7. The root stops parsing the `[adblock]` section and
only wires Adblock together.

### Acceptance criteria
- [ ] Adblock loads its base values from its own file via an Adblock loader.
- [ ] The root no longer parses the `[adblock]` section.
- [ ] Adblock still validates its own settings.
- [ ] The one-time `config.toml` compatibility read still covers the adblock values.
- [ ] The architecture boundary lint / build still passes.
- [ ] The app starts with the same effective adblock behavior as before.

### Blocked by
- Issue 7 (sets up the per-module loading pattern and the compatibility read).

---

## Issue 9 — Config move: DNS module (dns)

### What to build
Move the DNS module's base config values (`[dns]`) out of `config.toml` into a
DNS-owned base-config file with a DNS loader, following the pattern from Issue 7.
The root stops parsing the `[dns]` section and only wires DNS together.

### Acceptance criteria
- [ ] DNS loads its base values from its own file via a DNS loader.
- [ ] The root no longer parses the `[dns]` section.
- [ ] DNS still validates its own settings.
- [ ] The one-time `config.toml` compatibility read still covers the dns values.
- [ ] The architecture boundary lint / build still passes.
- [ ] The app starts with the same effective DNS behavior as before.

### Blocked by
- Issue 7 (sets up the per-module loading pattern and the compatibility read).

---

## Issue 10 — Config move: Stats module (logging) + remove the central config

### What to build
Move the Stats module's base config values (`[logging]`) out of `config.toml` into
a Stats-owned base-config file with a Stats loader, following the pattern from
Issue 7. As the last section to move, this issue also removes the now-empty central
`Config` struct and central parsing so the root is left with only wiring.

### Acceptance criteria
- [ ] Stats loads its base values from its own file via a Stats loader.
- [ ] The root no longer parses the `[logging]` section.
- [ ] Stats still validates its own settings.
- [ ] The central `Config` struct and central config parsing are removed; the root
      only wires modules together.
- [ ] The one-time `config.toml` compatibility read still works for existing
      deployments (or a documented migration replaces it).
- [ ] The architecture boundary lint / build still passes.
- [ ] The app starts with the same effective behavior as before, verified
      end-to-end.

### Blocked by
- Issues 7, 8, and 9 (every other section must move before the central config can
  be removed).
