use crate::model::{CreditsSnapshot, PlanType, RateLimitSnapshot, RateLimitWindow};
use anyhow::{Context, Result};
use chrono::DateTime;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct LatestUsage {
    pub path: PathBuf,
    pub event_timestamp_ms: i64,
    pub snapshot: RateLimitSnapshot,
}

#[derive(Debug, Deserialize)]
struct UsageEventLineJson {
    timestamp: String,
    #[serde(rename = "type")]
    kind: String,
    payload: UsagePayloadJson,
}

#[derive(Debug, Deserialize)]
struct UsagePayloadJson {
    #[serde(rename = "type")]
    kind: String,
    rate_limits: Option<UsageRateLimitsJson>,
}

#[derive(Debug, Deserialize)]
struct UsageRateLimitsJson {
    primary: Option<UsageWindowJson>,
    secondary: Option<UsageWindowJson>,
    credits: Option<UsageCreditsJson>,
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageWindowJson {
    used_percent: Option<serde_json::Value>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct UsageCreditsJson {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

pub fn scan_latest_usage(codex_home: &Path) -> Result<Option<LatestUsage>> {
    let sessions_root = codex_home.join("sessions");
    if !sessions_root.exists() {
        return Ok(None);
    }

    let mut best: Option<LatestUsage> = None;
    for entry in WalkDir::new(&sessions_root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !entry.file_type().is_file() || !is_rollout_file(path) {
            continue;
        }

        let data = fs::read_to_string(path).with_context(|| format!("failed reading {}", path.display()))?;
        for line in data.lines() {
            let Some(parsed) = parse_usage_event_line(line) else {
                continue;
            };
            let candidate = LatestUsage {
                path: path.to_path_buf(),
                event_timestamp_ms: parsed.0,
                snapshot: parsed.1,
            };

            let replace = best
                .as_ref()
                .is_none_or(|best| candidate.event_timestamp_ms >= best.event_timestamp_ms);
            if replace {
                best = Some(candidate);
            }
        }
    }

    Ok(best)
}

fn parse_usage_event_line(line: &str) -> Option<(i64, RateLimitSnapshot)> {
    if !(line.contains("\"event_msg\"")
        && line.contains("\"token_count\"")
        && line.contains("\"rate_limits\"")
        && line.contains("\"timestamp\""))
    {
        return None;
    }

    let parsed: UsageEventLineJson = serde_json::from_str(line).ok()?;
    if parsed.kind != "event_msg" || parsed.payload.kind != "token_count" {
        return None;
    }
    let rate_limits = parsed.payload.rate_limits?;
    let snapshot = parse_rate_limits(rate_limits)?;
    let timestamp = DateTime::parse_from_rfc3339(&parsed.timestamp).ok()?;
    Some((timestamp.timestamp_millis(), snapshot))
}

fn parse_rate_limits(parsed: UsageRateLimitsJson) -> Option<RateLimitSnapshot> {
    let snapshot = RateLimitSnapshot {
        primary: parsed.primary.and_then(parse_window),
        secondary: parsed.secondary.and_then(parse_window),
        credits: parsed.credits.map(|credits| CreditsSnapshot {
            has_credits: credits.has_credits,
            unlimited: credits.unlimited,
            balance: credits.balance,
        }),
        plan_type: parsed.plan_type.as_deref().map(parse_plan_type),
    };
    if snapshot.primary.is_none() && snapshot.secondary.is_none() {
        return None;
    }
    Some(snapshot)
}

fn parse_window(parsed: UsageWindowJson) -> Option<RateLimitWindow> {
    let used_percent = match parsed.used_percent? {
        serde_json::Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    Some(RateLimitWindow {
        used_percent,
        window_minutes: parsed.window_minutes,
        resets_at: parsed.resets_at,
    })
}

fn parse_plan_type(value: &str) -> PlanType {
    match value.to_ascii_lowercase().as_str() {
        "free" => PlanType::Free,
        "plus" => PlanType::Plus,
        "pro" => PlanType::Pro,
        "team" => PlanType::Team,
        "business" => PlanType::Business,
        "enterprise" => PlanType::Enterprise,
        "edu" => PlanType::Edu,
        _ => PlanType::Unknown,
    }
}

fn is_rollout_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}
