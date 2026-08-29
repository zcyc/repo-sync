use serde::{Deserialize, Serialize};
use std::{collections::HashSet, error::Error, path::Path};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub source: String,
    pub target: Vec<String>,
    pub workspace: String,
    pub mode: SyncMode,
    pub crontab: Option<String>,
    pub branches: Vec<String>,
    pub include_refs: Vec<String>,
    pub exclude_refs: Vec<String>,
    pub timeout_secs: u64,
    pub dry_run: bool,
    pub allow_destructive: bool,
    pub sync_lfs: bool,
    pub divergence: DivergencePolicy,
    pub tag_policy: TagPolicy,
    pub prune_branches: bool,
    pub prune_tags: bool,
    pub atomic: bool,
    pub max_retries: u32,
    pub retry_backoff_secs: u64,
    pub failure_cooldown_secs: u64,
    pub webhook_secret_envs: Vec<String>,
    pub webhook_max_pending_events: u64,
    pub webhook_event_lease_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Branch,
    Mirror,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DivergencePolicy {
    Fail,
    Keep,
    Force,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TagPolicy {
    Preserve,
    Fail,
    Force,
}

pub fn validate(config: &[Item]) -> Result<(), Box<dyn Error>> {
    if config.is_empty() {
        return Err("configuration must contain at least one item".into());
    }

    let mut workspaces = HashSet::new();
    for item in config {
        validate_item(item)?;
        if !workspaces.insert(&item.workspace) {
            return Err(format!("workspace is duplicated: {}", item.workspace).into());
        }
    }
    Ok(())
}

pub fn validate_item(item: &Item) -> Result<(), Box<dyn Error>> {
    if item.source.trim().is_empty() {
        return Err("source cannot be empty".into());
    }
    if item.target.is_empty() {
        return Err("at least one target is required".into());
    }
    let mut targets = HashSet::new();
    for (index, target) in item.target.iter().enumerate() {
        if target.trim().is_empty() {
            return Err(format!("target {index} cannot be empty").into());
        }
        if !targets.insert(target) {
            return Err(format!("target {index} is duplicated").into());
        }
    }

    let workspace = Path::new(&item.workspace);
    if item.workspace.trim().is_empty()
        || workspace == Path::new(".")
        || workspace == Path::new("..")
    {
        return Err("workspace must be a non-current directory".into());
    }
    if item.timeout_secs == 0 {
        return Err("timeout_secs must be greater than zero".into());
    }
    if item.max_retries > 10 {
        return Err("max_retries cannot be greater than 10".into());
    }
    if item.webhook_max_pending_events == 0 {
        return Err("webhook_max_pending_events must be greater than zero".into());
    }
    if item.webhook_event_lease_secs == 0 {
        return Err("webhook_event_lease_secs must be greater than zero".into());
    }
    if item
        .webhook_secret_envs
        .iter()
        .any(|name| name.trim().is_empty())
    {
        return Err("webhook_secret_envs cannot contain an empty name".into());
    }
    if item.branches.iter().any(|branch| branch.trim().is_empty()) {
        return Err("branches cannot contain empty patterns".into());
    }
    for (name, patterns) in [
        ("include_refs", &item.include_refs),
        ("exclude_refs", &item.exclude_refs),
    ] {
        if patterns.iter().any(|pattern| pattern.trim().is_empty()) {
            return Err(format!("{name} cannot contain empty patterns").into());
        }
        if patterns.iter().any(|pattern| !pattern.starts_with("refs/")) {
            return Err(format!("{name} patterns must start with refs/").into());
        }
    }
    for value in [&item.source].into_iter().chain(item.target.iter()) {
        validate_repository_url(value)?;
    }

    if matches!(item.mode, SyncMode::Mirror) && !item.branches.is_empty() {
        return Err("mirror mode cannot set branches".into());
    }
    if matches!(item.mode, SyncMode::Mirror) && item.tag_policy != TagPolicy::Force {
        return Err("mirror mode requires tag_policy=force".into());
    }
    if matches!(item.mode, SyncMode::Mirror) && (item.prune_branches || item.prune_tags) {
        return Err("mirror mode already prunes refs; remove prune options".into());
    }

    let destructive = matches!(item.mode, SyncMode::Mirror)
        || item.prune_branches
        || item.prune_tags
        || item.tag_policy == TagPolicy::Force;
    if !item.dry_run && destructive && !item.allow_destructive {
        return Err("destructive sync requires allow_destructive=true".into());
    }
    if item.allow_destructive && !destructive {
        return Err("allow_destructive has no enabled destructive operation".into());
    }
    Ok(())
}

fn validate_repository_url(value: &str) -> Result<(), Box<dyn Error>> {
    let Some((scheme, rest)) = value.split_once("://") else {
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    if matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") && authority.contains('@') {
        return Err(
            "repository URLs must not contain credentials; use SSH agent or Git credential helper"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn branch_selected(patterns: &[String], branch: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|pattern| glob_matches(pattern, branch))
}

pub(crate) fn ref_selected(
    include_patterns: &[String],
    exclude_patterns: &[String],
    reference: &str,
) -> bool {
    (include_patterns.is_empty()
        || include_patterns
            .iter()
            .any(|pattern| glob_matches(pattern, reference)))
        && !exclude_patterns
            .iter()
            .any(|pattern| glob_matches(pattern, reference))
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut pattern_index = 0;
    let mut value_index = 0;
    let mut star_index = None;
    let mut star_value_index = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::{
        branch_selected, ref_selected, validate, DivergencePolicy, Item, SyncMode, TagPolicy,
    };

    fn item() -> Item {
        Item {
            source: "source".into(),
            target: vec!["target".into()],
            workspace: "./work".into(),
            mode: SyncMode::Branch,
            crontab: None,
            branches: vec!["main".into()],
            include_refs: Vec::new(),
            exclude_refs: Vec::new(),
            timeout_secs: 300,
            dry_run: false,
            allow_destructive: false,
            sync_lfs: false,
            divergence: DivergencePolicy::Fail,
            tag_policy: TagPolicy::Preserve,
            prune_branches: false,
            prune_tags: false,
            atomic: true,
            max_retries: 3,
            retry_backoff_secs: 5,
            failure_cooldown_secs: 60,
            webhook_secret_envs: Vec::new(),
            webhook_max_pending_events: 10_000,
            webhook_event_lease_secs: 900,
        }
    }

    #[test]
    fn matches_branch_globs() {
        assert!(branch_selected(&["release/*".into()], "release/2026.08"));
        assert!(!branch_selected(&["main".into()], "develop"));
    }

    #[test]
    fn include_refs_override_excluded_refs() {
        let include = vec!["refs/heads/*".into(), "refs/tags/v*".into()];
        let exclude = vec!["refs/heads/release/*".into()];
        assert!(ref_selected(&include, &exclude, "refs/heads/main"));
        assert!(!ref_selected(&include, &exclude, "refs/heads/release/old"));
        assert!(ref_selected(&include, &exclude, "refs/tags/v1"));
    }

    #[test]
    fn rejects_embedded_http_credentials() {
        let mut config = item();
        config.source = "https://user:secret@example.com/repo.git".into();
        assert!(validate(&[config]).is_err());
    }
}
