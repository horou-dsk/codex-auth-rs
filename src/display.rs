use crate::model::{AccountRecord, RateLimitSnapshot, RateLimitWindow, Registry};
use chrono::{Local, TimeZone};

#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub account_index: Option<usize>,
    pub account_cell: String,
    pub depth: u8,
    pub is_active: bool,
}

pub fn build_display_rows(registry: &Registry, account_indices: Option<&[usize]>) -> Vec<DisplayRow> {
    let mut ordered = match account_indices {
        Some(indices) => indices.to_vec(),
        None => (0..registry.accounts.len()).collect(),
    };
    ordered.sort_by(|lhs, rhs| compare_display_order(registry, *lhs, *rhs));

    let mut rows = Vec::new();
    let mut i = 0;
    while i < ordered.len() {
        let group_start = i;
        let email = registry.accounts[ordered[i]].email.clone();
        while i < ordered.len() && registry.accounts[ordered[i]].email == email {
            i += 1;
        }
        let group = &ordered[group_start..i];
        if group.len() == 1 {
            let idx = group[0];
            rows.push(DisplayRow {
                account_index: Some(idx),
                account_cell: registry.accounts[idx].email.clone(),
                depth: 0,
                is_active: is_active(registry, idx),
            });
            continue;
        }

        rows.push(DisplayRow {
            account_index: None,
            account_cell: email,
            depth: 0,
            is_active: false,
        });

        for idx in group.iter().copied() {
            rows.push(DisplayRow {
                account_index: Some(idx),
                account_cell: grouped_account_cell(registry, group, idx),
                depth: 1,
                is_active: is_active(registry, idx),
            });
        }
    }

    rows
}

pub fn format_rate_limit_ui(window: Option<&RateLimitWindow>) -> String {
    let Some(window) = window else {
        return "-".to_owned();
    };
    let Some(reset_at) = window.resets_at else {
        return "-".to_owned();
    };
    let remaining = remaining_percent(window.used_percent);
    let now = Local::now().timestamp();
    if now >= reset_at {
        return "100%".to_owned();
    }
    let reset = match Local.timestamp_opt(reset_at, 0).single() {
        Some(value) => value,
        None => return format!("{remaining}%"),
    };
    if reset.date_naive() == Local::now().date_naive() {
        return format!("{remaining}% ({})", reset.format("%H:%M"));
    }
    format!("{remaining}% ({} on {})", reset.format("%H:%M"), reset.format("%-d %b"))
}

pub fn format_last_activity(last_usage_at: Option<i64>) -> String {
    let Some(last_usage_at) = last_usage_at else {
        return "-".to_owned();
    };
    let now = Local::now().timestamp();
    let delta = now.saturating_sub(last_usage_at);
    if delta < 60 {
        "just now".to_owned()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

pub fn resolve_rate_window(usage: Option<&RateLimitSnapshot>, minutes: i64, fallback_primary: bool) -> Option<&RateLimitWindow> {
    let usage = usage?;
    if let Some(window) = usage.primary.as_ref() {
        if window.window_minutes == Some(minutes) {
            return Some(window);
        }
    }
    if let Some(window) = usage.secondary.as_ref() {
        if window.window_minutes == Some(minutes) {
            return Some(window);
        }
    }
    if fallback_primary {
        usage.primary.as_ref()
    } else {
        usage.secondary.as_ref()
    }
}

pub fn display_plan(record: &AccountRecord) -> String {
    record.plan.map(|plan| plan.to_string()).unwrap_or_else(|| "-".to_owned())
}

fn compare_display_order(registry: &Registry, lhs: usize, rhs: usize) -> std::cmp::Ordering {
    let a = &registry.accounts[lhs];
    let b = &registry.accounts[rhs];
    a.email
        .cmp(&b.email)
        .then_with(|| is_active(registry, rhs).cmp(&is_active(registry, lhs)))
        .then_with(|| plan_sort_rank(a).cmp(&plan_sort_rank(b)))
        .then_with(|| display_plan(a).cmp(&display_plan(b)))
        .then_with(|| a.account_key.cmp(&b.account_key))
}

fn plan_sort_rank(record: &AccountRecord) -> u8 {
    match record.plan {
        Some(crate::model::PlanType::Team)
        | Some(crate::model::PlanType::Business)
        | Some(crate::model::PlanType::Enterprise)
        | Some(crate::model::PlanType::Edu) => 0,
        Some(crate::model::PlanType::Free)
        | Some(crate::model::PlanType::Plus)
        | Some(crate::model::PlanType::Pro) => 1,
        _ => 2,
    }
}

fn grouped_account_cell(registry: &Registry, group: &[usize], account_idx: usize) -> String {
    let record = &registry.accounts[account_idx];
    let fallback = {
        let base = display_plan(record);
        let same = group
            .iter()
            .filter(|&&idx| registry.accounts[idx].alias.is_empty() && display_plan(&registry.accounts[idx]) == base)
            .copied()
            .collect::<Vec<_>>();
        if same.len() <= 1 {
            base
        } else {
            let ordinal = same
                .iter()
                .filter(|&&idx| registry.accounts[idx].account_key < record.account_key)
                .count()
                + 1;
            format!("{base} #{ordinal}")
        }
    };
    build_preferred_account_label(record, &fallback)
}

fn build_preferred_account_label(record: &AccountRecord, fallback: &str) -> String {
    match (
        (!record.alias.is_empty()).then_some(record.alias.as_str()),
        record.account_name.as_deref().filter(|name| !name.is_empty()),
    ) {
        (Some(alias), Some(name)) => format!("{alias} ({name})"),
        (Some(alias), None) => alias.to_owned(),
        (None, Some(name)) => name.to_owned(),
        (None, None) => fallback.to_owned(),
    }
}

fn is_active(registry: &Registry, account_idx: usize) -> bool {
    registry
        .active_account_key
        .as_deref()
        .is_some_and(|active| active == registry.accounts[account_idx].account_key)
}

fn remaining_percent(used_percent: f64) -> i64 {
    (100.0 - used_percent).clamp(0.0, 100.0).round() as i64
}
