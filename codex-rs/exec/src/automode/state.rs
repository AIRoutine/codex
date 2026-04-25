use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use serde_json::Value;

use super::types::AutomodeState;

pub(super) const PROGRESS_DOC_FILE: &str = "progress.md";
pub(super) const STATE_FILE: &str = "state.json";
pub(super) const EVENTS_FILE: &str = "events.jsonl";

pub(super) fn load_state(
    state_dir: &Path,
    goal: &str,
    project: &Path,
    started_at: i64,
    deadline_at: i64,
) -> anyhow::Result<AutomodeState> {
    let state_path = state_dir.join(STATE_FILE);
    if state_path.exists() {
        let contents = fs::read_to_string(&state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let mut state: AutomodeState = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", state_path.display()))?;
        state.goal = goal.to_string();
        state.project = project.to_string_lossy().to_string();
        state.deadline_at = deadline_at;
        if state.progress_document.trim().is_empty() {
            state.progress_document = initial_progress_document(goal, project);
        }
        return Ok(state);
    }

    Ok(AutomodeState {
        goal: goal.to_string(),
        project: project.to_string_lossy().to_string(),
        started_at,
        deadline_at,
        iteration: 0,
        progress_document: initial_progress_document(goal, project),
        last_worker_summary: None,
        last_decision: None,
    })
}

pub(super) fn write_state(state_dir: &Path, state: &AutomodeState) -> anyhow::Result<()> {
    let path = state_dir.join(STATE_FILE);
    let contents = serde_json::to_vec_pretty(state)?;
    fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
}

pub(super) fn append_event(state_dir: &Path, event: Value) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(EVENTS_FILE);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(&event)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

pub(super) fn resolve_state_dir(project: &Path, state_dir: Option<PathBuf>) -> PathBuf {
    match state_dir {
        Some(path) if path.is_absolute() => path,
        Some(path) => project.join(path),
        None => project.join(".codex").join("automode"),
    }
}

pub(super) fn unix_timestamp_secs() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(secs).unwrap_or(i64::MAX)
}

fn initial_progress_document(goal: &str, project: &Path) -> String {
    format!(
        r#"# Automode Progress

Goal: {goal}

Project: {project}

No simulated operator decision has been recorded yet. The first operator turn must define numeric metrics with fixed baselines and targets before worker turns begin.
"#,
        project = project.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_state_dir_defaults_inside_project() {
        let project = Path::new("/tmp/project");
        assert_eq!(
            resolve_state_dir(project, None),
            PathBuf::from("/tmp/project/.codex/automode")
        );
        assert_eq!(
            resolve_state_dir(project, Some(PathBuf::from("state"))),
            PathBuf::from("/tmp/project/state")
        );
        assert_eq!(
            resolve_state_dir(project, Some(PathBuf::from("/var/state"))),
            PathBuf::from("/var/state")
        );
    }
}
