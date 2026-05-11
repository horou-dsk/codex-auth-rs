use crate::auth::{AuthInfo, convert_cpa_auth_json, parse_auth_info, parse_auth_info_data};
use crate::chatgpt_api::{self, MeFetchResult};
use crate::model::{
    AccountRecord, AuthMode, CURRENT_SCHEMA_VERSION, MIN_SUPPORTED_SCHEMA_VERSION, PlanType,
    RateLimitSnapshot, Registry, RolloutSignature,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Local;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
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

#[derive(Debug, Deserialize)]
struct LegacyAccountRecord {
    email: String,
    #[serde(default)]
    alias: String,
    plan: Option<PlanType>,
    #[serde(default = "now_sec")]
    created_at: i64,
    last_used_at: Option<i64>,
    last_usage: Option<RateLimitSnapshot>,
    last_usage_at: Option<i64>,
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
    let value: Value = serde_json::from_slice(&bytes).context("invalid registry json")?;
    let Some(root) = value.as_object() else {
        return Ok(Registry::default());
    };
    let schema_version = detect_schema_version(root);
    if schema_version > CURRENT_SCHEMA_VERSION {
        bail!(
            "registry schema_version {schema_version} is newer than this codex-auth binary supports (max {CURRENT_SCHEMA_VERSION}); upgrade codex-auth"
        );
    }
    if schema_version < MIN_SUPPORTED_SCHEMA_VERSION {
        bail!(
            "registry schema_version {schema_version} is older than the minimum supported {MIN_SUPPORTED_SCHEMA_VERSION}; use an intermediate codex-auth release or import --purge"
        );
    }

    let needs_rewrite = schema_version < CURRENT_SCHEMA_VERSION
        || (root.get("schema_version").is_none() && root.get("version").is_some())
        || root.get("live").is_none()
        || (root.get("active_account_key").is_some()
            && root.get("active_account_activated_at_ms").is_none());
    let mut registry = match schema_version {
        2 => load_legacy_v2_registry(paths, root)?,
        3 | 4 => {
            let mut current = value.clone();
            if let Some(obj) = current.as_object_mut() {
                obj.insert(
                    "schema_version".to_owned(),
                    Value::Number(serde_json::Number::from(schema_version)),
                );
            }
            serde_json::from_value::<Registry>(current).context("invalid registry json")?
        }
        _ => unreachable!(),
    };
    registry.schema_version = CURRENT_SCHEMA_VERSION;
    if schema_version < CURRENT_SCHEMA_VERSION {
        registry.auto_switch.threshold_5h_percent =
            crate::model::DEFAULT_AUTO_SWITCH_THRESHOLD_5H_PERCENT;
        registry.auto_switch.threshold_weekly_percent =
            crate::model::DEFAULT_AUTO_SWITCH_THRESHOLD_WEEKLY_PERCENT;
    }
    if registry.active_account_key.is_some() && registry.active_account_activated_at_ms.is_none() {
        registry.active_account_activated_at_ms = Some(0);
    }
    if needs_rewrite {
        save_registry(paths, &registry)?;
    }
    Ok(registry)
}

pub fn save_registry(paths: &Paths, registry: &Registry) -> Result<()> {
    ensure_accounts_dir(paths)?;
    let path = registry_path(paths);
    let data = serde_json::to_vec_pretty(registry)?;
    backup_registry_if_changed(paths, &path, &data)?;
    write_managed_file(&path, &data).with_context(|| format!("failed writing {}", path.display()))
}

pub fn registry_path(paths: &Paths) -> PathBuf {
    paths.codex_home.join("accounts").join("registry.json")
}

pub fn active_auth_path(paths: &Paths) -> PathBuf {
    paths.codex_home.join("auth.json")
}

pub fn ensure_accounts_dir(paths: &Paths) -> Result<()> {
    let dir = paths.codex_home.join("accounts");
    fs::create_dir_all(&dir).context("failed creating accounts directory")?;
    harden_sensitive_dir(&dir)
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

pub fn copy_managed_file(src: &Path, dest: &Path) -> Result<()> {
    if src == dest {
        return Ok(());
    }
    let data = fs::read(src).with_context(|| format!("failed reading {}", src.display()))?;
    write_managed_file(dest, &data)
}

fn write_managed_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
        harden_sensitive_dir(parent)?;
    }
    write_file_private(path, data)?;
    harden_sensitive_file(path)
}

