//! API handlers for the stats snapshot, logging config, and counter reset.

use serde_json::{json, Value};

use crate::stats::history::Metric;
use crate::web::runtime::Runtime;
use crate::stats::{EventKind, SharedState};

use super::respond::json_ok;
use super::AdminResponse;

pub(super) fn stats_json(state: &SharedState) -> Value {
    let h = state.history.snapshot();
    let mut totals = serde_json::Map::new();
    let mut series = serde_json::Map::new();
    for metric in Metric::ALL {
        totals.insert(metric.key().to_string(), h.totals[metric.index()].into());
        series.insert(metric.key().to_string(), h.series[metric.index()].clone().into());
    }
    let domains = |list: &[(String, u64)]| -> Vec<Value> {
        list.iter().map(|(d, n)| json!({"domain": d, "count": n})).collect()
    };
    json!({
        "info": {
            "version": state.info.version,
            "listen": state.info.listen,
            "admin_listen": state.info.admin_listen,
        },
        "uptime_secs": state.uptime_secs(),
        "metrics": state.metrics.view(),
        "window": {
            "bucket_secs": h.bucket_secs,
            "totals": totals,
            "series": series,
            "top_queried": domains(&h.top_queried),
            "top_blocked": domains(&h.top_blocked),
        },
        "settings": serde_json::to_value(state.stats_settings()).unwrap_or_default(),
    })
}

pub(super) fn config(state: &SharedState, body: &[u8]) -> AdminResponse {
    let change: crate::stats::StatsOverrides = match serde_json::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            return super::respond::json_status(
                hyper::StatusCode::BAD_REQUEST,
                json!({"error": format!("bad stats config: {e}")}),
            )
        }
    };
    if let Err(e) = state.apply_stats_settings(change) {
        return super::respond::json_status(hyper::StatusCode::BAD_REQUEST, json!({"error": e}));
    }
    let s = state.stats_settings();
    state.log_event(
        EventKind::Info,
        format!(
            "stats settings: retention {} h, log rotation {} h",
            s.retention_hours.unwrap_or_default(),
            s.log_rotate_hours.unwrap_or_default()
        ),
    );
    json_ok(stats_json(state))
}

pub(super) fn reset(state: &SharedState, runtime: &Runtime) -> AdminResponse {
    state.reset_stats();
    if let Some(dns) = runtime.dns() {
        dns.reset_upstream_stats();
    }
    state.log_event(EventKind::Info, "statistics reset");
    json_ok(stats_json(state))
}
