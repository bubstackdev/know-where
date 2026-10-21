use super::{ModelFault, oauth::now};
use axum::http::HeaderMap;
use serde_json::Value;
pub fn instant(s: &str) -> Option<i64> {
    if let Ok(n) = s.parse::<f64>() {
        if !n.is_finite() || n <= 0.0 {
            return None;
        }
        let n = if n > 1e11 { n / 1000.0 } else { n };
        if n > 253402300799.0 {
            return None;
        }
        Some(n as i64)
    } else {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|v| v.timestamp())
            .ok()
            .or_else(|| {
                chrono::DateTime::parse_from_rfc2822(s)
                    .map(|v| v.timestamp())
                    .ok()
            })
    }
}
fn value<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}
fn number(h: &HeaderMap, name: &str) -> Option<f64> {
    value(h, name)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite())
}
pub fn reading(provider: &str, h: &HeaderMap) -> Option<ModelFault> {
    let mut exhausted = false;
    let mut resets = vec![];
    if provider == "claude" {
        for prefix in [
            "anthropic-ratelimit-unified-",
            "anthropic-ratelimit-unified-5h-",
            "anthropic-ratelimit-unified-7d-",
        ] {
            if value(h, &format!("{prefix}status"))
                .is_some_and(|v| v.eq_ignore_ascii_case("rejected"))
                || number(h, &format!("{prefix}utilization")).is_some_and(|v| v >= 1.0)
            {
                exhausted = true;
                if let Some(reset) = value(h, &format!("{prefix}reset")).and_then(instant) {
                    resets.push(reset);
                }
            }
        }
    } else if provider == "codex" {
        for window in ["primary", "secondary"] {
            let p = format!("x-codex-{window}-");
            if number(h, &format!("{p}used-percent")).is_some_and(|n| n >= 100.0) {
                exhausted = true;
                if let Some(reset) =
                    value(h, &format!("{p}reset-at"))
                        .and_then(instant)
                        .or_else(|| {
                            number(h, &format!("{p}reset-after-seconds"))
                                .filter(|n| *n > 0.0)
                                .map(|n| now().saturating_add(n.ceil() as i64))
                        })
                {
                    resets.push(reset);
                }
            }
        }
    }
    if exhausted {
        Some(ModelFault {
            kind: "limited".into(),
            reset_at: resets.into_iter().max(),
            message: None,
        })
    } else {
        None
    }
}
pub fn reset(h: &HeaderMap, body: &Value) -> Option<i64> {
    let retry = value(h, "retry-after").and_then(|s| {
        s.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| now().saturating_add(n.ceil() as i64))
            .or_else(|| instant(s))
    });
    let e = body
        .get("error")
        .or_else(|| body.get("detail"))
        .unwrap_or(body);
    e.get("resets_at")
        .and_then(|v| v.as_str().and_then(instant).or_else(|| v.as_i64()))
        .or_else(|| {
            e["resets_in_seconds"]
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| now().saturating_add(n.ceil() as i64))
        })
        .or(retry)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_exhausted_window_wins() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "1.0".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            "1800000000".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-status",
            "rejected".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-reset",
            "1900000000000".parse().unwrap(),
        );
        assert_eq!(reading("claude", &h).unwrap().reset_at, Some(1900000000));
    }
    #[test]
    fn retry_after_http_date() {
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            "Wed, 21 Oct 2026 12:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(reset(&h, &Value::Null), Some(1792584000));
    }
}
