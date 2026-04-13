use crate::auth::{AuthInfo, convert_cpa_auth_json, parse_auth_info, parse_auth_info_data};
use crate::model::{
    AccountRecord, PlanType, RateLimitSnapshot, Registry, RolloutSignature, CURRENT_SCHEMA_VERSION,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::cmp::Ordering;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub codex_home: PathBuf,
}

#[derive(Debug)]
pub struct ImportItem {
    pub label: String,
    pub outcome: ImportOutcome,
}

#[derive(Debug, Clone, Copy)]
pub enum ImportOutcome {
    Imported,
    Updated,
    Skipped,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub items: Vec<ImportItem>,
    pub imported: usize,
    pub updated: usize,
    pub skipped: usize,
}

impl ImportReport {
    pub fn push(&mut self, label: String, outcome: ImportOutcome) {
        match outcome {
            ImportOutcome::Imported => self.imported += 1,
            ImportOutcome::Updated => self.updated += 1,
            ImportOutcome::Skipped => self.skipped += 1,
        }
        self.items.push(ImportItem { label, outcome });
    }

    pub fn applied_count(&self) -> usize {
        self.imported + self.updated
    }
}

pub fn resolve_paths() -> Result<Paths> {
    if let Some(codex_home) = env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Ok(Paths { codex_home });
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME/USERPROFILE is not set"))?;
    Ok(Paths {
        codex_home: home.join(".codex"),
    })
}

pub fn load_registry(paths: &Paths) -> Result<Registry> {
    let path = registry_path(paths);
    if !path.exists() {
        return Ok(Registry::default());
    }
    let bytes = fs::read(&path).with_context(|| format!("failed reading {}", path.display()))?;
    let mut registry: Registry = serde_json::from_slice(&bytes).context("invalid registry json")?;
    registry.schema_version = CURRENT_SCHEMA_VERSION;
    Ok(registry)
}

pub fn save_registry(paths: &Paths, registry: &Registry) -> Result<()> {
    ensure_accounts_dir(paths)?;
    let path = registry_path(paths);
    let data = serde_json::to_vec_pretty(registry)?;
    fs::write(&path, data).with_context(|| format!("failed writing {}", path.display()))
}

pub fn registry_path(paths: &Paths) -> PathBuf {
    paths.codex_home.join("accounts").join("registry.json")
}

pub fn active_auth_path(paths: &Paths) -> PathBuf {
    paths.codex_home.join("auth.json")
}

pub fn ensure_accounts_dir(paths: &Paths) -> Result<()> {
    fs::create_dir_all(paths.codex_home.join("accounts")).context("failed creating accounts directory")
}

pub fn account_auth_path(paths: &Paths, account_key: &str) -> PathBuf {
    let file_key = if key_needs_filename_encoding(account_key) {
        URL_SAFE_NO_PAD.encode(account_key)
    } else {
        account_key.to_owned()
    };
    paths
        .codex_home
        .join("accounts")
        .join(format!("{file_key}.auth.json"))
}

pub fn load_active_auth_info(paths: &Paths) -> Result<Option<AuthInfo>> {
    let path = active_auth_path(paths);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(parse_auth_info(&path)?))
}

#[allow(dead_code)]
pub fn auto_import_active_auth(paths: &Paths, registry: &mut Registry) -> Result<bool> {
    if !registry.accounts.is_empty() {
        return Ok(false);
    }
    let path = active_auth_path(paths);
    if !path.exists() {
        return Ok(false);
    }
    let auth = parse_auth_info(&path)?;
    let Some(record_key) = auth.record_key.as_ref() else {
        return Ok(false);
    };
    ensure_accounts_dir(paths)?;
    fs::copy(&path, account_auth_path(paths, record_key)).context("failed copying active auth snapshot")?;
    upsert_account(registry, account_from_auth("", &auth)?)?;
    set_active_account_key(registry, record_key);
    Ok(true)
}

