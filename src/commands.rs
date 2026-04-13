use crate::auth::parse_auth_info;
use crate::chatgpt_api::{self, DEFAULT_ACCOUNT_ENDPOINT};
use crate::cli::{
    Command, ConfigApiArgs, ConfigAutoArgs, ConfigCommand, ConfigToggle, DaemonArgs, ImportArgs,
    LoginArgs, RemoveArgs, SwitchArgs,
};
use crate::display::{
    build_display_rows, display_plan, format_last_activity, format_rate_limit_ui, resolve_rate_window,
};
use crate::model::{AccountRecord, RateLimitSnapshot, Registry};
use crate::registry::{
    ImportOutcome, Paths, account_auth_path, account_from_auth, activate_account_by_key, active_auth_path,
    apply_account_names_for_user, clean_accounts_dir, find_matching_accounts, import_cpa_path,
    import_standard_path, load_active_auth_info, load_registry, purge_registry_from_path, remove_accounts,
    resolve_paths, save_registry, select_best_account_key_by_usage, set_active_account_key,
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
    dirty |= refresh_active_account_names(paths, &mut registry)?;
    dirty |= update_plan_from_usage(&mut registry);
    if dirty {
        save_registry(paths, &registry)?;
    }

    if registry.accounts.is_empty() {
        println!("no accounts");
        return Ok(());
    }

    render_accounts_table(&registry);
    Ok(())
}

fn login(paths: &Paths, args: LoginArgs) -> Result<()> {
    let mut command = build_codex_login_command(args.device_auth);
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

fn build_codex_login_command(device_auth: bool) -> ProcessCommand {
    #[cfg(target_os = "windows")]
    {
        let mut command = ProcessCommand::new("powershell.exe");
        command.arg("-NoLogo");
        command.arg("-NoProfile");
        command.arg("-ExecutionPolicy");
        command.arg("Bypass");
        command.arg("-Command");
        command.arg(if device_auth {
            "codex login --device-auth"
        } else {
            "codex login"
        });
        command
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut command = ProcessCommand::new("codex");
        command.arg("login");
        if device_auth {
            command.arg("--device-auth");
        }
        command
    }
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
    dirty |= refresh_active_account_names(paths, &mut registry)?;
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
    println!(
        "runtime: {}",
        if registry.auto_switch.enabled {
            "watch/daemon expected"
        } else {
            "stopped"
        }
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
    let _ = refresh_active_account_names(paths, registry)?;
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
    let latest = if registry.api.usage {
        match chatgpt_api::fetch_usage_for_auth_path(&active_auth_path(paths)) {
            Ok(result) => result.snapshot.map(|snapshot| (snapshot, "api".to_owned(), now_ms())),
            Err(_) => None,
        }
    } else {
        None
    };

    let (snapshot, source_path, event_timestamp_ms) = if let Some(result) = latest {
        result
    } else {
        let Some(latest) = scan_latest_usage(&paths.codex_home)? else {
            return Ok(false);
        };
        (
            latest.snapshot,
            latest.path.display().to_string(),
            latest.event_timestamp_ms,
        )
    };

    let already_applied = registry
        .accounts
        .iter()
        .find(|record| record.account_key == active_key)
        .and_then(|record| record.last_local_rollout.as_ref())
        .is_some_and(|rollout| {
            rollout.event_timestamp_ms == event_timestamp_ms && rollout.path == source_path
        });
    if already_applied {
        return Ok(false);
    }

    let changed = update_account_usage(
        registry,
        &active_key,
        snapshot,
        event_timestamp_ms,
        source_path,
        event_timestamp_ms,
    );
    Ok(changed)
}

fn refresh_active_account_names(paths: &Paths, registry: &mut Registry) -> Result<bool> {
    if !registry.api.account {
        return Ok(false);
    }
    let Some(info) = load_active_auth_info(paths)? else {
        return Ok(false);
    };
    let (Some(access_token), Some(account_id), Some(user_id)) = (
        info.access_token.as_deref(),
        info.chatgpt_account_id.as_deref(),
        info.chatgpt_user_id.as_deref(),
    ) else {
        return Ok(false);
    };

    let result = match chatgpt_api::fetch_accounts_for_token(DEFAULT_ACCOUNT_ENDPOINT, access_token, account_id) {
        Ok(result) => result,
        Err(_) => return Ok(false),
    };
    let Some(entries) = result.entries else {
        return Ok(false);
    };
    let mapped = entries
        .into_iter()
        .map(|entry| (entry.account_id, entry.account_name))
        .collect::<Vec<_>>();
    Ok(apply_account_names_for_user(registry, user_id, &mapped))
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

fn render_accounts_table(registry: &Registry) {
    let rows = build_display_rows(registry, None);
    println!(
        "{:<5} {:<32} {:<12} {:<18} {:<18} {:<14}",
        "", "ACCOUNT", "PLAN", "5H USAGE", "WEEKLY USAGE", "LAST ACTIVITY"
    );
    for (row_index, row) in rows.iter().enumerate() {
        if let Some(account_index) = row.account_index {
            let record = &registry.accounts[account_index];
            let indent = "  ".repeat(row.depth as usize);
            println!(
                "{:<5} {:<32} {:<12} {:<18} {:<18} {:<14}",
                if row.is_active {
                    format!("*{:02}", row_index + 1)
                } else {
                    format!("{:02}", row_index + 1)
                },
                truncate(&(indent + &row.account_cell), 32),
                display_plan(record),
                truncate(&format_rate_limit_ui(resolve_rate_window(record.last_usage.as_ref(), 300, true)), 18),
                truncate(&format_rate_limit_ui(resolve_rate_window(record.last_usage.as_ref(), 10080, false)), 18),
                format_last_activity(record.last_usage_at),
            );
        } else {
            println!("{:<5} {}", "", row.account_cell);
        }
    }
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
