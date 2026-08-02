# Domain glossary — proxy

Terms the code is named after. Use these words in code, docs, and reviews.

## Traffic

**Egress**:
The outbound-connection policy for everything this process dials out: which resolver to use, whether to send ECH, whether IPv6 is allowed. A client of the built-in DNS, not part of it.
_Avoid_: outbound config, connection settings

**Exclusion**:
A domain the proxy must not inspect. Its traffic gets a blind tunnel instead of MITM.
_Avoid_: bypass, whitelist

**Blind tunnel**:
A CONNECT pass-through where bytes are relayed unread — no TLS interception.
_Avoid_: passthrough

**Blackhole**:
A host whose DNS answer was entirely 0.0.0.0, meaning the DNS filter blocked it. Connections to it fail fast instead of timing out.

**Resolver-only**:
Egress mode where outbound connections may only resolve through the built-in DNS. If it is unavailable, connections fail rather than fall back to the OS resolver.

## DNS

**Rewrite**:
A local DNS record the built-in server answers directly, before blocking and forwarding are considered.
_Avoid_: local record, custom DNS, override (that means something else here)

**Upstream**:
A real DNS server the built-in resolver forwards to, over UDP, TCP, DoT, or DoH.

**ECH stripping**:
Removing Encrypted Client Hello configs from DNS answers, so nothing resolving through this server uses ECH — a kill switch for testing, and for networks where ECH misbehaves. It also turns off the proxy's own outbound ECH, which reads its configs from the same answers. Clients using the proxy are unaffected: they send CONNECT and never resolve.

## Ad blocking

**Blocklist**:
A named set of filter rules, downloaded from a source URL or maintained by hand (the custom list).
_Avoid_: filter list, ruleset

**Curation**:
The managed collection of blocklists: what is installed, enabled, and current.

**Scriptlet**:
A small JavaScript snippet (from uBlock Origin) injected into a page to neutralize trackers that request blocking alone can't stop.

**Maintenance**:
The background jobs that keep blocklists and scriptlets fresh.

**Injection**:
What Adblock puts into an HTML page on its way through: cosmetic CSS, scriptlets, the procedural evaluator, and the live-DOM runtime. Adblock owns both the rules and the decision to apply them; the proxy hands over the page it received and forwards the page it gets back. Three switches, all Adblock's: the CSS and the evaluator share the cosmetic one, scriptlets and the runtime have their own.

**Redirect**:
A harmless stand-in body served in place of a blocked resource, so the page's own code does not break on the missing file. Comes from a `$redirect` or `$redirect-rule` option, and the bodies live in the scriptlet resource file. Whether one is offered is an Adblock setting, not a proxy one — the proxy only asks what happens to the request.
_Avoid_: using "redirect" for an HTTP 3xx, or for a DNS rewrite

**Live-DOM runtime**:
The script Adblock puts into a page so it can keep asking about elements it builds after it was served. It reports new class and id names to the admin server and applies the cosmetic CSS that comes back.
_Avoid_: content script, agent

**Procedural rule**:
A cosmetic rule the engine cannot reduce to a plain hide, because it carries an action (`:style()`, `:remove()`) or an operator (`:has-text()`, `:upward()`). The pure-CSS ones are emitted as CSS; the rest go into the page as JSON for the procedural evaluator.

**Procedural evaluator**:
The script that applies procedural rules to the live page. Adblock builds it — rules and all — and puts it into the page, the same as the cosmetic CSS. It carries that page's rules with it, so it asks the admin server nothing, unlike the live-DOM runtime, which exists to ask.
_Avoid_: procedural engine, DOM filter

**Rule tester**:
Adblock's own answer to "what would this rule do to this URL?", asked from the dashboard. It never issues a request; it asks the engine.
_Avoid_: confusing it with the rule-type tester

**Rule-type tester**:
The page at `/test` that issues a real probe per rule type and reports which types a blocker enforced. It is its own module and asks no other module anything, so it reports on whichever blocker is running — this proxy, a browser extension, or none.
_Avoid_: test page, parity page

## Control

**Runtime**:
The control plane that starts, stops, and reconfigures a listener while the process runs. Each module owns its own: proxy has one for the proxy listener, DNS has one for the DNS listener. Updating the settings is what starts, stops, or rebinds.

**Override**:
A setting changed at runtime and persisted to the owning module's settings file, layered on top of that module's built-in defaults at startup.
_Avoid_: using "override" for DNS rewrites
