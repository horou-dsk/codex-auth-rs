use crate::auth::parse_auth_info;
use crate::cli::{
    Command, ConfigApiArgs, ConfigAutoArgs, ConfigCommand, ConfigToggle, DaemonArgs, ImportArgs,
    LoginArgs, RemoveArgs, SwitchArgs,
};
use crate::model::{AccountRecord, RateLimitSnapshot, Registry};
use crate::registry::{
    ImportOutcome, Paths, account_auth_path, account_from_auth, activate_account_by_key,
    active_auth_path, clean_accounts_dir, find_matching_accounts, import_cpa_path,
    import_standard_path, load_registry, purge_registry_from_path, remove_accounts, resolve_paths,
    save_registry, select_best_account_key_by_usage, set_active_account_key,
    sync_active_account_from_auth, update_account_usage, update_plan_from_usage, upsert_account,
};
use crate::sessions::scan_latest_usage;
use anyhow::{Context, Result, anyhow, bail};
use dialoguer::{Confirm, MultiSelect, Select, theme::ColorfulTheme};
use std::process::Command as ProcessCommand;
use std::thread;
use std::time::Duration;

pub fn run(command: Command) -> Result<()> {
    let paths = resolve_paths()?;
    match command {
        Command::List => list_accounts(&paths),
        Command::Login(args) => login(&paths, args),
        Command::Import(args) => import_accounts(&paths, args),
        Command::Switch(args) => switch_account(&paths, args),
        Command::Remove(args) => remove_account(&paths, args),
        Command::Status => status(&paths),
        Command::Clean => clean(&paths),
        Command::Daemon(args) => daemon(&paths, args),
        Command::Config(args) => config(&paths, args.section),
    }
}

fn list_accounts(paths: &Paths) -> Result<()> {
    let mut registry = load_registry(paths)?;
    let mut dirty = sync_active_account_from_auth(paths, &mut registry)?;
    dirty |= refresh_active_usage(paths, &mut registry)?;
    dirty |= update_plan_from_usage(&mut registry);
    if dirty {
        save_registry(paths, &registry)?;
    }

    if registry.accounts.is_empty() {
        println!("no accounts");
        return Ok(());
    }

    println!(
        "{:<2} {:<32} {:<12} {:<12} {:<18} {:<8}",
        "", "ACCOUNT", "ALIAS", "PLAN", "LAST", "ACTIVE"
    );
    for record in &registry.accounts {
        println!(
            "{:<2} {:<32} {:<12} {:<12} {:<18} {:<8}",
            if registry.active_account_key.as_deref() == Some(record.account_key.as_str()) {
                "*"
            } else {
                ""
            },
            display_account(record),
            truncate(&record.alias, 12),
            record
                .plan
                .map(|plan| plan.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            format_last_seen(record.last_usage_at),
            if registry.active_account_key.as_deref() == Some(record.account_key.as_str()) {
                "yes"
            } else {
                ""
            },
        );
    }
    Ok(())
}

fn login(paths: &Paths, args: LoginArgs) -> Result<()> {
    let mut command = ProcessCommand::new("codex");
    command.arg("login");
    if args.device_auth {
        command.arg("--device-auth");
    }
    let status = command.status().context("failed to launch `codex login`")?;
    if !status.success() {
        bail!("`codex login` failed");
    }

    let auth_path = active_auth_path(paths);
    let info = parse_auth_info(&auth_path)?;
    let mut registry = load_registry(paths)?;
    let Some(record_key) = info.record_key.as_ref() else {
        bail!("active auth is missing record key");
    };
    std::fs::copy(&auth_path, account_auth_path(paths, record_key))
        .with_context(|| format!("failed storing snapshot for `{record_key}`"))?;
    let _ = upsert_account(&mut registry, account_from_auth("", &info)?)?;
    set_active_account_key(&mut registry, record_key);
    save_registry(paths, &registry)?;
    println!("added {}", info.email.unwrap_or_else(|| record_key.clone()));
    Ok(())
}

fn import_accounts(paths: &Paths, args: ImportArgs) -> Result<()> {
    if args.purge && args.cpa {
        bail!("`--purge` cannot be combined with `--cpa`");
    }

    if args.purge {
        let report = purge_registry_from_path(paths, args.path.as_deref(), args.alias.as_deref())?;
        print_import_report(&report);
        return Ok(());
    }

    let mut registry = load_registry(paths)?;
    let report = if args.cpa {
        import_cpa_path(paths, &mut registry, args.path.as_deref(), args.alias.as_deref())?
    } else {
        let path = args
            .path
            .as_deref()
            .ok_or_else(|| anyhow!("`import` requires a path unless `--cpa` is used"))?;
        import_standard_path(paths, &mut registry, path, args.alias.as_deref())?
    };
    if report.applied_count() > 0 {
        save_registry(paths, &registry)?;
    }
    print_import_report(&report);
    Ok(())
}

fn switch_account(paths: &Paths, args: SwitchArgs) -> Result<()> {
    let mut registry = load_registry(paths)?;
    if sync_active_account_from_auth(paths, &mut registry)? {
        save_registry(paths, &registry)?;
    }
    refresh_active_usage(paths, &mut registry)?;

    let selected_key = if let Some(query) = args.query.as_deref() {
        let matches = find_matching_accounts(&registry, query);
        match matches.len() {
            0 => bail!("account not found: {query}"),
            1 => matches[0].account_key.clone(),
            _ => select_from_matches(&matches)?,
        }
    } else {
        select_from_matches(&registry.accounts.iter().collect::<Vec<_>>())?
    };

    activate_account_by_key(paths, &mut registry, &selected_key)?;
    save_registry(paths, &registry)?;
    println!("switched to {selected_key}");
    Ok(())
}

fn remove_account(paths: &Paths, args: RemoveArgs) -> Result<()> {
    let mut registry = load_registry(paths)?;
    if registry.accounts.is_empty() {
        println!("no accounts");
        return Ok(());
    }

    let keys = if args.all {
        registry
            .accounts
            .iter()
            .map(|record| record.account_key.clone())
            .collect::<Vec<_>>()
    } else if let Some(query) = args.query.as_deref() {
        let matches = find_matching_accounts(&registry, query);
        if matches.is_empty() {
            bail!("account not found: {query}");
        }
        if matches.len() > 1 {
            let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("remove {} matched accounts?", matches.len()))
                .default(false)
                .interact()?;
            if !confirmed {
                return Ok(());
            }
        }
        matches.into_iter().map(|record| record.account_key.clone()).collect()
    } else {
        let labels = registry.accounts.iter().map(display_account).collect::<Vec<_>>();
        let selection = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("select accounts to remove")
            .items(&labels)
            .interact()?;
        selection
            .into_iter()
            .map(|index| registry.accounts[index].account_key.clone())
            .collect()
    };

    if keys.is_empty() {
        return Ok(());
    }

    remove_accounts(paths, &mut registry, &keys)?;
    save_registry(paths, &registry)?;
    println!("removed {}", keys.join(", "));
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let mut registry = load_registry(paths)?;
    let mut dirty = sync_active_account_from_auth(paths, &mut registry)?;
    dirty |= refresh_active_usage(paths, &mut registry)?;
    if dirty {
        save_registry(paths, &registry)?;
    }

    println!(
        "auto_switch: {} (5h={}%, weekly={}%)",
        if registry.auto_switch.enabled { "enabled" } else { "disabled" },
        registry.auto_switch.threshold_5h_percent,
        registry.auto_switch.threshold_weekly_percent
    );
    println!(
        "api: usage={}, account={}",
        registry.api.usage, registry.api.account
    );
    if let Some(active) = registry.active_account() {
        println!("active: {}", display_account(active));
        println!("last_usage: {}", format_usage_summary(active.last_usage.as_ref()));
    } else {
        println!("active: none");
    }
    Ok(())
}