pub fn sync_active_account_from_auth(paths: &Paths, registry: &mut Registry) -> Result<bool> {
    let Some(info) = load_active_auth_info(paths)? else {
        return Ok(false);
    };
    let Some(record_key) = info.record_key else {
        return Ok(false);
    };
    if registry.active_account_key.as_deref() == Some(record_key.as_str()) {
        return Ok(false);
    }
    if registry.accounts.iter().any(|record| record.account_key == record_key) {
        set_active_account_key(registry, &record_key);
        return Ok(true);
    }
    Ok(false)
}

pub fn activate_account_by_key(paths: &Paths, registry: &mut Registry, account_key: &str) -> Result<()> {
    let source = account_auth_path(paths, account_key);
    if !source.exists() {
        bail!("account snapshot not found for `{account_key}`");
    }
    let target = active_auth_path(paths);
    ensure_accounts_dir(paths)?;
    fs::copy(&source, &target).with_context(|| format!("failed switching active auth to {}", source.display()))?;
    set_active_account_key(registry, account_key);
    let now = now_ms();
    registry.active_account_activated_at_ms = Some(now);
    if let Some(record) = registry.accounts.iter_mut().find(|record| record.account_key == account_key) {
        record.last_used_at = Some(now / 1000);
    }
    Ok(())
}

pub fn set_active_account_key(registry: &mut Registry, account_key: &str) {
    registry.active_account_key = Some(account_key.to_owned());
    registry.active_account_activated_at_ms = Some(now_ms());
}

pub fn account_from_auth(alias: &str, auth: &AuthInfo) -> Result<AccountRecord> {
    let record_key = auth
        .record_key
        .as_ref()
        .ok_or_else(|| anyhow!("auth is missing record key"))?;
    let email = auth
        .email
        .as_ref()
        .ok_or_else(|| anyhow!("auth is missing email"))?;
    let chatgpt_account_id = auth
        .chatgpt_account_id
        .as_ref()
        .ok_or_else(|| anyhow!("auth is missing chatgpt account id"))?;
    let chatgpt_user_id = auth
        .chatgpt_user_id
        .as_ref()
        .ok_or_else(|| anyhow!("auth is missing chatgpt user id"))?;

    Ok(AccountRecord {
        account_key: record_key.clone(),
        chatgpt_account_id: chatgpt_account_id.clone(),
        chatgpt_user_id: chatgpt_user_id.clone(),
        email: email.clone(),
        alias: alias.to_owned(),
        account_name: None,
        plan: auth.plan,
        auth_mode: Some(auth.auth_mode),
        created_at: now_sec(),
        last_used_at: None,
        last_usage: None,
        last_usage_at: None,
        last_local_rollout: None,
    })
}

pub fn upsert_account(registry: &mut Registry, record: AccountRecord) -> Result<ImportOutcome> {
    if let Some(existing) = registry
        .accounts
        .iter_mut()
        .find(|existing| existing.account_key == record.account_key)
    {
        let last_usage = existing.last_usage.clone();
        let last_usage_at = existing.last_usage_at;
        let last_local_rollout = existing.last_local_rollout.clone();
        let last_used_at = existing.last_used_at;
        *existing = record;
        existing.last_usage = last_usage;
        existing.last_usage_at = last_usage_at;
        existing.last_local_rollout = last_local_rollout;
        existing.last_used_at = last_used_at;
        return Ok(ImportOutcome::Updated);
    }
    registry.accounts.push(record);
    Ok(ImportOutcome::Imported)
}

pub fn import_standard_path(
    paths: &Paths,
    registry: &mut Registry,
    path: &Path,
    alias: Option<&str>,
) -> Result<ImportReport> {
    import_path_impl(paths, registry, path, alias, false)
}

