use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

pub const CURRENT_SCHEMA_VERSION: u32 = 4;
pub const MIN_SUPPORTED_SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_AUTO_SWITCH_THRESHOLD_5H_PERCENT: u8 = 1;
pub const DEFAULT_AUTO_SWITCH_THRESHOLD_WEEKLY_PERCENT: u8 = 1;
pub const DEFAULT_LIVE_REFRESH_INTERVAL_SECONDS: u16 = 60;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlanType {
    Free,
    Plus,
    Prolite,
    Pro,
    Team,
    Business,
    Enterprise,
    Edu,
    Unknown,
}

impl Display for PlanType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Free => "free",
            Self::Plus => "plus",
            Self::Prolite => "prolite",
            Self::Pro => "pro",
            Self::Team => "team",
            Self::Business => "business",
            Self::Enterprise => "enterprise",
            Self::Edu => "edu",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    Chatgpt,
    Apikey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateLimitSnapshot {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub plan_type: Option<PlanType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutSignature {
    pub path: String,
    pub event_timestamp_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSwitchConfig {
    pub enabled: bool,
    pub threshold_5h_percent: u8,
    pub threshold_weekly_percent: u8,
}

impl Default for AutoSwitchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_5h_percent: DEFAULT_AUTO_SWITCH_THRESHOLD_5H_PERCENT,
            threshold_weekly_percent: DEFAULT_AUTO_SWITCH_THRESHOLD_WEEKLY_PERCENT,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub usage: bool,
    pub account: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveConfig {
    pub interval_seconds: u16,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            interval_seconds: DEFAULT_LIVE_REFRESH_INTERVAL_SECONDS,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            usage: true,
            account: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub account_key: String,
    pub chatgpt_account_id: String,
    pub chatgpt_user_id: String,
    pub email: String,
    #[serde(default)]
    pub alias: String,
    pub account_name: Option<String>,
    pub plan: Option<PlanType>,
    pub auth_mode: Option<AuthMode>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub last_usage: Option<RateLimitSnapshot>,
    pub last_usage_at: Option<i64>,
    pub last_local_rollout: Option<RolloutSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub schema_version: u32,
    pub active_account_key: Option<String>,
    pub active_account_activated_at_ms: Option<i64>,
    #[serde(default)]
    pub auto_switch: AutoSwitchConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub live: LiveConfig,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            active_account_key: None,
            active_account_activated_at_ms: None,
            auto_switch: AutoSwitchConfig::default(),
            api: ApiConfig::default(),
            live: LiveConfig::default(),
            accounts: Vec::new(),
        }
    }
}

impl Registry {
    pub fn active_account(&self) -> Option<&AccountRecord> {
        let key = self.active_account_key.as_ref()?;
        self.accounts
            .iter()
            .find(|record| &record.account_key == key)
    }
}
