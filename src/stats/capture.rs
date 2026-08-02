//! The stored form of a captured request/response body.
//!
//! Stats owns the record, so Stats decides what goes in it: how many bytes are
//! kept, how a body that cannot be shown inline is described, and how the raw
//! prefix is packed so [`decode`](super::decode) can read it back. A caller
//! hands over the bytes it saw and the `Content-Encoding` they arrived under,
//! and nothing else.
//!
//! Bodies on the wire are usually compressed (gzip/br/deflate/zstd), so the raw
//! prefix looks binary. Decompressing every one of them on the request path
//! would be wasteful, so an identity body is stored as text while a compressed
//! body is stored as a short placeholder plus its raw compressed prefix
//! (base64, tagged with the encoding). The admin UI decodes that prefix on
//! demand — only when a request is opened and "Decompress" is clicked.

use base64::{engine::general_purpose::STANDARD, Engine as _};

pub(super) const REQ_BODY_CAP: usize = 16 * 1024;
pub(super) const RESP_BODY_CAP: usize = 64 * 1024;

/// The token stored alongside a raw prefix, naming what it is compressed with.
/// A missing or unsupported encoding is `identity`: those bodies are stored
/// as-is. A comma list stacks encodings and the last one is the outermost —
/// which is what the captured bytes are wrapped in.
fn label_of(content_encoding: &str) -> &'static str {
    let last = content_encoding.split(',').map(str::trim).next_back().unwrap_or("");
    match last.to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => "gzip",
        "deflate" => "deflate",
        "br" => "br",
        "zstd" => "zstd",
        _ => "identity",
    }
}

/// What to store for a captured body: the text to show, and the raw prefix to
/// keep for on-demand decoding (`None` when the text is the whole story).
///
/// `prefix` is what the caller managed to keep of a `total`-byte body.
pub(super) fn stored(
    prefix: &[u8],
    total: usize,
    cap: usize,
    content_encoding: &str,
) -> (String, Option<String>) {
    let label = label_of(content_encoding);
    let kept = &prefix[..prefix.len().min(cap)];
    if label != "identity" {
        return (
            format!("[compressed body — {label}, {total} bytes]"),
            Some(pack(label, kept)),
        );
    }
    // A binary identity body (an image, font, wasm, …) can't be shown inline,
    // so keep its raw bytes too, tagged `identity`, and the decode endpoint can
    // hand the real bytes back for a download / hex view instead of leaving the
    // placeholder a dead end.
    let raw = kept.contains(&0).then(|| pack(label, kept));
    (render(kept, total, cap), raw)
}

/// Pack a prefix as `"<label>\n<base64>"` for the raw capture slot.
fn pack(label: &str, bytes: &[u8]) -> String {
    format!("{}\n{}", label, STANDARD.encode(bytes))
}

fn render(kept: &[u8], total: usize, cap: usize) -> String {
    if kept.contains(&0) {
        return format!("[binary body — {total} bytes]");
    }
    let mut s = String::from_utf8_lossy(kept).into_owned();
    if total > cap {
        s.push_str(&format!("\n… [truncated — {total} bytes total]"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_passes_through_and_notes_truncation() {
        let (body, raw) = stored(b"hello", 5, 16, "");
        assert_eq!(body, "hello");
        assert!(raw.is_none(), "a text identity body needs no raw prefix");

        let (body, _) = stored(b"abcd", 100, 4, "");
        assert!(body.starts_with("abcd"));
        assert!(body.contains("truncated — 100 bytes total"), "body: {body}");
    }

    #[test]
    fn binary_is_replaced_by_a_placeholder_but_keeps_its_bytes() {
        let (body, raw) = stored(&[b'a', 0, b'b'], 3, 16, "");
        assert_eq!(body, "[binary body — 3 bytes]");
        assert_eq!(raw.as_deref().map(|r| r.starts_with("identity\n")), Some(true));
    }

    #[test]
    fn the_encoding_comes_off_the_header_outermost_first() {
        assert_eq!(label_of(""), "identity");
        assert_eq!(label_of("gzip"), "gzip");
        assert_eq!(label_of("BR"), "br");
        // A stack of encodings: the outermost (last) one wins.
        assert_eq!(label_of("gzip, br"), "br");
        // Unknown encodings fall back to identity (stored as-is).
        assert_eq!(label_of("snappy"), "identity");
    }

    #[test]
    fn a_compressed_body_stores_a_placeholder_plus_a_tagged_prefix() {
        let (body, raw) = stored(b"\x1f\x8b\x08\x00not-really", 27, RESP_BODY_CAP, "gzip");
        assert_eq!(body, "[compressed body — gzip, 27 bytes]");
        assert!(raw.unwrap().starts_with("gzip\n"));
    }
}
