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
Removing Encrypted Client Hello configs from DNS answers so HTTPS connections stay inspectable by the proxy.

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

## Control

**Runtime**:
The control plane that starts, stops, and reconfigures the proxy and DNS listeners while the process runs. Owned by the admin web app.

**Override**:
A setting changed at runtime and persisted separately, layered on top of config.toml values at startup.
_Avoid_: using "override" for DNS rewrites