fn clean(paths: &Paths) -> Result<()> {
    let registry = load_registry(paths)?;
    let (auth_backups, registry_backups, stale_entries) = clean_accounts_dir(paths, &registry)?;
    println!(
        "cleaned accounts: auth_backups={auth_backups}, registry_backups={registry_backups}, stale_entries={stale_entries}"
    );
    Ok(())
}

fn daemon(paths: &Paths, args: DaemonArgs) -> Result<()> {
    if args.once {
        let mut registry = load_registry(paths)?;
        daemon_once(paths, &mut registry)?;
        save_registry(paths, &registry)?;
        return Ok(());
    }

    if !args.watch {
        bail!("`daemon` requires `--watch` or `--once`");
    }

    loop {
        let mut registry = load_registry(paths)?;
        let _ = daemon_once(paths, &mut registry);
        let _ = save_registry(paths, &registry);
        thread::sleep(Duration::from_secs(30));
    }
}

fn config(paths: &Paths, command: ConfigCommand) -> Result<()> {
    let mut registry = load_registry(paths)?;
    match command {
        ConfigCommand::Auto(args) => config_auto(&mut registry, args)?,
        ConfigCommand::Api(args) => config_api(&mut registry, args),
    }
    save_registry(paths, &registry)?;
    Ok(())
}

fn config_auto(registry: &mut Registry, args: ConfigAutoArgs) -> Result<()> {
    if let Some(action) = args.action {
        registry.auto_switch.enabled = matches!(action, ConfigToggle::Enable);
    }
    if let Some(value) = args.threshold_5h_percent {
        validate_percent(value, "--5h")?;
        registry.auto_switch.threshold_5h_percent = value;
    }
    if let Some(value) = args.weekly {
        validate_percent(value, "--weekly")?;
        registry.auto_switch.threshold_weekly_percent = value;
    }
    println!(
        "auto_switch: {} (5h={}%, weekly={}%)",
        if registry.auto_switch.enabled { "enabled" } else { "disabled" },
        registry.auto_switch.threshold_5h_percent,
        registry.auto_switch.threshold_weekly_percent
    );
    Ok(())
}

