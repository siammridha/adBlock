//! API handlers for the stats snapshot, logging config, and counter reset.

use serde_json::{json, Value};

use crate::stats::history::Metric;
use crate::dns::DnsService;
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
    let change = match crate::stats::StatsOverrides::parse(body) {
        Ok(c) => c,
        Err(e) => {
            return super::respond::json_status(
                hyper::StatusCode::BAD_REQUEST,
                json!({"error": e}),
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

pub(super) fn exclusions_json(state: &SharedState) -> Value {
    let domains = state.stats_exclusions().map(|s| s.list()).unwrap_or_default();
    json!({ "domains": domains })
}

/// Add or delete a stats-excluded domain. Body: `{"domain": "...", "delete"?: true}`.
pub(super) fn edit_exclusions(state: &SharedState, body: &[u8]) -> AdminResponse {
    use hyper::StatusCode;
    let cmd = match crate::stats::StatsExclusionCommand::parse(body) {
        Ok(c) => c,
        Err(e) => return super::respond::json_status(StatusCode::BAD_REQUEST, json!({"error": e})),
    };
    let Some(store) = state.stats_exclusions() else {
        return super::respond::json_status(
            StatusCode::BAD_REQUEST,
            json!({"error": "stats persistence is not configured"}),
        );
    };
    let outcome = match &cmd {
        crate::stats::StatsExclusionCommand::Delete { domain } => {
            store.remove(domain).map(|removed| {
                if removed {
                    state.log_event(EventKind::Info, format!("stats exclusion removed: {domain}"));
                }
            })
        }
        crate::stats::StatsExclusionCommand::Add { domain } => store.add(domain).map(|_| {
            state.log_event(EventKind::Info, format!("stats exclusion added: {domain}"));
        }),
    };
    match outcome {
        Ok(()) => json_ok(exclusions_json(state)),
        Err(e) => super::respond::json_status(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    }
}

pub(super) fn reset(state: &SharedState, dns: &DnsService) -> AdminResponse {
    state.reset_stats();
    dns.reset_upstream_stats();
    state.log_event(EventKind::Info, "statistics reset");
    json_ok(stats_json(state))
}
