//! Tees request/response bytes off the wire so the record can show them.
//!
//! The proxy sees the bytes; Stats decides what is kept and how it is stored.
//! This module only collects a prefix (Stats says how much) and hands it over
//! with the `Content-Encoding` it arrived under.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame};

use crate::stats::api::Exchange;

/// The `Content-Encoding` a body arrived under, as a plain header value for
/// Stats to interpret. Empty when the header is absent.
pub(crate) fn content_encoding(headers: &hyper::HeaderMap) -> String {
    headers
        .get(hyper::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

pub(crate) fn headers_text(headers: &hyper::HeaderMap) -> String {
    headers
        .iter()
        .map(|(n, v)| format!("{}: {}", n, String::from_utf8_lossy(v.as_bytes())))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wrap a streamed response so a prefix of it lands on the record without ever
/// buffering the whole thing.
pub(crate) fn stream_response<B>(exchange: Exchange, body: B, enc: String) -> CaptureBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    CaptureBody {
        cap: exchange.response_body_cap(),
        inner: body,
        exchange,
        enc,
        buf: Vec::new(),
        total: 0,
        flushed: false,
    }
}

pub(crate) struct CaptureBody<B> {
    inner: B,
    exchange: Exchange,
    enc: String,
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
        self.exchange.capture_response_body(&self.buf, self.total, &self.enc);
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
        ex.capture_request_body(b"req", 3, "");
        ex.capture_response_body(b"resp", 4, "");
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
        ex.capture_response_body(&gz, gz.len(), "gzip");
        drop(ex); // flush the detail line
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert!(detail.resp_body.starts_with("[compressed body — gzip,"), "body: {}", detail.resp_body);
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
        ex.capture_response_body(png, png.len(), "");
        drop(ex);
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert!(detail.resp_body.starts_with("[binary body —"), "body: {}", detail.resp_body);
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
        ex.capture_response_body(b"<html>plain</html>", 18, "");
        drop(ex);
        state.flush_logs();

        let seq = state.request_page(None, 10)[0].seq;
        let detail = state.request_detail(seq);
        assert_eq!(detail.resp_body, "<html>plain</html>");
        assert!(
            matches!(state.decode_captured_body(seq, "resp"), crate::stats::api::BodyDecode::NoData),
            "a text identity body keeps no raw prefix to decode"
        );
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
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"console.log(1)")), String::new());
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
        let wrapped = stream_response(ex, Full::new(Bytes::copy_from_slice(&gz)), "gzip".to_string());
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
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"data")), String::new());
        assert_eq!(wrapped.cap, 0, "no prefix budget without a live record");
        let body = wrapped.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"data");
    }
}
