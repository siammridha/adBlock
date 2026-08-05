//! Adblock's own page-facing endpoints.
//!
//! The scripts Adblock puts into a page have questions it has to answer after
//! the page was served — which cosmetic rules the names it just grew select,
//! and where the picture detector's weights are. Those answers used to come
//! from the admin server on `127.0.0.1`, which meant an HTTPS page fetching
//! plain HTTP from a different origin: mixed content, so Safari refused it, and
//! unreachable from any machine other than the one the proxy runs on.
//!
//! Instead they are answered here, on the address the page is already on. The
//! request goes back through the proxy like any other, Adblock recognises the
//! path and serves the answer itself, and the browser sees a same-origin
//! request to the site it is already reading. No CORS, no mixed content, and it
//! works for whoever is browsing.
//!
//! Matching happens before any rule is consulted, so no filter list can shadow
//! one of these and no `$redirect` can stand in for it. It also happens whether
//! or not Adblock is switched on: a page served while it was on can still ask
//! after it is switched off, and that question — which carries the page's own
//! class and id names — must not reach the site.
//!
//! Adding one later is a line in `ROUTES` and the function it names. Nothing
//! else moves: the proxy hands over every request already.

use super::AdBlocker;

/// The path every endpoint here lives under. Long and unlovely on purpose —
/// a request to it is one Adblock answers instead of the site, so it has to be
/// a path no site would use.
pub(crate) const PREFIX: &str = "/__abx/";

/// What one endpoint answered. Each route says how long its own answer keeps,
/// because they differ: the model's weights never change, and an answer worked
/// out for one page is never right for another.
pub(crate) struct Served {
    pub body: Vec<u8>,
    pub mime: &'static str,
    pub cache: &'static str,
}

/// `None` means Adblock has nothing for this one, and the request is refused
/// rather than passed to the site.
pub(crate) type Answer = Option<Served>;

/// What answers one endpoint. It gets whatever followed the route's own name in
/// the path, and the request body. A static endpoint reads the path and ignores
/// the body; a computed one does the opposite.
pub(crate) type Handler = fn(&AdBlocker, &str, &[u8]) -> Answer;

const ROUTES: &[(&str, Handler)] = &[("cosmetic", cosmetic), ("blur-model/", blur_model)];

/// Which endpoint this URL is for, and what followed its name. `None` for every
/// ordinary request, which is nearly all of them — one `strip_prefix` before
/// anything else looks at the request.
pub(crate) fn match_url(url: &str) -> Option<(Handler, &str)> {
    let path = path_of(url)?;
    let rest = path.strip_prefix(PREFIX)?;
    ROUTES.iter().find_map(|(name, h)| rest.strip_prefix(name).map(|tail| (*h, tail)))
}

/// The path out of an absolute URL, without the query or fragment.
fn path_of(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let path = &after_scheme[after_scheme.find('/')?..];
    Some(&path[..path.find(['?', '#']).unwrap_or(path.len())])
}

/// Cosmetic rules for class and id names a page grew after it was served. The
/// page sends the names as JSON; Adblock decides what is valid and what the
/// answer is.
fn cosmetic(adblock: &AdBlocker, _rest: &str, body: &[u8]) -> Answer {
    let q = super::commands::CosmeticQuery::parse(body).ok()?;
    let css = adblock.cosmetic_css_for_names(&q.url, &q.classes, &q.ids);
    let json = serde_json::json!({ "css": css });
    Some(Served {
        body: json.to_string().into_bytes(),
        mime: "application/json",
        cache: "no-store",
    })
}

/// One file of the picture detector's model. The name is checked by
/// `blur_model_file`, which only ever reads the manifest and the weight shards
/// beside it.
fn blur_model(adblock: &AdBlocker, rest: &str, _body: &[u8]) -> Answer {
    let (body, mime) = adblock.blur_model_file(rest)?;
    Some(Served { body, mime, cache: "public, max-age=604800, immutable" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_reserved_paths_match() {
        assert!(match_url("https://site.example/__abx/cosmetic").is_some());
        assert!(match_url("https://site.example/__abx/blur-model/model.json").is_some());
        assert!(match_url("http://site.example:8080/__abx/cosmetic?x=1").is_some());

        assert!(match_url("https://site.example/").is_none());
        assert!(match_url("https://site.example/__abx/").is_none(), "the prefix alone");
        assert!(match_url("https://site.example/__abx/nope").is_none(), "an unknown name");
        assert!(match_url("https://site.example/x/__abx/cosmetic").is_none(), "not at the root");
        assert!(match_url("https://site.example/?u=/__abx/cosmetic").is_none(), "in the query");
        assert!(match_url("https://site.example").is_none(), "no path at all");
    }

    #[test]
    fn the_rest_of_the_path_reaches_the_handler() {
        let (_, rest) = match_url("https://site.example/__abx/blur-model/a.bin").unwrap();
        assert_eq!(rest, "a.bin");
        let (_, rest) = match_url("https://site.example/__abx/cosmetic").unwrap();
        assert_eq!(rest, "");
    }
}