fn replace_file_preserving_permissions(src: &Path, dest: &Path) -> Result<()> {
    let data = fs::read(src).with_context(|| format!("failed reading {}", src.display()))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let existing_permissions = fs::metadata(dest)
        .ok()
        .map(|metadata| metadata.permissions());
    write_file_private(dest, &data)?;
    if let Some(permissions) = existing_permissions {
        fs::set_permissions(dest, permissions)
            .with_context(|| format!("failed preserving permissions for {}", dest.display()))?;
    } else {
        harden_sensitive_file(dest)?;
    }
    Ok(())
}

fn backup_auth_if_changed(
    paths: &Paths,
    current_auth_path: &Path,
    replacement_path: &Path,
) -> Result<()> {
    if !current_auth_path.exists() || files_equal(current_auth_path, replacement_path)? {
        return Ok(());
    }
    let backup = next_backup_path(paths, "auth.json.bak.")?;
    copy_managed_file(current_auth_path, &backup)?;
    prune_backups(paths, "auth.json.bak.")
}

fn backup_registry_if_changed(
    paths: &Paths,
    current_registry_path: &Path,
    replacement_data: &[u8],
) -> Result<()> {
    if !current_registry_path.exists()
        || file_equals_bytes(current_registry_path, replacement_data)?
    {
        return Ok(());
    }
    let backup = next_backup_path(paths, "registry.json.bak.")?;
    copy_managed_file(current_registry_path, &backup)?;
    prune_backups(paths, "registry.json.bak.")
}

fn next_backup_path(paths: &Paths, prefix: &str) -> Result<PathBuf> {
    ensure_accounts_dir(paths)?;
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let dir = paths.codex_home.join("accounts");
    for suffix in 0..1000usize {
        let name = if suffix == 0 {
            format!("{prefix}{timestamp}")
        } else {
            format!("{prefix}{timestamp}.{suffix}")
        };
        let path = dir.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!("failed finding free backup path for {prefix}")
}

fn prune_backups(paths: &Paths, prefix: &str) -> Result<()> {
    const MAX_BACKUPS: usize = 5;
    let dir = paths.codex_home.join("accounts");
    if !dir.exists() {
        return Ok(());
    }
    let mut backups = fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    backups.sort();
    let remove_count = backups.len().saturating_sub(MAX_BACKUPS);
    for path in backups.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

fn files_equal(lhs: &Path, rhs: &Path) -> Result<bool> {
    if !lhs.exists() || !rhs.exists() {
        return Ok(false);
    }
    Ok(fs::read(lhs)? == fs::read(rhs)?)
}

fn file_equals_bytes(path: &Path, bytes: &[u8]) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    Ok(fs::read(path)? == bytes)
}

#[cfg(unix)]
fn write_file_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed opening {}", path.display()))?;
    file.write_all(data)
        .with_context(|| format!("failed writing {}", path.display()))
}

#[cfg(not(unix))]
fn write_file_private(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data).with_context(|| format!("failed writing {}", path.display()))
}

