- When reporting back to me from now on explain things in plain, simple language. Use short sentences and everyday words. Avoid unnecessary detail, buzzwords, overly formal language, metaphors, analogies, and figures of speech. Be direct, clear, and literal.

## Architecture boundaries (non-negotiable)

Modules: `adblock/`, `proxy/`, `dns/`, `stats/`, `webapp/`.

- Each module owns its own settings, storage, validation, and network access.
- Modules interact ONLY through each other's exposed APIs. Never import another module's internals.
- No shared utils, no shared config, no shared state. Duplicate code instead.
- No top-level code implements functionality — root only wires modules together.
- `webapp/` implements nothing. It instantiates modules and calls their APIs. It never validates input; the owning module validates and returns success/error.

Before adding code, ask: which module owns this? Put it there. Full rationale: @docs/ARCHITECTURE.md