fn config_api(registry: &mut Registry, args: ConfigApiArgs) {
    let enabled = matches!(args.action, ConfigToggle::Enable);
    registry.api.usage = enabled;
    registry.api.account = enabled;
    println!("api: usage={}, account={}", registry.api.usage, registry.api.account);
}

fn daemon_once(paths: &Paths, registry: &mut Registry) -> Result<()> {
    let _ = sync_active_account_from_auth(paths, registry)?;
    let _ = refresh_active_usage(paths, registry)?;
    if !registry.auto_switch.enabled {
        return Ok(());
    }
    let Some(active) = registry.active_account() else {
        return Ok(());
    };

    let primary_remaining = remaining_percent(active.last_usage.as_ref().and_then(|usage| usage.primary.as_ref()));
    let weekly_remaining = remaining_percent(active.last_usage.as_ref().and_then(|usage| usage.secondary.as_ref()));
    let should_switch = primary_remaining.is_some_and(|value| value < registry.auto_switch.threshold_5h_percent)
        || weekly_remaining.is_some_and(|value| value < registry.auto_switch.threshold_weekly_percent);
    if !should_switch {
        return Ok(());
    }

    let Some(next_key) = select_best_account_key_by_usage(registry) else {
        return Ok(());
    };
    if registry.active_account_key.as_deref() == Some(next_key.as_str()) {
        return Ok(());
    }
    activate_account_by_key(paths, registry, &next_key)?;
    println!("auto-switched to {next_key}");
    Ok(())
}

fn refresh_active_usage(paths: &Paths, registry: &mut Registry) -> Result<bool> {
    let Some(active_key) = registry.active_account_key.clone() else {
        return Ok(false);
    };
    let Some(latest) = scan_latest_usage(&paths.codex_home)? else {
        return Ok(false);
    };

    let already_applied = registry
        .accounts
        .iter()
        .find(|record| record.account_key == active_key)
        .and_then(|record| record.last_local_rollout.as_ref())
        .is_some_and(|rollout| {
            rollout.event_timestamp_ms == latest.event_timestamp_ms
                && rollout.path == latest.path.display().to_string()
        });
    if already_applied {
        return Ok(false);
    }

    let changed = update_account_usage(
        registry,
        &active_key,
        latest.snapshot,
        latest.event_timestamp_ms,
        latest.path.display().to_string(),
        latest.event_timestamp_ms,
    );
    Ok(changed)
}

fn select_from_matches(matches: &[&AccountRecord]) -> Result<String> {
    let labels = matches.iter().map(|record| display_account(record)).collect::<Vec<_>>();
    let index = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("select account")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(matches[index].account_key.clone())
}

fn print_import_report(report: &crate::registry::ImportReport) {
    for item in &report.items {
        let status = match item.outcome {
            ImportOutcome::Imported => "imported",
            ImportOutcome::Updated => "updated",
            ImportOutcome::Skipped => "skipped",
        };
        println!("  {status:<8} {}", item.label);
    }
    println!(
        "Import Summary: {} imported, {} updated, {} skipped (total {} files)",
        report.imported,
        report.updated,
        report.skipped,
        report.items.len()
    );
}

fn display_account(record: &AccountRecord) -> String {
    if record.alias.is_empty() {
        truncate(&record.email, 32)
    } else {
        truncate(&format!("{} ({})", record.email, record.alias), 32)
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    value.chars().take(max.saturating_sub(1)).collect::<String>() + "."
}

fn format_last_seen(value: Option<i64>) -> String {
    match value {
        Some(value) => chrono::DateTime::from_timestamp(value, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_owned()),
        None => "-".to_owned(),
    }
}

fn format_usage_summary(usage: Option<&RateLimitSnapshot>) -> String {
    let Some(usage) = usage else {
        return "-".to_owned();
    };
    let primary = remaining_percent(usage.primary.as_ref())
        .map(|value| format!("5h={value}%"))
        .unwrap_or_else(|| "5h=-".to_owned());
    let weekly = remaining_percent(usage.secondary.as_ref())
        .map(|value| format!("weekly={value}%"))
        .unwrap_or_else(|| "weekly=-".to_owned());
    format!("{primary}, {weekly}")
}

fn remaining_percent(window: Option<&crate::model::RateLimitWindow>) -> Option<u8> {
    window.map(|window| (100.0 - window.used_percent).clamp(0.0, 100.0).round() as u8)
}

fn validate_percent(value: u8, flag: &str) -> Result<()> {
    if (1..=100).contains(&value) {
        Ok(())
    } else {
        bail!("{flag} must be between 1 and 100")
    }
}