pub fn import_cpa_path(
    paths: &Paths,
    registry: &mut Registry,
    path: Option<&Path>,
    alias: Option<&str>,
) -> Result<ImportReport> {
    let root = path
        .map(PathBuf::from)
        .unwrap_or_else(default_cpa_root);
    import_path_impl(paths, registry, &root, alias, true)
}

pub fn purge_registry_from_path(paths: &Paths, source: Option<&Path>, alias: Option<&str>) -> Result<ImportReport> {
    let mut registry = Registry::default();
    let root = source
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.codex_home.join("accounts"));

    let mut report = ImportReport::default();
    let files = collect_json_files(&root)?;
    for file in files {
        if file.file_name().and_then(|name| name.to_str()) == Some("registry.json") {
            continue;
        }
        match parse_auth_info(&file) {
            Ok(info) => {
                let Some(label) = info.email.clone() else {
                    report.push(file.display().to_string(), ImportOutcome::Skipped);
                    continue;
                };
                let snapshot_path = if let Some(key) = info.record_key.as_deref() {
                    account_auth_path(paths, key)
                } else {
                    file.clone()
                };
                if snapshot_path != file {
                    ensure_accounts_dir(paths)?;
                    let _ = fs::copy(&file, &snapshot_path);
                }
                let mut record = account_from_auth(alias.unwrap_or(""), &info)?;
                record.alias = alias.unwrap_or("").to_owned();
                let _ = upsert_account(&mut registry, record)?;
                report.push(label, ImportOutcome::Imported);
            }
            Err(_) => report.push(file.display().to_string(), ImportOutcome::Skipped),
        }
    }

    if report.applied_count() > 0 {
        if let Some(active) = load_active_auth_info(paths)? {
            if let Some(key) = active.record_key {
                registry.active_account_key = Some(key);
            }
        }
        save_registry(paths, &registry)?;
    }

    Ok(report)
}

pub fn remove_accounts(paths: &Paths, registry: &mut Registry, account_keys: &[String]) -> Result<()> {
    registry.accounts.retain(|record| {
        if account_keys.iter().any(|key| key == &record.account_key) {
            let snapshot = account_auth_path(paths, &record.account_key);
            let _ = fs::remove_file(snapshot);
            false
        } else {
            true
        }
    });

    if registry
        .active_account_key
        .as_ref()
        .is_some_and(|key| account_keys.iter().any(|removed| removed == key))
    {
        registry.active_account_key = select_best_account_key_by_usage(registry);
        if let Some(next_key) = registry.active_account_key.clone() {
            let _ = activate_account_by_key(paths, registry, &next_key);
        } else {
            let _ = fs::remove_file(active_auth_path(paths));
        }
    }

    Ok(())
}

pub fn clean_accounts_dir(paths: &Paths, registry: &Registry) -> Result<(usize, usize, usize)> {
    let accounts_dir = paths.codex_home.join("accounts");
    if !accounts_dir.exists() {
        return Ok((0, 0, 0));
    }

    let mut stale = 0usize;
    for entry in fs::read_dir(&accounts_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "registry.json" || name.ends_with(".bak") {
            continue;
        }
        let tracked = registry
            .accounts
            .iter()
            .any(|record| account_auth_path(paths, &record.account_key).file_name() == path.file_name());
        if !tracked && name.ends_with(".auth.json") {
            fs::remove_file(path)?;
            stale += 1;
        }
    }

    Ok((0, 0, stale))
}

pub fn find_matching_accounts<'a>(registry: &'a Registry, query: &str) -> Vec<&'a AccountRecord> {
    let query = query.to_ascii_lowercase();
    registry
        .accounts
        .iter()
        .filter(|record| {
            record.email.to_ascii_lowercase().contains(&query)
                || (!record.alias.is_empty() && record.alias.to_ascii_lowercase().contains(&query))
                || record
                    .account_name
                    .as_ref()
                    .is_some_and(|name| name.to_ascii_lowercase().contains(&query))
        })
        .collect()
}

