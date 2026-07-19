//! Captures request/response bodies (size-capped) so the admin UI can show
//! them.
//!
//! Bodies on the wire are usually compressed (gzip/br/deflate/zstd), so the raw
//! prefix looks binary. To keep the request path cheap we do **not** decompress
//! at capture time: an identity body is stored as text, while a compressed body
//! is stored as a short placeholder plus its raw compressed prefix (base64,
//! tagged with the encoding). The admin UI decodes that prefix on demand — only
//! when a request is opened and the "Decompress" button is clicked — via
//! [`decode_captured`].

use std::pin::Pin;
use std::task::{Context, Poll};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use hyper::body::{Body, Frame};

use crate::stats::{CaptureSlot, Exchange};

const REQ_BODY_CAP: usize = 16 * 1024;
const RESP_BODY_CAP: usize = 64 * 1024;
/// Ceiling on decompressed output, so a small compressed prefix can't expand
/// into a huge string (decompression-bomb guard).
const DECODE_CAP: usize = 512 * 1024;

/// The `Content-Encoding` applied to a captured body. `Identity` covers both a
/// missing header and an unknown/unsupported encoding — those bodies are stored
/// as-is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyEncoding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl BodyEncoding {
    pub(crate) fn from_headers(headers: &hyper::HeaderMap) -> Self {
        let Some(value) = headers
            .get(hyper::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
        else {
            return Self::Identity;
        };
        // A comma list stacks encodings; the last one is the outermost (applied
        // last), which is what our captured bytes are wrapped in.
        let last = value.split(',').map(str::trim).next_back().unwrap_or("");
        Self::parse(last)
    }

    fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Self::Gzip,
            "deflate" => Self::Deflate,
            "br" => Self::Brotli,
            "zstd" => Self::Zstd,
            _ => Self::Identity,
        }
    }

    /// The token stored alongside the raw prefix and shown in the placeholder.
    fn label(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
        }
    }
}

pub(crate) fn request_body(ex: &Exchange, bytes: &[u8], enc: BodyEncoding) {
    attach_body(ex, CaptureSlot::ReqBody, CaptureSlot::ReqBodyRaw, bytes, bytes.len(), REQ_BODY_CAP, enc);
}

pub(crate) fn response_body(ex: &Exchange, bytes: &[u8], enc: BodyEncoding) {
    attach_body(ex, CaptureSlot::RespBody, CaptureSlot::RespBodyRaw, bytes, bytes.len(), RESP_BODY_CAP, enc);
}

pub(crate) fn stream_response<B>(exchange: Exchange, body: B, enc: BodyEncoding) -> CaptureBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    CaptureBody {
        inner: body,
        cap: if exchange.is_active() { RESP_BODY_CAP } else { 0 },
        exchange,
        enc,
        buf: Vec::new(),
        total: 0,
        flushed: false,
    }
}