#[cfg(unix)]
fn harden_sensitive_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed setting permissions for {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_sensitive_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn harden_sensitive_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed setting permissions for {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_sensitive_dir(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn load_active_auth_info(paths: &Paths) -> Result<Option<AuthInfo>> {
    let path = active_auth_path(paths);
    if !path.exists() {
        return Ok(None);
    }
    Ok(parse_auth_info(&path).ok())
}

fn detect_schema_version(root: &serde_json::Map<String, Value>) -> u32 {
    root.get("schema_version")
        .or_else(|| root.get("version"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| {
            if root.get("active_email").is_some() {
                2
            } else {
                CURRENT_SCHEMA_VERSION
            }
        })
}

fn load_legacy_v2_registry(
    paths: &Paths,
    root: &serde_json::Map<String, Value>,
) -> Result<Registry> {
    let mut registry = Registry::default();
    registry.active_account_key = root
        .get("active_account_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    if registry.active_account_key.is_some() {
        registry.active_account_activated_at_ms = Some(0);
    }
    let active_email = root
        .get("active_email")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());

    if let Some(value) = root.get("auto_switch") {
        registry.auto_switch = serde_json::from_value(value.clone()).unwrap_or_default();
    }
    if let Some(value) = root.get("api") {
        registry.api = serde_json::from_value(value.clone()).unwrap_or_default();
    }
    if let Some(value) = root.get("live") {
        registry.live = serde_json::from_value(value.clone()).unwrap_or_default();
    }

    let Some(accounts) = root.get("accounts").and_then(Value::as_array) else {
        return Ok(registry);
    };
    for item in accounts {
        if item.get("account_key").is_some() {
            let record = serde_json::from_value::<AccountRecord>(item.clone())
                .context("invalid account record")?;
            let _ = upsert_account(&mut registry, record)?;
            continue;
        }

        let legacy = serde_json::from_value::<LegacyAccountRecord>(item.clone())
            .context("invalid legacy account record")?;
        let migrated = migrate_legacy_account(paths, &legacy)
            .with_context(|| format!("failed migrating legacy account {}", legacy.email))?;
        let should_activate = registry.active_account_key.is_none()
            && active_email
                .as_deref()
                .is_some_and(|email| email == migrated.email);
        if should_activate {
            registry.active_account_key = Some(migrated.account_key.clone());
            registry.active_account_activated_at_ms = Some(0);
        }
        let _ = upsert_account(&mut registry, migrated)?;
    }

    Ok(registry)
}

fn migrate_legacy_account(paths: &Paths, legacy: &LegacyAccountRecord) -> Result<AccountRecord> {
    let legacy_email = legacy.email.to_ascii_lowercase();
    let legacy_path = resolve_legacy_snapshot_path(paths, &legacy_email)?;
    let info = parse_auth_info(&legacy_path)?;
    let mut record = account_from_auth(&legacy.alias, &info)?;
    if record.email != legacy_email {
        bail!("legacy account email mismatch");
    }
    record.plan = info.plan.or(legacy.plan);
    record.auth_mode = Some(info.auth_mode);
    record.created_at = legacy.created_at;
    record.last_used_at = legacy.last_used_at;
    record.last_usage = legacy.last_usage.clone();
    record.last_usage_at = legacy.last_usage_at;

    let new_path = account_auth_path(paths, &record.account_key);
    ensure_accounts_dir(paths)?;
    copy_managed_file(&legacy_path, &new_path)?;
    let old_legacy_path = legacy_account_auth_path(paths, &legacy_email);
    if legacy_path == old_legacy_path && new_path != old_legacy_path {
        let _ = fs::remove_file(old_legacy_path);
    }
    Ok(record)
}

fn resolve_legacy_snapshot_path(paths: &Paths, email: &str) -> Result<PathBuf> {
    let legacy = legacy_account_auth_path(paths, email);
    if legacy.exists() {
        return Ok(legacy);
    }
    let accounts_dir = paths.codex_home.join("accounts");
    if accounts_dir.exists() {
        for entry in fs::read_dir(&accounts_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file()
                || !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.ends_with(".auth.json") && !name.starts_with("auth.json.bak.")
                    })
            {
                continue;
            }
            let Ok(info) = parse_auth_info(&path) else {
                continue;
            };
            if info.email.as_deref() == Some(email) {
                return Ok(path);
            }
        }
    }
    let active = active_auth_path(paths);
    if active.exists() {
        if let Ok(info) = parse_auth_info(&active) {
            if info.email.as_deref() == Some(email) {
                return Ok(active);
            }
        }
    }
    bail!("legacy snapshot not found for {email}")
}

fn legacy_account_auth_path(paths: &Paths, email: &str) -> PathBuf {
    let key = URL_SAFE_NO_PAD.encode(email);
    paths
        .codex_home
        .join("accounts")
        .join(format!("{key}.auth.json"))
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
    let record = account_from_auth_info("", &auth)?;
    let record_key = record.account_key.clone();
    ensure_accounts_dir(paths)?;
    copy_managed_file(&path, &account_auth_path(paths, &record_key))
        .context("failed copying active auth snapshot")?;
    upsert_account(registry, record)?;
    set_active_account_key(registry, &record_key);
    Ok(true)
}

pub fn sync_active_account_from_auth(paths: &Paths, registry: &mut Registry) -> Result<bool> {
    if registry.accounts.is_empty() {
        return auto_import_active_auth(paths, registry);
    }
    let auth_path = active_auth_path(paths);
    if !auth_path.exists() {
        return Ok(false);
    };
    let auth_bytes =
        fs::read(&auth_path).with_context(|| format!("failed reading {}", auth_path.display()))?;
    let info = match parse_auth_info(&auth_path) {
        Ok(info) => info,
        Err(_) => return Ok(false),
    };
    let mut record = match account_from_auth_info("", &info) {
        Ok(record) => record,
        Err(_) => return Ok(false),
    };
    let record_key = record.account_key.clone();
    let snapshot_path = account_auth_path(paths, &record_key);
    ensure_accounts_dir(paths)?;

    if let Some(existing) = registry
        .accounts
        .iter_mut()
        .find(|existing| existing.account_key == record_key)
    {
        let mut changed = registry.active_account_key.as_deref() != Some(record_key.as_str());
        if existing.email != record.email {
            existing.email = record.email.clone();
            changed = true;
        }
        if existing.plan != record.plan {
            existing.plan = record.plan;
            changed = true;
        }
        if existing.auth_mode != record.auth_mode {
            existing.auth_mode = record.auth_mode;
            changed = true;
        }
        if record.account_name.is_some() && existing.account_name != record.account_name {
            existing.account_name = record.account_name.take();
            changed = true;
        }
        if !file_equals_bytes(&snapshot_path, &auth_bytes)? {
            write_managed_file(&snapshot_path, &auth_bytes)?;
            changed = true;
        } else {
            harden_sensitive_file(&snapshot_path)?;
        }
        set_active_account_key(registry, &record_key);
        return Ok(changed);
    }

    write_managed_file(&snapshot_path, &auth_bytes)?;
    upsert_account(registry, record)?;
    set_active_account_key(registry, &record_key);
    Ok(true)
}

pub fn activate_account_by_key(
    paths: &Paths,
    registry: &mut Registry,
    account_key: &str,
) -> Result<()> {
    let source = account_auth_path(paths, account_key);
    if !source.exists() {
        bail!("account snapshot not found for `{account_key}`");
    }
    let target = active_auth_path(paths);
    ensure_accounts_dir(paths)?;
    backup_auth_if_changed(paths, &target, &source)?;
    replace_file_preserving_permissions(&source, &target)
        .with_context(|| format!("failed switching active auth to {}", source.display()))?;
    set_active_account_key(registry, account_key);
    let now = now_ms();
    registry.active_account_activated_at_ms = Some(now);
    if let Some(record) = registry
        .accounts
        .iter_mut()
        .find(|record| record.account_key == account_key)
    {
        record.last_used_at = Some(now / 1000);
    }
    Ok(())
}

pub fn set_active_account_key(registry: &mut Registry, account_key: &str) {
    if registry.active_account_key.as_deref() == Some(account_key) {
        return;
    }
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

pub fn account_from_auth_info(alias: &str, auth: &AuthInfo) -> Result<AccountRecord> {
    match auth.auth_mode {
        AuthMode::Chatgpt => account_from_auth(alias, auth),
        AuthMode::Apikey => {
            let api_key = auth
                .openai_api_key
                .as_deref()
                .ok_or_else(|| anyhow!("auth is missing OPENAI_API_KEY"))?;
            let me = chatgpt_api::fetch_me_for_api_key(api_key)?;
            account_from_api_key_me(alias, auth, &me)
        }
    }
}

pub fn account_from_api_key_me(
    alias: &str,
    auth: &AuthInfo,
    me: &MeFetchResult,
) -> Result<AccountRecord> {
    let api_key = auth
        .openai_api_key
        .as_deref()
        .ok_or_else(|| anyhow!("auth is missing OPENAI_API_KEY"))?;
    let record_key = api_key_account_key(&me.user_id, api_key);
    Ok(AccountRecord {
        account_key: record_key,
        chatgpt_account_id: String::new(),
        chatgpt_user_id: me.user_id.clone(),
        email: me.email.to_ascii_lowercase(),
        alias: alias.to_owned(),
        account_name: Some(api_key_account_name(api_key)),
        plan: None,
        auth_mode: Some(AuthMode::Apikey),
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
        let account_name = existing.account_name.clone();
        *existing = record;
        if existing.account_name.is_none() {
            existing.account_name = account_name;
        }
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
    let root = path.map(PathBuf::from).unwrap_or_else(default_cpa_root);
    import_path_impl(paths, registry, &root, alias, true)
}

pub fn purge_registry_from_path(
    paths: &Paths,
    source: Option<&Path>,
    alias: Option<&str>,
) -> Result<ImportReport> {
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
                let mut record = match account_from_auth_info(alias.unwrap_or(""), &info) {
                    Ok(record) => record,
                    Err(_) => {
                        report.push(file.display().to_string(), ImportOutcome::Skipped);
                        continue;
                    }
                };
                let label = record.email.clone();
                let snapshot_path = account_auth_path(paths, &record.account_key);
                if snapshot_path != file {
                    ensure_accounts_dir(paths)?;
                    let _ = copy_managed_file(&file, &snapshot_path);
                }
                record.alias = alias.unwrap_or("").to_owned();
                let _ = upsert_account(&mut registry, record)?;
                report.push(label, ImportOutcome::Imported);
            }
            Err(_) => report.push(file.display().to_string(), ImportOutcome::Skipped),
        }
    }

    if report.applied_count() > 0 {
        if let Some(active) = load_active_auth_info(paths)? {
            if let Ok(record) = account_from_auth_info("", &active) {
                registry.active_account_key = Some(record.account_key);
                registry.active_account_activated_at_ms = Some(0);
            }
        }
        save_registry(paths, &registry)?;
    }

    Ok(report)
}

pub fn remove_accounts(
    paths: &Paths,
    registry: &mut Registry,
    account_keys: &[String],
) -> Result<()> {
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
        let tracked = registry.accounts.iter().any(|record| {
            account_auth_path(paths, &record.account_key).file_name() == path.file_name()
        });
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
    if let Some(record) = registry
        .accounts
        .iter_mut()
        .find(|record| record.account_key == account_key)
    {
        let changed = record.last_usage.as_ref() != Some(&usage)
            || record.last_usage_at != Some(last_usage_at_ms / 1000)
            || record.last_local_rollout.as_ref().is_none_or(|rollout| {
                rollout.path != rollout_path || rollout.event_timestamp_ms != event_timestamp_ms
            });
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

pub fn apply_account_names_for_user(
    registry: &mut Registry,
    chatgpt_user_id: &str,
    entries: &[(String, Option<String>)],
) -> bool {
    let mut changed = false;
    for record in &mut registry.accounts {
        if record.chatgpt_user_id != chatgpt_user_id {
            continue;
        }
        let next_name = entries
            .iter()
            .find(|(account_id, _)| account_id == &record.chatgpt_account_id)
            .and_then(|(_, name)| name.clone());
        if record.account_name != next_name {
            record.account_name = next_name;
            changed = true;
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
                let mut record = match account_from_auth_info(alias.unwrap_or(""), &info) {
                    Ok(record) => record,
                    Err(_) => {
                        report.push(file.display().to_string(), ImportOutcome::Skipped);
                        continue;
                    }
                };
                if record.account_key.is_empty() {
                    report.push(file.display().to_string(), ImportOutcome::Skipped);
                    continue;
                }
                let label = record.email.clone();
                let snapshot_path = account_auth_path(paths, &record.account_key);
                write_managed_file(&snapshot_path, &prepared)
                    .with_context(|| format!("failed writing {}", snapshot_path.display()))?;
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

fn api_key_account_key(user_id: &str, api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    format!("apikey::{user_id}::{}", hex_lower(&digest))
}

fn api_key_account_name(api_key: &str) -> String {
    let digest = Sha256::digest(api_key.as_bytes());
    let hex = hex_lower(&digest);
    format!("sk-{}***{}", &hex[..5], &hex[hex.len() - 4..])
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
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
        "prolite" => PlanType::Prolite,
        "pro" => PlanType::Pro,
        "team" => PlanType::Team,
        "business" => PlanType::Business,
        "enterprise" => PlanType::Enterprise,
        "edu" => PlanType::Edu,
        _ => PlanType::Unknown,
    }
}
