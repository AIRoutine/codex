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
const RUN_PREFIX: &str = "run";

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
            state.progress_document = initial_progress_document(goal, project, state_dir);
        }
        return Ok(state);
    }

    Ok(AutomodeState {
        goal: goal.to_string(),
        project: project.to_string_lossy().to_string(),
        started_at,
        deadline_at,
        iteration: 0,
        progress_document: initial_progress_document(goal, project, state_dir),
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

pub(super) fn resolve_state_dir(
    project: &Path,
    state_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    match state_dir {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Ok(project.join(path)),
        None => {
            let root = project.join(".codex").join("automode");
            let run_name = next_run_name(&root)?;
            Ok(root.join(run_name))
        }
    }
}

pub(super) fn unix_timestamp_secs() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    i64::try_from(secs).unwrap_or(i64::MAX)
}

fn next_run_name(root: &Path) -> anyhow::Result<String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok("run1".to_string()),
        Err(err) => {
            anyhow::bail!(
                "failed to read automode state root {}: {err}",
                root.display()
            );
        }
    };
    let mut max = None;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read automode state root {}", root.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(number) = parse_run_number(&name) else {
            continue;
        };
        max = Some(max.map_or(number, |previous: u64| previous.max(number)));
    }
    let next = match max {
        Some(number) => number
            .checked_add(1)
            .context("automode run number overflowed")?,
        None => 1,
    };
    Ok(format!("{RUN_PREFIX}{next}"))
}

fn parse_run_number(value: &str) -> Option<u64> {
    let suffix = value.strip_prefix(RUN_PREFIX)?;
    let number = suffix.parse::<u64>().ok()?;
    (number > 0 && value == format!("{RUN_PREFIX}{number}")).then_some(number)
}

fn initial_progress_document(goal: &str, project: &Path, state_dir: &Path) -> String {
    let project = project.display();
    let run_name = state_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| parse_run_number(name).is_some());
    let run_line = run_name.map_or(String::new(), |run_name| format!("\n\nRun: {run_name}"));
    format!(
        r#"# Automode Progress

Goal: {goal}

Project: {project}{run_line}

No simulated operator decision has been recorded yet. The first operator turn must define numeric metrics with fixed baselines and targets before worker turns begin.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_state_dir_defaults_inside_project_run_dir() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        assert_eq!(
            resolve_state_dir(project, None).unwrap(),
            project.join(".codex").join("automode").join("run1")
        );
        fs::create_dir_all(project.join(".codex").join("automode").join("run1")).unwrap();
        assert_eq!(
            resolve_state_dir(project, None).unwrap(),
            project.join(".codex").join("automode").join("run2")
        );
    }

    #[test]
    fn resolve_state_dir_keeps_explicit_dir_exact() {
        let project = Path::new("/tmp/project");
        assert_eq!(
            resolve_state_dir(project, Some(PathBuf::from("state"))).unwrap(),
            PathBuf::from("/tmp/project/state")
        );
        assert_eq!(
            resolve_state_dir(project, Some(PathBuf::from("/var/state"))).unwrap(),
            PathBuf::from("/var/state")
        );
    }

    #[test]
    fn initial_progress_document_marks_numbered_runs_only() {
        let project = Path::new("/tmp/project");
        let run_doc = initial_progress_document(
            "test goal",
            project,
            Path::new("/tmp/project/.codex/automode/run2"),
        );
        let explicit_doc =
            initial_progress_document("test goal", project, Path::new("/tmp/project/state"));

        assert!(run_doc.contains("Run: run2"));
        assert!(!explicit_doc.contains("Run: state"));
    }
}
