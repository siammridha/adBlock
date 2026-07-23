//! Server-sent events stream pushing live updates to the dashboard.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Frame;
use hyper::{Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::stats::api::{SharedState, UiMsg};

use super::stats::stats_json;
use super::AdminResponse;

pub(super) fn sse_stream(state: Arc<SharedState>) -> AdminResponse {
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let mut updates = state.subscribe_ui();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let frame = tokio::select! {
                _ = tick.tick() => sse_frame("stats", &stats_json(&state)),
                msg = updates.recv() => match msg {
                    Ok(m) => ui_frame(&m),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(SseBody(rx).boxed())
        .unwrap()
}

fn ui_frame(msg: &UiMsg) -> Bytes {
    match msg {
        UiMsg::Request(r) => sse_frame(
            "request",
            &serde_json::to_value(&**r).expect("record serializes"),
        ),
        UiMsg::Attach { seq, slot, text } => sse_frame(
            "attach",
            &json!({ "seq": seq, "slot": slot.as_str(), "text": &**text }),
        ),
        UiMsg::Event(e) => sse_frame(
            "event",
            &json!({ "ts_ms": e.ts_ms, "kind": e.kind.as_str(), "message": e.message }),
        ),
        UiMsg::Dns(d) => sse_frame(
            "dns",
            &serde_json::to_value(&**d).expect("dns record serializes"),
        ),
    }
}

fn sse_frame(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

struct SseBody(mpsc::Receiver<Bytes>);

impl hyper::body::Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|opt| opt.map(|b| Ok(Frame::data(b))))
    }
}
