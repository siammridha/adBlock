//! On-demand decompression of captured request/response bodies.
//!
//! Stats owns the stored capture records, so it also owns turning a stored raw
//! prefix back into readable text. A compressed body is stored as
//! `"<label>\n<base64>"` (the label names the `Content-Encoding`); this module
//! decodes that back to text on demand — only when the admin UI opens a request
//! and asks for it. The proxy produces the stored form; stats consumes it, and
//! each keeps its own copy of the wire format (no shared helper).

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Ceiling on decompressed output, so a small compressed prefix can't expand
/// into a huge string (decompression-bomb guard).
const DECODE_CAP: usize = 512 * 1024;

/// The outcome of a body-decode request. The web app maps each variant to an
/// HTTP response; the decode logic and its limits stay here.
pub enum BodyDecode {
    /// Decoded (or best-effort) text ready to show.
    Text(String),
    /// Decoded bytes that are not text (they contain a NUL): an image, font,
    /// wasm, etc. Stats hands back the real bytes; the web app offers them as a
    /// download / hex view. Capped at `DECODE_CAP` like the text path.
    Binary(Vec<u8>),
    /// No compressed body was captured for this request/slot.
    NoData,
    /// The slot name was neither `req` nor `resp`.
    UnknownSlot,
}

/// Decode a raw capture slot (`"<label>\n<base64>"`) back to its bytes. Text
/// bodies come back as `Text`; non-text (binary) bodies come back as `Binary`
/// with the real bytes, so nothing is a dead end. Best-effort: a truncated
/// compressed prefix decodes as far as it can. A malformed capture comes back as
/// a short bracketed `Text` note.
pub(crate) fn decode_captured(raw: &str) -> BodyDecode {
    let Some((label, b64)) = raw.split_once('\n') else {
        return BodyDecode::Text("[cannot decode — malformed capture]".into());
    };
    let Ok(compressed) = STANDARD.decode(b64.trim()) else {
        return BodyDecode::Text("[cannot decode — bad base64]".into());
    };
    let decoded = decompress(&compressed, label);
    classify_decoded(decoded)
}

fn decompress(bytes: &[u8], label: &str) -> Vec<u8> {
    match label.trim().to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => read_capped(flate2::read::GzDecoder::new(bytes)),
        "deflate" => {
            // HTTP "deflate" is officially zlib-wrapped, but some servers send a
            // raw stream. Try zlib first, fall back to raw if it yields nothing.
            let zlib = read_capped(flate2::read::ZlibDecoder::new(bytes));
            if zlib.is_empty() {
                read_capped(flate2::read::DeflateDecoder::new(bytes))
            } else {
                zlib
            }
        }
        "br" => read_capped(brotli::Decompressor::new(bytes, 4096)),
        "zstd" => match ruzstd::StreamingDecoder::new(bytes) {
            Ok(dec) => read_capped(dec),
            Err(_) => Vec::new(),
        },
        // Identity or any unknown label: the bytes are stored as-is.
        _ => bytes.to_vec(),
    }
}

/// Read a decoder to EOF or the decode cap, keeping whatever came before an
/// error (a truncated prefix errors partway but the decoded head is still
/// useful).
fn read_capped(mut r: impl std::io::Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while out.len() < DECODE_CAP {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out.truncate(DECODE_CAP);
    out
}

/// Classify decompressed bytes: text bodies become `Text` (best-effort UTF-8,
/// with a truncation note at the cap); non-text bodies come back as `Binary`
/// carrying the real bytes for the web app to download or hex-view.
fn classify_decoded(bytes: Vec<u8>) -> BodyDecode {
    if bytes.is_empty() {
        return BodyDecode::Text("[nothing decoded — the captured prefix was incomplete]".into());
    }
    if bytes.contains(&0) {
        // Not text: hand back the real (capped) bytes instead of a placeholder.
        return BodyDecode::Binary(bytes);
    }
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if bytes.len() >= DECODE_CAP {
        s.push_str(&format!("\n… [truncated — {DECODE_CAP} bytes decompressed shown]"));
    }
    BodyDecode::Text(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_raw(label: &str, bytes: &[u8]) -> String {
        format!("{}\n{}", label, STANDARD.encode(bytes))
    }

    /// The decoded text for a slot, or a panic if it decoded to binary — a test
    /// helper that keeps the text assertions terse.
    fn decoded_text(raw: &str) -> String {
        match decode_captured(raw) {
            BodyDecode::Text(s) => s,
            other => panic!(
                "expected text, got {}",
                match other {
                    BodyDecode::Binary(b) => format!("binary ({} bytes)", b.len()),
                    _ => "non-text variant".into(),
                }
            ),
        }
    }

    #[test]
    fn gzip_round_trips_through_decode() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let text = b"function ad(){ return 42; } // enough bytes to matter";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(text).unwrap();
        let gz = enc.finish().unwrap();
        let raw = encode_raw("gzip", &gz);
        assert_eq!(decoded_text(&raw), String::from_utf8_lossy(text));
    }

    #[test]
    fn brotli_round_trips_through_decode() {
        use std::io::Write;
        let text = b"body { display: none !important; } /* cosmetic */";
        let mut out = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
            w.write_all(text).unwrap();
        }
        let raw = encode_raw("br", &out);
        assert_eq!(decoded_text(&raw), String::from_utf8_lossy(text));
    }

    #[test]
    fn decode_reports_a_bad_capture_instead_of_panicking() {
        assert_eq!(decoded_text("no-newline-here"), "[cannot decode — malformed capture]");
        assert_eq!(decoded_text("gzip\nnot-valid-base64!!"), "[cannot decode — bad base64]");
    }

    #[test]
    fn identity_and_unknown_labels_pass_bytes_through() {
        let raw = encode_raw("identity", b"plain text body");
        assert_eq!(decoded_text(&raw), "plain text body");
        let raw = encode_raw("snappy", b"unknown-encoding body");
        assert_eq!(decoded_text(&raw), "unknown-encoding body");
    }

    #[test]
    fn binary_body_comes_back_as_full_bytes_not_a_placeholder() {
        // Bytes with an embedded NUL are not text (a PNG-like header here). The
        // decode must return the real bytes in full, not a `[binary body — …]`
        // note, so the web app can download or hex-view them.
        let bytes: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x01, 0xff];
        let raw = encode_raw("identity", bytes);
        match decode_captured(&raw) {
            BodyDecode::Binary(out) => assert_eq!(out, bytes, "binary bytes must round-trip in full"),
            BodyDecode::Text(s) => panic!("expected binary bytes, got text: {s:?}"),
            _ => panic!("expected binary bytes"),
        }
    }

    #[test]
    fn binary_decode_respects_the_size_cap() {
        // A gzip payload that decompresses to more than the cap, full of NULs so
        // it counts as binary, must come back capped: the decompression-bomb
        // guard still applies to the binary path.
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let big = vec![0u8; DECODE_CAP + 4096];
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&big).unwrap();
        let gz = enc.finish().unwrap();
        let raw = encode_raw("gzip", &gz);
        match decode_captured(&raw) {
            BodyDecode::Binary(out) => assert_eq!(out.len(), DECODE_CAP, "binary output must be capped"),
            other => panic!(
                "expected capped binary bytes, got {}",
                match other {
                    BodyDecode::Text(_) => "text",
                    _ => "other",
                }
            ),
        }
    }
}
