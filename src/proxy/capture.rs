//! Captures request/response bodies (size-capped) so the admin UI can show
//! them.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame};

use crate::stats::{CaptureSlot, Exchange};

const REQ_BODY_CAP: usize = 16 * 1024;
const RESP_BODY_CAP: usize = 64 * 1024;

pub(crate) fn request_body(ex: &Exchange, bytes: &[u8]) {
    ex.attach(CaptureSlot::ReqBody, || render(bytes, bytes.len(), REQ_BODY_CAP));
}

pub(crate) fn response_body(ex: &Exchange, bytes: &[u8]) {
    ex.attach(CaptureSlot::RespBody, || render(bytes, bytes.len(), RESP_BODY_CAP));
}

pub(crate) fn stream_response<B>(exchange: Exchange, body: B) -> CaptureBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    CaptureBody {
        inner: body,
        cap: if exchange.is_live() { RESP_BODY_CAP } else { 0 },
        exchange,
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
        let (buf, total) = (&self.buf, self.total);
        self.exchange
            .attach(CaptureSlot::RespBody, || render(buf, total, RESP_BODY_CAP));
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

    fn state() -> std::sync::Arc<crate::stats::SharedState> {
        use crate::support::config::LoggingConfig;
        use crate::stats::{SharedState, StaticInfo};
        std::sync::Arc::new(SharedState::new(
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
        ))
    }

    #[test]
    fn buffered_capture_attaches_to_the_record() {
        let state = state();
        let mut obs = state.observe();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "document", url: "https://a.example/" },
            200,
            false,
        );
        request_body(&ex, b"req");
        response_body(&ex, b"resp");
        let recs = obs.records();
        assert_eq!(recs[0].req_body, "req");
        assert_eq!(recs[0].resp_body, "resp");
    }

    #[tokio::test]
    async fn streaming_capture_flushes_a_prefix_onto_the_record() {
        use http_body_util::{BodyExt, Full};
        let state = state();
        let mut obs = state.observe();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js" },
            200,
            false,
        );
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"console.log(1)")));
        let body = wrapped.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"console.log(1)");
        assert_eq!(obs.records()[0].resp_body, "console.log(1)");
    }

    #[tokio::test]
    async fn inert_exchange_streams_through_without_buffering() {
        use http_body_util::{BodyExt, Full};
        let state = state();
        let ex = state.record_forwarded(
            RequestFacts { method: "GET", req_type: "script", url: "https://a.example/app.js" },
            200,
            false,
        );
        let wrapped = stream_response(ex, Full::new(Bytes::from_static(b"data")));
        assert_eq!(wrapped.cap, 0, "no prefix budget without a live record");
        let body = wrapped.collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"data");
    }
}
