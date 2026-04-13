use crate::auth::parse_auth_info;
use crate::model::{CreditsSnapshot, PlanType, RateLimitSnapshot, RateLimitWindow};
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_USAGE_ENDPOINT: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const DEFAULT_ACCOUNT_ENDPOINT: &str =
    "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27";
const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct UsageFetchResult {
    pub snapshot: Option<RateLimitSnapshot>,
    pub status_code: Option<u16>,
    pub missing_auth: bool,
}

#[derive(Debug, Clone)]
pub struct AccountEntry {
    pub account_id: String,
    pub account_name: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AccountFetchResult {
    pub entries: Option<Vec<AccountEntry>>,
    pub status_code: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    credits: Option<CreditsJson>,
    rate_limit: Option<RateLimitJson>,
}

#[derive(Debug, Deserialize)]
struct CreditsJson {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateLimitJson {
    primary_window: Option<ApiWindowJson>,
    secondary_window: Option<ApiWindowJson>,
}

#[derive(Debug, Deserialize)]
struct ApiWindowJson {
    used_percent: f64,
    limit_window_seconds: Option<i64>,
    reset_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AccountsResponse {
    accounts: Option<serde_json::Map<String, serde_json::Value>>,
}

pub fn fetch_usage_for_auth_path(auth_path: &Path) -> Result<UsageFetchResult> {
    let info = parse_auth_info(auth_path)?;
    let (Some(access_token), Some(account_id)) = (info.access_token, info.chatgpt_account_id) else {
        return Ok(UsageFetchResult {
            snapshot: None,
            status_code: None,
            missing_auth: true,
        });
    };
    fetch_usage_for_token(DEFAULT_USAGE_ENDPOINT, &access_token, &account_id)
}

pub fn fetch_usage_for_token(endpoint: &str, access_token: &str, account_id: &str) -> Result<UsageFetchResult> {
    let client = build_client()?;
    let response = client
        .get(endpoint)
        .headers(build_headers(access_token, account_id)?)
        .send()
        .context("usage request failed")?;
    let status_code = Some(response.status().as_u16());
    let body = response.text().context("failed reading usage response body")?;
    if body.trim().is_empty() {
        return Ok(UsageFetchResult {
            snapshot: None,
            status_code,
            missing_auth: false,
        });
    }
    let snapshot = parse_usage_response(&body)?;
    Ok(UsageFetchResult {
        snapshot,
        status_code,
        missing_auth: false,
    })
}

pub fn fetch_accounts_for_token(endpoint: &str, access_token: &str, account_id: &str) -> Result<AccountFetchResult> {
    let client = build_client()?;
    let response = client
        .get(endpoint)
        .headers(build_headers(access_token, account_id)?)
        .send()
        .context("account metadata request failed")?;
    let status_code = Some(response.status().as_u16());
    let body = response.text().context("failed reading account metadata response body")?;
    if body.trim().is_empty() {
        return Ok(AccountFetchResult {
            entries: None,
            status_code,
        });
    }
    Ok(AccountFetchResult {
        entries: parse_accounts_response(&body)?,
        status_code,
    })
}

pub fn parse_usage_response(body: &str) -> Result<Option<RateLimitSnapshot>> {
    let parsed: UsageResponse = serde_json::from_str(body).context("invalid usage response json")?;
    let snapshot = RateLimitSnapshot {
        primary: parsed.rate_limit.as_ref().and_then(|rate| rate.primary_window.as_ref()).map(parse_window),
        secondary: parsed.rate_limit.as_ref().and_then(|rate| rate.secondary_window.as_ref()).map(parse_window),
        credits: parsed.credits.map(|credits| CreditsSnapshot {
            has_credits: credits.has_credits,
            unlimited: credits.unlimited,
            balance: credits.balance.filter(|value| !value.is_empty()),
        }),
        plan_type: parsed.plan_type.as_deref().map(parse_plan_type),
    };
    if snapshot.primary.is_none() && snapshot.secondary.is_none() {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

pub fn parse_accounts_response(body: &str) -> Result<Option<Vec<AccountEntry>>> {
    let parsed: AccountsResponse = serde_json::from_str(body).context("invalid accounts response json")?;
    let Some(accounts) = parsed.accounts else {
        return Ok(None);
    };

    let mut entries = Vec::new();
    for (key, value) in accounts {
        if key == "default" {
            continue;
        }
        let Some(account) = value
            .get("account")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let Some(account_id) = account.get("account_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if account_id.is_empty() {
            continue;
        }
        let account_name = account
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
        entries.push(AccountEntry {
            account_id: account_id.to_owned(),
            account_name,
        });
    }

    Ok(Some(entries))
}

fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed building http client")
}

fn build_headers(access_token: &str, account_id: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}")).context("invalid auth header")?,
    );
    headers.insert(
        "ChatGPT-Account-Id",
        HeaderValue::from_str(account_id).context("invalid account id header")?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_USER_AGENT));
    Ok(headers)
}

fn parse_window(window: &ApiWindowJson) -> RateLimitWindow {
    RateLimitWindow {
        used_percent: window.used_percent,
        window_minutes: window.limit_window_seconds.map(ceil_minutes),
        resets_at: window.reset_at,
    }
}

fn ceil_minutes(seconds: i64) -> i64 {
    (seconds + 59) / 60
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
