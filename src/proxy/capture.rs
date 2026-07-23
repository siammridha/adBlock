//! Captures request/response bodies (size-capped) so the admin UI can show
//! them.
//!
//! Bodies on the wire are usually compressed (gzip/br/deflate/zstd), so the raw
//! prefix looks binary. To keep the request path cheap we do **not** decompress
//! at capture time: an identity body is stored as text, while a compressed body
//! is stored as a short placeholder plus its raw compressed prefix (base64,
//! tagged with the encoding). The admin UI decodes that prefix on demand — only
//! when a request is opened and the "Decompress" button is clicked. Decoding is
//! owned by the stats module (which owns the stored records); this module only
//! produces the stored form.

use std::pin::Pin;
use std::task::{Context, Poll};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use hyper::body::{Body, Frame};

use crate::stats::api::{CaptureSlot, Exchange};

const REQ_BODY_CAP: usize = 16 * 1024;
const RESP_BODY_CAP: usize = 64 * 1024;

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
        let kept = &prefix[..prefix.len().min(cap)];
        ex.attach(disp, || render(prefix, total, cap));
        // A binary identity body (an image, font, wasm, …) can't be shown inline,
        // so `render` returns a placeholder. Keep its raw bytes too, tagged
        // `identity`, so the decode endpoint can hand the real bytes back for a
        // download / hex view instead of leaving the placeholder a dead end.
        if kept.contains(&0) {
            ex.attach_quiet(raw, || encode_raw(BodyEncoding::Identity, kept));
        }
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
    use crate::stats::api::RequestFacts;

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
    fn compressed_bytes_render_as_binary_and_pack_a_tagged_prefix() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"function ad(){ return 42; }").unwrap();
        let gz = enc.finish().unwrap();
        // The gzip bytes contain nulls — the plain renderer hides them.
        assert_eq!(render(&gz, gz.len(), RESP_BODY_CAP), format!("[binary body — {} bytes]", gz.len()));
        // Packed as a raw capture slot, the prefix is tagged with the encoding so
        // stats can decode it later.
        let raw = encode_raw(BodyEncoding::Gzip, &gz);
        assert!(raw.starts_with("gzip\n"), "raw: {raw}");
    }

    fn state() -> std::sync::Arc<crate::stats::api::SharedState> {
        std::sync::Arc::new(bare_state())
    }

    fn bare_state() -> crate::stats::api::SharedState {
        use crate::stats::api::LoggingConfig;
        use crate::stats::api::{SharedState, StaticInfo};
        SharedState::new(
            StaticInfo {
                version: "test".into(),
                listen: String::new(),
                admin_listen: String::new(),
                started: std::time::Instant::now(),
            },
            &LoggingConfig {
                level: "info".into(),
                log_actions: true,
                log_requests: true,
                ..Default::default()
            },
        )
    }

    fn persisting_state(dir: &std::path::Path) -> crate::stats::api::SharedState {
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
        // Stats owns the decode; the proxy-produced prefix decodes through it.
        let crate::stats::api::BodyDecode::Text(text) = state.decode_captured_body(seq, "resp") else {
            panic!("expected a decodable body");
        };
        assert_eq!(text, "console.log('hello world, compressed')");
    }

    #[test]
    fn identity_binary_capture_keeps_decodable_raw_bytes() {
        // An uncompressed (identity) binary body — e.g. a PNG — shows a
        // placeholder inline but must keep its raw bytes so the decode endpoint
        // can return them in full for a download / hex view.
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x01, 0xff];

        let dir = std::env::temp_dir().join("proxy-capture-identity-binary-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = std::sync::Arc::new(persisting_state(&dir));

        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "image", url: "https://a.example/logo.png", host: "a.example" },
            200,
            false,
        );
        response_body(&ex, png, BodyEncoding::Identity);
        drop(ex);
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert!(detail.resp_body.starts_with("[binary body —"), "body: {}", detail.resp_body);
        assert!(!detail.resp_body_raw.is_empty(), "raw bytes should be captured for a binary identity body");
        let crate::stats::api::BodyDecode::Binary(bytes) = state.decode_captured_body(seq, "resp") else {
            panic!("expected binary bytes back, not text/placeholder");
        };
        assert_eq!(bytes, png, "the full binary body must decode back");
    }

    #[test]
    fn identity_text_body_keeps_no_raw_prefix() {
        // Regression guard: a plain text identity body still shows inline with no
        // raw sidecar (text bodies are unchanged by the binary work).
        let dir = std::env::temp_dir().join("proxy-capture-identity-text-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = std::sync::Arc::new(persisting_state(&dir));

        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "document", url: "https://a.example/", host: "a.example" },
            200,
            false,
        );
        response_body(&ex, b"<html>plain</html>", BodyEncoding::Identity);
        drop(ex);
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert_eq!(detail.resp_body, "<html>plain</html>");
        assert!(detail.resp_body_raw.is_empty(), "a text identity body needs no raw prefix");
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
        let crate::stats::api::BodyDecode::Text(text) = state.decode_captured_body(seq, "resp") else {
            panic!("expected a decodable body");
        };
        assert_eq!(text, "streamed-and-gzipped-payload");
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
