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
What the proxy puts into an HTML page it forwards: cosmetic CSS and scriptlets. The rules come from Adblock; whether they go in is a proxy setting.

## Control

**Runtime**:
The control plane that starts, stops, and reconfigures a listener while the process runs. Each module owns its own: proxy has one for the proxy listener, DNS has one for the DNS listener. Updating the settings is what starts, stops, or rebinds.

**Override**:
A setting changed at runtime and persisted to the owning module's settings file, layered on top of that module's built-in defaults at startup.
_Avoid_: using "override" for DNS rewrites
