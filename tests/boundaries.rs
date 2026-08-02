//! Architecture-boundary lint.
//!
//! Every module (`adblock`, `proxy`, `dns`, `stats`) exposes a single public
//! interface: its `api` facade. Sibling modules and the web app must reach a
//! module only through that facade, and only along the dependency edges the
//! architecture allows (see `docs/ARCHITECTURE.md`).
//!
//! This test scans the source tree and fails on:
//!   1. any cross-module path into one of the four modules that does not go
//!      through `::api::`, and
//!   2. any cross-module reference that is not on the allowed dependency graph.
//!
//! It also proves, with a fixture, that a forbidden import is actually caught.

use std::path::{Path, PathBuf};

/// The modules that hide behind an `api` facade. `web` has no facade: it
/// implements nothing and is only wired up from the root.
const FACADE_MODULES: [&str; 5] = ["adblock", "proxy", "dns", "stats", "tester"];
const ALL_MODULES: [&str; 6] = ["adblock", "proxy", "dns", "stats", "tester", "web"];

/// A file belongs to a "zone": the module it lives in, `web`, or `root`
/// (the top-level wiring files: main.rs, lib.rs, config.rs, error.rs).
fn zone_of(rel_to_src: &Path) -> String {
    let mut comps = rel_to_src.components();
    let first = comps.next().map(|c| c.as_os_str().to_string_lossy().to_string());
    match first {
        // A path with more components is inside a subdirectory (the module).
        Some(dir) if comps.next().is_some() => dir,
        _ => "root".to_string(),
    }
}

/// Which zones may reference which modules. `root` and `web` may reach every
/// module; the rest follow the architecture's dependency edges.
fn allowed_targets(zone: &str) -> &'static [&'static str] {
    match zone {
        "root" => &["adblock", "proxy", "dns", "stats", "tester", "web"],
        "web" => &["adblock", "proxy", "dns", "stats", "tester"],
        "adblock" => &["stats"],
        "proxy" => &["adblock", "dns", "stats"],
        "dns" => &["adblock", "stats"],
        "stats" => &["adblock", "proxy", "dns"],
        // The tester judges rules from inside the browser. It asks no module
        // anything, so that nothing it reports depends on this project's own
        // filtering being the one under test.
        "tester" => &[],
        _ => &[],
    }
}

/// Find every `crate::<module>::<segment>` or `proxy::<module>::<segment>`
/// reference (the binary crate refers to the library by its crate name,
/// `proxy`) and return `(module, first_segment)` pairs.
fn module_refs(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for prefix in ["crate::", "proxy::"] {
        let mut from = 0;
        while let Some(pos) = text[from..].find(prefix) {
            let start = from + pos;
            from = start + prefix.len();
            // The prefix must start on a token boundary, not mid-identifier.
            if start > 0 {
                let p = bytes[start - 1];
                if p == b'_' || p.is_ascii_alphanumeric() || p == b':' {
                    continue;
                }
            }
            let rest = &text[from..];
            let module: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !ALL_MODULES.contains(&module.as_str()) {
                continue;
            }
            let after = &rest[module.len()..];
            let Some(after) = after.strip_prefix("::") else { continue };
            let segment: String =
                after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if segment.is_empty() {
                continue;
            }
            out.push((module, segment));
        }
    }
    out
}

/// Check one file's text against the rules for its zone. Returns human-readable
/// violation messages (empty when the file is clean).
fn check(zone: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (module, segment) in module_refs(text) {
        if module == zone {
            continue; // same-module reference is internal, not cross-module
        }
        if !allowed_targets(zone).contains(&module.as_str()) {
            problems.push(format!(
                "{zone} references {module} (not an allowed dependency edge)"
            ));
            continue;
        }
        if FACADE_MODULES.contains(&module.as_str()) && segment != "api" {
            problems.push(format!(
                "{zone} reaches into {module}::{segment} (must go through {module}::api)"
            ));
        }
    }
    problems
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn source_tree_respects_module_boundaries() {
    let src = src_dir();
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "found no source files under {}", src.display());

    let mut all = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&src).expect("path under src");
        let zone = zone_of(rel);
        let text = std::fs::read_to_string(&path).expect("read source file");
        for problem in check(&zone, &text) {
            all.push(format!("{}: {problem}", rel.display()));
        }
    }
    all.sort();
    all.dedup();
    assert!(all.is_empty(), "boundary violations found:\n{}", all.join("\n"));
}

#[test]
fn lint_flags_a_forbidden_facade_bypass() {
    // proxy may depend on dns, but only through dns::api.
    let problems = check("proxy", "use crate::dns::lookup::DnsService;");
    assert!(
        problems.iter().any(|p| p.contains("dns::lookup") && p.contains("dns::api")),
        "expected a facade-bypass violation, got: {problems:?}"
    );
}

#[test]
fn lint_flags_a_forbidden_dependency_edge() {
    // dns must not depend on proxy at all, even through its facade.
    let problems = check("dns", "use crate::proxy::api::EgressPolicy;");
    assert!(
        problems.iter().any(|p| p.contains("not an allowed dependency edge")),
        "expected an edge violation, got: {problems:?}"
    );
}

#[test]
fn lint_accepts_a_valid_facade_import() {
    // proxy -> dns through the facade is allowed and must not be flagged.
    assert!(check("proxy", "use crate::dns::api::DnsService;").is_empty());
    // Same-module internal references are fine.
    assert!(check("proxy", "use crate::proxy::egress::EgressPolicy;").is_empty());
    // Root wiring may reach any module.
    assert!(check("root", "use proxy::stats::api::SharedState;").is_empty());
}
