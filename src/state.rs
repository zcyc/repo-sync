use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateFile {
    pub(crate) source: String,
    pub(crate) targets: BTreeMap<String, TargetState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetState {
    pub(crate) last_attempt_ms: u64,
    pub(crate) last_success_ms: Option<u64>,
    pub(crate) consecutive_failures: u32,
    pub(crate) status: String,
    pub(crate) last_error: Option<String>,
    pub(crate) synced_refs: BTreeMap<String, String>,
}

pub(crate) fn load(workspace: &Path) -> io::Result<(PathBuf, StateFile)> {
    let path = state_path(workspace)?;
    if !path.exists() {
        return Ok((path, StateFile::default()));
    }
    let content = fs::read_to_string(&path)?;
    let state = toml::from_str(&content)
        .map_err(|error| io::Error::other(format!("invalid state file: {error}")))?;
    Ok((path, state))
}

pub(crate) fn save(path: &Path, state: &StateFile) -> io::Result<()> {
    let content = toml::to_string_pretty(state)
        .map_err(|error| io::Error::other(format!("serialize state failed: {error}")))?;
    let mut temporary = path.to_path_buf();
    temporary.set_extension("state.toml.tmp");
    fs::write(&temporary, content)?;
    fs::rename(temporary, path)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn state_path(workspace: &Path) -> io::Result<PathBuf> {
    let name = workspace
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "workspace has no file name"))?;
    let mut path = workspace.to_path_buf();
    path.set_file_name(format!("{}.state.toml", name.to_string_lossy()));
    Ok(path)
}
