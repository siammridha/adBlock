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
    /// No compressed body was captured for this request/slot.
    NoData,
    /// The slot name was neither `req` nor `resp`.
    UnknownSlot,
}

/// Decode a raw capture slot (`"<label>\n<base64>"`) back to readable text.
/// Best-effort: a truncated compressed prefix decodes as far as it can. Returns
/// a short bracketed note when the bytes can't be shown as text.
pub(crate) fn decode_captured(raw: &str) -> String {
    let Some((label, b64)) = raw.split_once('\n') else {
        return "[cannot decode — malformed capture]".into();
    };
    let Ok(compressed) = STANDARD.decode(b64.trim()) else {
        return "[cannot decode — bad base64]".into();
    };
    let decoded = decompress(&compressed, label);
    render_decoded(&decoded)
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

/// Render decompressed bytes as text for the UI.
fn render_decoded(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "[nothing decoded — the captured prefix was incomplete]".into();
    }
    if bytes.contains(&0) {
        return format!("[binary body — {} bytes decompressed]", bytes.len());
    }
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if bytes.len() >= DECODE_CAP {
        s.push_str(&format!("\n… [truncated — {DECODE_CAP} bytes decompressed shown]"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_raw(label: &str, bytes: &[u8]) -> String {
        format!("{}\n{}", label, STANDARD.encode(bytes))
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
        assert_eq!(decode_captured(&raw), String::from_utf8_lossy(text));
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
        assert_eq!(decode_captured(&raw), String::from_utf8_lossy(text));
    }

    #[test]
    fn decode_reports_a_bad_capture_instead_of_panicking() {
        assert_eq!(decode_captured("no-newline-here"), "[cannot decode — malformed capture]");
        assert_eq!(decode_captured("gzip\nnot-valid-base64!!"), "[cannot decode — bad base64]");
    }

    #[test]
    fn identity_and_unknown_labels_pass_bytes_through() {
        let raw = encode_raw("identity", b"plain text body");
        assert_eq!(decode_captured(&raw), "plain text body");
        let raw = encode_raw("snappy", b"unknown-encoding body");
        assert_eq!(decode_captured(&raw), "unknown-encoding body");
    }
}
