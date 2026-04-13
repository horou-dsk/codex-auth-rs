use crate::model::{AuthMode, PlanType};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub email: Option<String>,
    pub chatgpt_account_id: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub record_key: Option<String>,
    pub access_token: Option<String>,
    pub last_refresh: Option<String>,
    pub plan: Option<PlanType>,
    pub auth_mode: AuthMode,
}

#[derive(Debug, Serialize)]
struct StandardAuthJson<'a> {
    auth_mode: &'a str,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<&'a str>,
    tokens: StandardAuthTokens<'a>,
    last_refresh: &'a str,
}

#[derive(Debug, Serialize)]
struct StandardAuthTokens<'a> {
    id_token: &'a str,
    access_token: &'a str,
    refresh_token: &'a str,
    account_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct JwtAuthClaim {
    chatgpt_account_id: Option<String>,
    chatgpt_plan_type: Option<String>,
    chatgpt_user_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<JwtAuthClaim>,
}

pub fn parse_auth_info(path: &Path) -> Result<AuthInfo> {
    let data = fs::read(path).with_context(|| format!("failed reading {}", path.display()))?;
    parse_auth_info_data(&data)
}

pub fn parse_auth_info_data(data: &[u8]) -> Result<AuthInfo> {
    let value: Value = serde_json::from_slice(data).context("invalid auth json")?;
    let Some(obj) = value.as_object() else {
        bail!("auth file root must be an object");
    };

    if obj
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.is_empty())
    {
        return Ok(AuthInfo {
            email: None,
            chatgpt_account_id: None,
            chatgpt_user_id: None,
            record_key: None,
            access_token: None,
            last_refresh: None,
            plan: None,
            auth_mode: AuthMode::Apikey,
        });
    }

    let last_refresh = obj
        .get("last_refresh")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let tokens = obj.get("tokens").and_then(Value::as_object);
    let access_token = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let account_id = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let id_token = tokens
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    if let Some(id_token) = id_token {
        let payload = decode_jwt_payload(id_token)?;
        let claims: JwtClaims = serde_json::from_slice(&payload).context("invalid jwt payload json")?;
        let email = claims.email.map(|s| s.to_ascii_lowercase());
        let auth = claims.auth;
        let jwt_account_id = auth.as_ref().and_then(|auth| auth.chatgpt_account_id.clone());
        let plan = auth
            .as_ref()
            .and_then(|auth| auth.chatgpt_plan_type.as_deref())
            .map(parse_plan_type);
        let chatgpt_user_id = auth
            .as_ref()
            .and_then(|auth| auth.chatgpt_user_id.clone().or_else(|| auth.user_id.clone()));

        match (account_id.as_deref(), jwt_account_id.as_deref(), chatgpt_user_id.as_deref()) {
            (Some(token_account_id), Some(jwt_account_id), Some(user_id)) if token_account_id == jwt_account_id => {
                return Ok(AuthInfo {
                    email,
                    chatgpt_account_id: account_id.clone(),
                    chatgpt_user_id: chatgpt_user_id.clone(),
                    record_key: Some(format!("{user_id}::{token_account_id}")),
                    access_token,
                    last_refresh,
                    plan,
                    auth_mode: AuthMode::Chatgpt,
                });
            }
            _ => {}
        }
    }

    Ok(AuthInfo {
        email: None,
        chatgpt_account_id: account_id,
        chatgpt_user_id: None,
        record_key: None,
        access_token,
        last_refresh,
        plan: None,
        auth_mode: AuthMode::Chatgpt,
    })
}

pub fn convert_cpa_auth_json(data: &[u8]) -> Result<Vec<u8>> {
    let value: Value = serde_json::from_slice(data).context("invalid CPA json")?;
    let Some(obj) = value.as_object() else {
        bail!("invalid CPA format");
    };
    let refresh_token = obj
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing refresh_token"))?;

    let standard = StandardAuthJson {
        auth_mode: "chatgpt",
        openai_api_key: None,
        tokens: StandardAuthTokens {
            id_token: string_field(obj, "id_token").unwrap_or_default(),
            access_token: string_field(obj, "access_token").unwrap_or_default(),
            refresh_token,
            account_id: string_field(obj, "account_id").unwrap_or_default(),
        },
        last_refresh: string_field(obj, "last_refresh").unwrap_or_default(),
    };
    let mut data = serde_json::to_vec_pretty(&standard)?;
    data.push(b'\n');
    Ok(data)
}

fn decode_jwt_payload(jwt: &str) -> Result<Vec<u8>> {
    let mut parts = jwt.split('.');
    let _header = parts.next().ok_or_else(|| anyhow!("invalid jwt"))?;
    let payload = parts.next().ok_or_else(|| anyhow!("invalid jwt"))?;
    let _sig = parts.next().ok_or_else(|| anyhow!("invalid jwt"))?;
    URL_SAFE_NO_PAD
        .decode(payload)
        .context("invalid base64url jwt payload")
}

fn string_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
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