pub(crate) fn headers_text(headers: &hyper::HeaderMap) -> String {
    headers
        .iter()
        .map(|(n, v)| format!("{}: {}", n, String::from_utf8_lossy(v.as_bytes())))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Store a captured body. Identity bodies go straight in as text; compressed
/// bodies store a placeholder for display plus the raw prefix (tagged, base64)
/// for on-demand decoding.
fn attach_body(
    ex: &Exchange,
    disp: CaptureSlot,
    raw: CaptureSlot,
    prefix: &[u8],
    total: usize,
    cap: usize,
    enc: BodyEncoding,
) {
    if enc == BodyEncoding::Identity {
        ex.attach(disp, || render(prefix, total, cap));
        return;
    }
    let kept = &prefix[..prefix.len().min(cap)];
    let label = enc.label();
    ex.attach(disp, || format!("[compressed body — {label}, {total} bytes]"));
    // The raw prefix can be large; keep it off the live SSE stream and only in
    // the record/sidecar, where the decode endpoint reads it.
    ex.attach_quiet(raw, || encode_raw(enc, kept));
}

/// Pack a compressed prefix as `"<label>\n<base64>"` for the raw capture slot.
fn encode_raw(enc: BodyEncoding, bytes: &[u8]) -> String {
    format!("{}\n{}", enc.label(), STANDARD.encode(bytes))
}

/// Decode a raw capture slot (`"<label>\n<base64>"`) back to readable text.
/// Best-effort: a truncated compressed prefix decodes as far as it can. Returns
/// a short bracketed note when the bytes can't be shown as text.
pub(crate) fn decode_captured(raw: &str) -> String {
    let Some((label, b64)) = raw.split_once('\n') else {
        return "[cannot decode — malformed capture]".into();
    };
    let enc = BodyEncoding::parse(label);
    let Ok(compressed) = STANDARD.decode(b64.trim()) else {
        return "[cannot decode — bad base64]".into();
    };
    let decoded = decompress(&compressed, enc);
    render_decoded(&decoded)
}

fn decompress(bytes: &[u8], enc: BodyEncoding) -> Vec<u8> {
    match enc {
        BodyEncoding::Identity => bytes.to_vec(),
        BodyEncoding::Gzip => read_capped(flate2::read::GzDecoder::new(bytes)),
        BodyEncoding::Deflate => {
            // HTTP "deflate" is officially zlib-wrapped, but some servers send a
            // raw stream. Try zlib first, fall back to raw if it yields nothing.
            let zlib = read_capped(flate2::read::ZlibDecoder::new(bytes));
            if zlib.is_empty() {
                read_capped(flate2::read::DeflateDecoder::new(bytes))
            } else {
                zlib
            }
        }
        BodyEncoding::Brotli => read_capped(brotli::Decompressor::new(bytes, 4096)),
        BodyEncoding::Zstd => match ruzstd::StreamingDecoder::new(bytes) {
            Ok(dec) => read_capped(dec),
            Err(_) => Vec::new(),
        },
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

fn render(prefix: &[u8], total: usize, cap: usize) -> String {
    let slice = &prefix[..prefix.len().min(cap)];
    if slice.contains(&0) {
        return format!("[binary body — {total} bytes]");
    }
    let mut s = String::from_utf8_lossy(slice).into_owned();
    if total > cap {
        s.push_str(&format!("\n… [truncated — {total} bytes total]"));
    }
    s
}

pub(crate) struct CaptureBody<B> {
    inner: B,
    exchange: Exchange,
    enc: BodyEncoding,
    cap: usize,
    buf: Vec<u8>,
    total: usize,
    flushed: bool,
}

impl<B> CaptureBody<B> {
    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        attach_body(
            &self.exchange,
            CaptureSlot::RespBody,
            CaptureSlot::RespBodyRaw,
            &self.buf,
            self.total,
            RESP_BODY_CAP,
            self.enc,
        );
    }
}

impl<B> Body for CaptureBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, B::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.total += data.len();
                    if this.buf.len() < this.cap {
                        let take = (this.cap - this.buf.len()).min(data.len());
                        this.buf.extend_from_slice(&data[..take]);
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.flush();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl<B> Drop for CaptureBody<B> {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::RequestFacts;

    #[test]
    fn render_passes_text_through_and_notes_truncation() {
        assert_eq!(render(b"hello", 5, 16), "hello");
        let out = render(b"abcd", 100, 4);
        assert!(out.starts_with("abcd"));
        assert!(out.contains("truncated — 100 bytes total"), "out: {out}");
    }

    #[test]
    fn render_replaces_binary_with_placeholder() {
        assert_eq!(render(&[b'a', 0, b'b'], 3, 16), "[binary body — 3 bytes]");
    }

    #[test]
    fn encoding_parses_from_the_content_encoding_header() {
        let mut h = hyper::HeaderMap::new();
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Identity);
        h.insert(hyper::header::CONTENT_ENCODING, "gzip".parse().unwrap());
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Gzip);
        h.insert(hyper::header::CONTENT_ENCODING, "br".parse().unwrap());
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Brotli);
        // A stack of encodings: the outermost (last) one wins.
        h.insert(hyper::header::CONTENT_ENCODING, "gzip, br".parse().unwrap());
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Brotli);
        // Unknown encodings fall back to identity (stored as-is).
        h.insert(hyper::header::CONTENT_ENCODING, "snappy".parse().unwrap());
        assert_eq!(BodyEncoding::from_headers(&h), BodyEncoding::Identity);
    }

    #[test]
    fn gzip_round_trips_through_encode_then_decode() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let text = b"function ad(){ return 42; } // enough bytes to matter";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(text).unwrap();
        let gz = enc.finish().unwrap();
        // The gzip bytes contain nulls — the plain renderer would hide them.
        assert_eq!(render(&gz, gz.len(), RESP_BODY_CAP), format!("[binary body — {} bytes]", gz.len()));
        // Packed as a raw capture slot, decode_captured recovers the text.
        let raw = encode_raw(BodyEncoding::Gzip, &gz);
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
        let raw = encode_raw(BodyEncoding::Brotli, &out);
        assert_eq!(decode_captured(&raw), String::from_utf8_lossy(text));
    }

    #[test]
    fn decode_reports_a_bad_capture_instead_of_panicking() {
        assert_eq!(decode_captured("no-newline-here"), "[cannot decode — malformed capture]");
        assert_eq!(decode_captured("gzip\nnot-valid-base64!!"), "[cannot decode — bad base64]");
    }

    fn state() -> std::sync::Arc<crate::stats::SharedState> {
        std::sync::Arc::new(bare_state())
    }

    fn bare_state() -> crate::stats::SharedState {
        use crate::stats::LoggingConfig;
        use crate::stats::{SharedState, StaticInfo};
        SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                ca_pem: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig {
                level: "info".into(),
                log_actions: true,
                log_requests: true,
            },
        )
    }

    fn persisting_state(dir: &std::path::Path) -> crate::stats::SharedState {
        bare_state().with_data_dir(dir)
    }

    #[test]
    fn buffered_capture_attaches_to_the_record() {
        let state = state();
        let mut obs = state.observe();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "document", url: "https://a.example/", host: "a.example" },
            200,
            false,
        );
        request_body(&ex, b"req", BodyEncoding::Identity);
        response_body(&ex, b"resp", BodyEncoding::Identity);
        let recs = obs.records();
        assert_eq!(recs[0].req_body, "req");
        assert_eq!(recs[0].resp_body, "resp");
    }

    #[test]
    fn compressed_capture_stores_placeholder_plus_decodable_raw() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"console.log('hello world, compressed')").unwrap();
        let gz = enc.finish().unwrap();

        // A persisting state so the raw prefix (kept off the live stream) lands
        // in the detail sidecar, where the decode endpoint reads it.
        let dir = std::env::temp_dir().join("proxy-capture-compressed-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = std::sync::Arc::new(persisting_state(&dir));

        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js", host: "a.example" },
            200,
            false,
        );
        response_body(&ex, &gz, BodyEncoding::Gzip);
        drop(ex); // flush the detail line
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert!(detail.resp_body.starts_with("[compressed body — gzip,"), "body: {}", detail.resp_body);
        assert!(!detail.resp_body_raw.is_empty(), "raw prefix should be captured");
        assert_eq!(decode_captured(&detail.resp_body_raw), "console.log('hello world, compressed')");
    }

    #[tokio::test]
    async fn streaming_capture_flushes_a_prefix_onto_the_record() {
        use http_body_util::{BodyExt, Full};
        let state = state();
        let mut obs = state.observe();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js", host: "a.example" },
            200,
            false,
        );
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"console.log(1)")), BodyEncoding::Identity);
        let body = wrapped.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"console.log(1)");
        assert_eq!(obs.records()[0].resp_body, "console.log(1)");
    }

    #[tokio::test]
    async fn streaming_compressed_capture_keeps_a_decodable_prefix() {
        use flate2::{write::GzEncoder, Compression};
        use http_body_util::{BodyExt, Full};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"streamed-and-gzipped-payload").unwrap();
        let gz = enc.finish().unwrap();

        let dir = std::env::temp_dir().join("proxy-capture-stream-compressed-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = std::sync::Arc::new(persisting_state(&dir));

        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js", host: "a.example" },
            200,
            false,
        );
        let wrapped = stream_response(ex, Full::new(Bytes::copy_from_slice(&gz)), BodyEncoding::Gzip);
        let _ = wrapped.collect().await.unwrap().to_bytes();
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert!(detail.resp_body.starts_with("[compressed body — gzip,"), "body: {}", detail.resp_body);
        assert_eq!(decode_captured(&detail.resp_body_raw), "streamed-and-gzipped-payload");
    }

    #[tokio::test]
    async fn inert_exchange_streams_through_without_buffering() {
        use http_body_util::{BodyExt, Full};
        let state = state();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js", host: "a.example" },
            200,
            false,
        );
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"data")), BodyEncoding::Identity);
        assert_eq!(wrapped.cap, 0, "no prefix budget without a live record");
        let body = wrapped.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"data");
    }
}