pub fn select_best_account_key_by_usage(registry: &Registry) -> Option<String> {
    registry
        .accounts
        .iter()
        .max_by(compare_usage_priority)
        .map(|record| record.account_key.clone())
}

pub fn update_account_usage(
    registry: &mut Registry,
    account_key: &str,
    usage: RateLimitSnapshot,
    last_usage_at_ms: i64,
    rollout_path: String,
    event_timestamp_ms: i64,
) -> bool {
    if let Some(record) = registry.accounts.iter_mut().find(|record| record.account_key == account_key) {
        let changed = record.last_usage.as_ref() != Some(&usage)
            || record.last_usage_at != Some(last_usage_at_ms / 1000)
            || record
                .last_local_rollout
                .as_ref()
                .is_none_or(|rollout| rollout.path != rollout_path || rollout.event_timestamp_ms != event_timestamp_ms);
        record.last_usage = Some(usage);
        record.last_usage_at = Some(last_usage_at_ms / 1000);
        record.last_local_rollout = Some(RolloutSignature {
            path: rollout_path,
            event_timestamp_ms,
        });
        return changed;
    }
    false
}

pub fn update_plan_from_usage(registry: &mut Registry) -> bool {
    let mut changed = false;
    for record in &mut registry.accounts {
        if record.plan.is_none() {
            record.plan = record.last_usage.as_ref().and_then(|usage| usage.plan_type);
            changed |= record.plan.is_some();
        }
    }
    changed
}

fn import_path_impl(
    paths: &Paths,
    registry: &mut Registry,
    path: &Path,
    alias: Option<&str>,
    cpa: bool,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let files = collect_json_files(path)?;
    ensure_accounts_dir(paths)?;

    for file in files {
        let data = fs::read(&file).with_context(|| format!("failed reading {}", file.display()))?;
        let prepared = if cpa {
            convert_cpa_auth_json(&data)?
        } else {
            data
        };

        match parse_auth_info_data(&prepared) {
            Ok(info) => {
                let Some(record_key) = info.record_key.as_ref() else {
                    report.push(file.display().to_string(), ImportOutcome::Skipped);
                    continue;
                };
                let label = info.email.clone().unwrap_or_else(|| file.display().to_string());
                let snapshot_path = account_auth_path(paths, record_key);
                fs::write(&snapshot_path, &prepared)
                    .with_context(|| format!("failed writing {}", snapshot_path.display()))?;
                let mut record = account_from_auth(alias.unwrap_or(""), &info)?;
                record.alias = alias.unwrap_or("").to_owned();
                let outcome = upsert_account(registry, record)?;
                report.push(label, outcome);
            }
            Err(_) => report.push(file.display().to_string(), ImportOutcome::Skipped),
        }
    }

    Ok(report)
}

fn collect_json_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_path = entry.path();
        if file_path.is_file()
            && file_path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            files.push(file_path);
        }
    }
    files.sort();
    Ok(files)
}

fn default_cpa_root() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cli-proxy-api")
}

fn key_needs_filename_encoding(key: &str) -> bool {
    key.is_empty()
        || key == "."
        || key == ".."
        || key
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_' | '.'))
}

fn compare_usage_priority(a: &&AccountRecord, b: &&AccountRecord) -> Ordering {
    let a_score = usage_score(a.last_usage.as_ref());
    let b_score = usage_score(b.last_usage.as_ref());
    a_score
        .cmp(&b_score)
        .then_with(|| a.last_usage_at.cmp(&b.last_usage_at))
}

fn usage_score(usage: Option<&RateLimitSnapshot>) -> i64 {
    usage
        .and_then(|snapshot| snapshot.primary.as_ref().or(snapshot.secondary.as_ref()))
        .map(|window| (100.0 - window.used_percent).round() as i64)
        .unwrap_or(-1)
}

fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[allow(dead_code)]
fn parse_plan_from_value(value: &str) -> PlanType {
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
