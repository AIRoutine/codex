use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use codex_exec::parse_automode_duration;
use codex_git_utils::get_git_repo_root;
use ratatui::style::Stylize;
use ratatui::text::Line;

const PROGRESS_DOC_FILE: &str = "progress.md";
const TUI_STATE_FILE: &str = "tui-state.json";
const EXEC_STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodeStartRequest {
    pub(crate) project: PathBuf,
    pub(crate) duration: Option<Duration>,
    pub(crate) goal: String,
    pub(crate) skip_git_repo_check: bool,
    pub(crate) resume: bool,
    pub(crate) run_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomodeSlashCommand {
    Start(AutomodeStartRequest),
    Stop,
}

#[derive(Debug)]
pub(crate) struct AutomodeRunState {
    request: AutomodeStartRequest,
    run_name: String,
    state_dir: PathBuf,
    progress_path: PathBuf,
    deadline: Option<Instant>,
    iteration: u64,
    run_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodeTurnPrompt {
    pub(crate) prompt: String,
    pub(crate) display_text: String,
    pub(crate) cwd: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunScan {
    AllEntries,
    DirectoriesOnly,
}

pub(crate) const AUTOMODE_USAGE: &str = "Usage: /automode [<duration>] <goal> [--project DIR] [--skip-git-repo-check] | /automode resume [RUN] [--duration DURATION] [--project DIR]";

pub(crate) fn parse_automode_slash_args(
    raw: &str,
    default_project: &Path,
) -> Result<AutomodeSlashCommand, String> {
    let tokens = shlex::split(raw)
        .ok_or_else(|| "Could not parse /automode arguments. Check quoted strings.".to_string())?;
    if tokens.is_empty() {
        return Err(AUTOMODE_USAGE.to_string());
    }

    if tokens.len() == 1 && tokens[0].eq_ignore_ascii_case("stop") {
        return Ok(AutomodeSlashCommand::Stop);
    }

    if tokens[0].eq_ignore_ascii_case("resume") {
        return parse_resume_args(&tokens[1..], default_project);
    }

    let mut project = default_project.to_path_buf();
    let mut duration = None;
    let mut goal_parts = Vec::new();
    let mut skip_git_repo_check = false;
    let mut index = 0;

    while index < tokens.len() {
        match tokens[index].as_str() {
            "--project" | "-p" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err("Missing value after --project.".to_string());
                };
                project = resolve_project_arg(value, default_project);
                index += 2;
            }
            "--duration" | "-d" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err("Missing value after --duration.".to_string());
                };
                duration = Some(parse_automode_duration(value)?);
                index += 2;
            }
            "--goal" | "-g" => {
                if index + 1 >= tokens.len() {
                    return Err("Missing value after --goal.".to_string());
                }
                goal_parts.extend(tokens[index + 1..].iter().cloned());
                break;
            }
            "--skip-git-repo-check" => {
                skip_git_repo_check = true;
                index += 1;
            }
            token if duration.is_none() => match parse_automode_duration(token) {
                Ok(parsed_duration) => {
                    duration = Some(parsed_duration);
                    index += 1;
                }
                Err(err) if starts_with_ascii_digit(token) => {
                    return Err(format!(
                        "{err}. First positional argument must be a valid duration or part of the goal."
                    ));
                }
                Err(_) => {
                    goal_parts.push(token.to_string());
                    index += 1;
                }
            },
            token => {
                goal_parts.push(token.to_string());
                index += 1;
            }
        }
    }

    if duration.is_some_and(|duration| duration.is_zero()) {
        return Err("Duration must be greater than zero.".to_string());
    }

    let goal = goal_parts.join(" ");
    if goal.trim().is_empty() {
        return Err("Automode goal must not be empty.".to_string());
    }

    Ok(AutomodeSlashCommand::Start(AutomodeStartRequest {
        project,
        duration,
        goal,
        skip_git_repo_check,
        resume: false,
        run_name: None,
    }))
}

fn parse_resume_args(
    tokens: &[String],
    default_project: &Path,
) -> Result<AutomodeSlashCommand, String> {
    let mut project = default_project.to_path_buf();
    let mut duration = None;
    let mut skip_git_repo_check = false;
    let mut run_name = None;
    let mut index = 0;

    while index < tokens.len() {
        match tokens[index].as_str() {
            "--project" | "-p" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err("Missing value after --project.".to_string());
                };
                project = resolve_project_arg(value, default_project);
                index += 2;
            }
            "--duration" | "-d" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err("Missing value after --duration.".to_string());
                };
                duration = Some(parse_automode_duration(value)?);
                index += 2;
            }
            "--run" | "--name" => {
                let Some(value) = tokens.get(index + 1) else {
                    return Err("Missing value after --run.".to_string());
                };
                run_name = Some(validate_run_name(value)?);
                index += 2;
            }
            "--skip-git-repo-check" => {
                skip_git_repo_check = true;
                index += 1;
            }
            token if duration.is_none() => match parse_automode_duration(token) {
                Ok(parsed_duration) => {
                    duration = Some(parsed_duration);
                    index += 1;
                }
                Err(_) if run_name.is_none() => {
                    run_name = Some(validate_run_name(token)?);
                    index += 1;
                }
                Err(err) => {
                    return Err(format!(
                        "{err}. Resume accepts one run name, one optional duration, and flags."
                    ));
                }
            },
            token if run_name.is_none() => {
                run_name = Some(validate_run_name(token)?);
                index += 1;
            }
            token => {
                return Err(format!("Unexpected /automode resume argument: {token}"));
            }
        }
    }

    if duration.is_some_and(|duration| duration.is_zero()) {
        return Err("Duration must be greater than zero.".to_string());
    }

    let run_name = resolve_resume_run_name(&project, run_name)?;
    let goal = read_resume_goal(&project, &run_name)?;
    Ok(AutomodeSlashCommand::Start(AutomodeStartRequest {
        project,
        duration,
        goal,
        skip_git_repo_check,
        resume: true,
        run_name: Some(run_name),
    }))
}

impl AutomodeRunState {
    pub(crate) fn start(request: AutomodeStartRequest, run_id: u64) -> Result<Self, String> {
        let mut request = request;
        if !request.project.is_dir() {
            return Err(format!(
                "Automode project does not exist or is not a directory: {}",
                request.project.display()
            ));
        }
        if !request.skip_git_repo_check && get_git_repo_root(&request.project).is_none() {
            return Err(
                "Not inside a trusted directory and --skip-git-repo-check was not specified."
                    .to_string(),
            );
        }

        let automode_root = automode_root(&request.project);
        fs::create_dir_all(&automode_root)
            .map_err(|err| format!("Failed to create {}: {err}", automode_root.display()))?;
        let run_name = if request.resume {
            request
                .run_name
                .clone()
                .map(Ok)
                .unwrap_or_else(|| resolve_resume_run_name(&request.project, None))?
        } else {
            next_run_name(&automode_root)?
        };
        request.run_name = Some(run_name.clone());

        let state_dir = automode_root.join(&run_name);
        fs::create_dir_all(&state_dir)
            .map_err(|err| format!("Failed to create {}: {err}", state_dir.display()))?;

        let progress_path = state_dir.join(PROGRESS_DOC_FILE);
        if !progress_path.exists() {
            fs::write(
                &progress_path,
                initial_progress_document(&request.goal, &request.project, &run_name),
            )
            .map_err(|err| format!("Failed to write {}: {err}", progress_path.display()))?;
        }

        write_tui_state(&state_dir, &request)?;

        let deadline = request.duration.map(|duration| {
            Instant::now()
                .checked_add(duration)
                .unwrap_or_else(Instant::now)
        });

        Ok(Self {
            request,
            run_name,
            state_dir,
            progress_path,
            deadline,
            iteration: 0,
            run_id,
        })
    }

    pub(crate) fn run_id(&self) -> u64 {
        self.run_id
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn deadline_reached(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(crate) fn progress_path(&self) -> &Path {
        &self.progress_path
    }

    pub(crate) fn next_turn_prompt(&mut self) -> AutomodeTurnPrompt {
        self.iteration += 1;
        let iteration = self.iteration;
        let prompt = build_turn_prompt(
            &self.request,
            &self.run_name,
            &self.state_dir,
            &self.progress_path,
            iteration,
            self.remaining_duration(),
        );
        let display_text = format!(
            "/automode iteration {iteration}: {}",
            truncate(&self.request.goal, 120)
        );

        AutomodeTurnPrompt {
            prompt,
            display_text,
            cwd: self.request.project.clone(),
        }
    }

    fn remaining_duration(&self) -> Duration {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::MAX)
    }
}

pub(crate) fn format_automode_command(request: &AutomodeStartRequest) -> String {
    match (
        request.resume,
        request.duration,
        request.run_name.as_deref(),
    ) {
        (true, Some(duration), Some(run_name)) => format!(
            "/automode resume {run_name} --project {} --duration {}",
            request.project.display(),
            format_duration(duration)
        ),
        (true, None, Some(run_name)) => format!(
            "/automode resume {run_name} --project {}",
            request.project.display()
        ),
        (true, Some(duration), None) => format!(
            "/automode resume --project {} --duration {}",
            request.project.display(),
            format_duration(duration)
        ),
        (true, None, None) => format!("/automode resume --project {}", request.project.display()),
        (false, Some(duration), _) => format!(
            "/automode --project {} --duration {} --goal {}",
            request.project.display(),
            format_duration(duration),
            request.goal
        ),
        (false, None, _) => format!(
            "/automode --project {} --goal {}",
            request.project.display(),
            request.goal
        ),
    }
}

pub(crate) fn automode_started_lines(state: &AutomodeRunState) -> Vec<Line<'static>> {
    let action = if state.request.resume {
        "resumed"
    } else {
        "started"
    };
    let mut lines = vec![
        format_automode_command(&state.request).magenta().into(),
        vec![
            "Automode ".magenta().bold(),
            action.green(),
            format!(" for {}", state.request.project.display()).into(),
        ]
        .into(),
        vec![
            "  progress: ".dim(),
            state.progress_path.display().to_string().cyan(),
        ]
        .into(),
        vec!["  run: ".dim(), state.run_name.clone().cyan()].into(),
        vec![
            "  duration: ".dim(),
            match state.request.duration {
                Some(duration) => format_duration(duration).into(),
                None => "unlimited".cyan(),
            },
        ]
        .into(),
    ];
    lines.extend(automode_full_access_warning_lines());
    lines
}

pub(crate) fn automode_finished_lines(progress_path: &Path) -> Vec<Line<'static>> {
    vec![
        vec!["Automode ".magenta().bold(), "duration reached.".green()].into(),
        vec![
            "  progress: ".dim(),
            progress_path.display().to_string().cyan(),
        ]
        .into(),
    ]
}

pub(crate) fn automode_stopped_lines(progress_path: Option<&Path>) -> Vec<Line<'static>> {
    let mut lines = vec![vec!["Automode ".magenta().bold(), "stopped.".red()].into()];
    if let Some(progress_path) = progress_path {
        lines.push(
            vec![
                "  progress: ".dim(),
                progress_path.display().to_string().cyan(),
            ]
            .into(),
        );
    }
    lines
}

fn automode_full_access_warning_lines() -> Vec<Line<'static>> {
    vec![
        vec![
            "Automode uses ".into(),
            "danger-full-access".red(),
            " with approval_policy=never.".into(),
        ]
        .into(),
        "It runs in this interactive thread until its duration expires or you interrupt it."
            .dim()
            .into(),
    ]
}

fn starts_with_ascii_digit(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

fn automode_root(project: &Path) -> PathBuf {
    project.join(".codex").join("automode")
}

fn read_resume_goal(project: &Path, run_name: &str) -> Result<String, String> {
    let state_dir = automode_root(project).join(run_name);
    read_goal_from_state_dir(&state_dir).ok_or_else(|| {
        format!(
            "Could not resume automode run {run_name}. No goal found in {}.",
            state_dir.display()
        )
    })
}

fn read_goal_from_state_dir(state_dir: &Path) -> Option<String> {
    read_goal_from_tui_state(state_dir)
        .or_else(|| read_goal_from_exec_state(state_dir))
        .or_else(|| read_goal_from_progress_doc(&state_dir.join(PROGRESS_DOC_FILE)))
}

fn read_goal_from_tui_state(state_dir: &Path) -> Option<String> {
    let path = state_dir.join(TUI_STATE_FILE);
    let contents = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value
        .get("goal")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .map(str::to_string)
}

fn read_goal_from_exec_state(state_dir: &Path) -> Option<String> {
    let path = state_dir.join(EXEC_STATE_FILE);
    let contents = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value
        .get("goal")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .map(str::to_string)
}

fn read_goal_from_progress_doc(progress_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(progress_path).ok()?;
    contents.lines().find_map(|line| {
        line.strip_prefix("Goal:")
            .map(str::trim)
            .filter(|goal| !goal.is_empty())
            .map(str::to_string)
    })
}

fn resolve_resume_run_name(project: &Path, requested: Option<String>) -> Result<String, String> {
    let root = automode_root(project);
    if let Some(run_name) = requested {
        if read_goal_from_state_dir(&root.join(&run_name)).is_some() {
            return Ok(run_name);
        }
        if run_name == "run1"
            && latest_run_name(&root)?.is_none()
            && migrate_legacy_resume_run(&root)?.is_some()
        {
            return Ok(run_name);
        }
        return Ok(run_name);
    }

    if let Some(run_name) = latest_run_name(&root)? {
        return Ok(run_name);
    }
    if let Some(run_name) = migrate_legacy_resume_run(&root)? {
        return Ok(run_name);
    }

    Err(format!(
        "Could not resume automode. No runs found in {}.",
        root.display()
    ))
}

fn validate_run_name(value: &str) -> Result<String, String> {
    parse_run_number(value)
        .map(|_| value.to_string())
        .ok_or_else(|| "Run name must look like run1, run2, ...".to_string())
}

fn next_run_name(root: &Path) -> Result<String, String> {
    let next = match max_run_number(root, RunScan::AllEntries)? {
        Some(number) => number
            .checked_add(1)
            .ok_or_else(|| "Automode run number overflowed.".to_string())?,
        None => 1,
    };
    Ok(format!("run{next}"))
}

fn latest_run_name(root: &Path) -> Result<Option<String>, String> {
    Ok(max_run_number(root, RunScan::DirectoriesOnly)?.map(|number| format!("run{number}")))
}

fn max_run_number(root: &Path, scan: RunScan) -> Result<Option<u64>, String> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("Failed to read {}: {err}", root.display())),
    };
    let mut max = None;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read {}: {err}", root.display()))?;
        if scan == RunScan::DirectoriesOnly {
            let file_type = entry.file_type().map_err(|err| {
                format!(
                    "Failed to read metadata for {}: {err}",
                    entry.path().display()
                )
            })?;
            if !file_type.is_dir() {
                continue;
            }
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(number) = parse_run_number(&name) else {
            continue;
        };
        max = Some(max.map_or(number, |previous: u64| previous.max(number)));
    }
    Ok(max)
}

fn parse_run_number(value: &str) -> Option<u64> {
    let suffix = value.strip_prefix("run")?;
    let number = suffix.parse::<u64>().ok()?;
    (number > 0 && value == format!("run{number}")).then_some(number)
}

fn migrate_legacy_resume_run(root: &Path) -> Result<Option<String>, String> {
    if read_goal_from_state_dir(root).is_none() {
        return Ok(None);
    }

    let run_name = "run1";
    let run_dir = root.join(run_name);
    fs::create_dir_all(&run_dir)
        .map_err(|err| format!("Failed to create {}: {err}", run_dir.display()))?;
    for file_name in [PROGRESS_DOC_FILE, TUI_STATE_FILE, EXEC_STATE_FILE] {
        let source = root.join(file_name);
        let target = run_dir.join(file_name);
        if source.is_file() && !target.exists() {
            fs::copy(&source, &target).map_err(|err| {
                format!(
                    "Failed to copy {} to {}: {err}",
                    source.display(),
                    target.display()
                )
            })?;
        }
    }
    Ok(Some(run_name.to_string()))
}

fn write_tui_state(state_dir: &Path, request: &AutomodeStartRequest) -> Result<(), String> {
    let path = state_dir.join(TUI_STATE_FILE);
    let contents = serde_json::to_vec_pretty(&serde_json::json!({
        "goal": request.goal.as_str(),
        "project": request.project.to_string_lossy(),
        "run_name": request.run_name.as_deref(),
    }))
    .map_err(|err| format!("Failed to serialize {}: {err}", path.display()))?;
    fs::write(&path, contents).map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

fn resolve_project_arg(raw: &str, default_project: &Path) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        default_project.join(path)
    }
}

fn initial_progress_document(goal: &str, project: &Path, run_name: &str) -> String {
    let project = project.display();
    format!(
        r#"# Automode Progress

Goal: {goal}

Project: {project}

Run: {run_name}

The simulated operator must keep this document current after every turn.

## Completion Metrics

Define fixed numeric metrics here before doing substantial work. Each metric must include:
- name
- baseline
- current value
- target
- unit
- measurement method
- direction: increase, decrease, or equal

## Iteration Log

No iterations have been recorded yet.
"#
    )
}

fn build_turn_prompt(
    request: &AutomodeStartRequest,
    run_name: &str,
    state_dir: &Path,
    progress_path: &Path,
    iteration: u64,
    remaining: Duration,
) -> String {
    let remaining = if remaining == Duration::MAX {
        "unlimited; run until /automode stop or interrupt".to_string()
    } else {
        format_duration(remaining)
    };
    let goal = &request.goal;
    let project = request.project.display();
    let progress_path = progress_path.display();
    let state_dir = state_dir.display();
    format!(
        r#"You are running inside Codex automode in the interactive TUI.

Project:
{project}

Goal:
{goal}

Automode run:
{run_name}

Automode state directory:
{state_dir}

Progress document:
{progress_path}

Iteration: {iteration}
Remaining run time before the controller stops automode: {remaining}

You have danger-full-access and approval_policy=never for this automode turn.

Before doing new work:
1. Read the progress document.
2. Evaluate whether the previous turn moved the goal closer using fixed numeric metrics.
3. Update the progress document if you learned anything that improves the metrics, evaluation method, or next-step choice.
4. Decide the single best next step.

Then execute that next step immediately. Do not ask the user for input. If the goal appears complete, use the remaining turn to improve verification, source quality, documentation, reproducibility, or the final report in a measurable way. Keep working until this turn ends; the controller will decide whether to start another turn.
"#
    )
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.trim().chars();
    let mut truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        truncated.push_str("...");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_positional_duration_and_goal() {
        let command = parse_automode_slash_args(
            "10m find the best beach destination",
            Path::new("/work/project"),
        )
        .unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: PathBuf::from("/work/project"),
                duration: Some(Duration::from_secs(600)),
                goal: "find the best beach destination".to_string(),
                skip_git_repo_check: false,
                resume: false,
                run_name: None,
            })
        );
    }

    #[test]
    fn parses_goal_without_duration_as_unlimited() {
        let command = parse_automode_slash_args(
            "find the best beach destination",
            Path::new("/work/project"),
        )
        .unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: PathBuf::from("/work/project"),
                duration: None,
                goal: "find the best beach destination".to_string(),
                skip_git_repo_check: false,
                resume: false,
                run_name: None,
            })
        );
    }

    #[test]
    fn parses_flags_and_relative_project() {
        let command = parse_automode_slash_args(
            "--project child --duration 2h --skip-git-repo-check --goal run validation",
            Path::new("/work/project"),
        )
        .unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: PathBuf::from("/work/project/child"),
                duration: Some(Duration::from_secs(7200)),
                goal: "run validation".to_string(),
                skip_git_repo_check: true,
                resume: false,
                run_name: None,
            })
        );
    }

    #[test]
    fn parses_resume_with_duration_from_progress_doc() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        let state_dir = project.join(".codex").join("automode");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join(PROGRESS_DOC_FILE),
            "# Automode Progress\n\nGoal: resume this goal\n\nProject: /tmp/project\n",
        )
        .unwrap();

        let command = parse_automode_slash_args("resume --duration 30m", project).unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: Some(Duration::from_secs(1800)),
                goal: "resume this goal".to_string(),
                skip_git_repo_check: false,
                resume: true,
                run_name: Some("run1".to_string()),
            })
        );
    }

    #[test]
    fn parses_resume_without_duration_as_unlimited() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        let state_dir = project.join(".codex").join("automode");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join(TUI_STATE_FILE),
            serde_json::json!({ "goal": "resume forever", "project": project.display().to_string() })
                .to_string(),
        )
        .unwrap();

        let command = parse_automode_slash_args("resume", project).unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: None,
                goal: "resume forever".to_string(),
                skip_git_repo_check: false,
                resume: true,
                run_name: Some("run1".to_string()),
            })
        );
    }

    #[test]
    fn parses_resume_with_explicit_run_name() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_run_goal(project, "run1", "first goal");
        write_run_goal(project, "run2", "second goal");

        let command = parse_automode_slash_args("resume run1 --duration 30m", project).unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: Some(Duration::from_secs(1800)),
                goal: "first goal".to_string(),
                skip_git_repo_check: false,
                resume: true,
                run_name: Some("run1".to_string()),
            })
        );
    }

    #[test]
    fn parses_resume_with_run_flag() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_run_goal(project, "run1", "first goal");
        write_run_goal(project, "run2", "second goal");

        let command = parse_automode_slash_args("resume --run run2", project).unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: None,
                goal: "second goal".to_string(),
                skip_git_repo_check: false,
                resume: true,
                run_name: Some("run2".to_string()),
            })
        );
    }

    #[test]
    fn resume_without_run_name_uses_latest_run() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_run_goal(project, "run1", "first goal");
        write_run_goal(project, "run2", "second goal");

        let command = parse_automode_slash_args("resume", project).unwrap();

        assert_eq!(
            command,
            AutomodeSlashCommand::Start(AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: None,
                goal: "second goal".to_string(),
                skip_git_repo_check: false,
                resume: true,
                run_name: Some("run2".to_string()),
            })
        );
    }

    #[test]
    fn new_runs_use_next_numbered_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();

        let first = AutomodeRunState::start(
            AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: None,
                goal: "first goal".to_string(),
                skip_git_repo_check: true,
                resume: false,
                run_name: None,
            },
            1,
        )
        .unwrap();
        let second = AutomodeRunState::start(
            AutomodeStartRequest {
                project: project.to_path_buf(),
                duration: None,
                goal: "second goal".to_string(),
                skip_git_repo_check: true,
                resume: false,
                run_name: None,
            },
            2,
        )
        .unwrap();

        assert_eq!(first.run_name, "run1");
        assert_eq!(
            first.progress_path,
            project
                .join(".codex")
                .join("automode")
                .join("run1")
                .join(PROGRESS_DOC_FILE)
        );
        assert_eq!(second.run_name, "run2");
        assert_eq!(
            second.progress_path,
            project
                .join(".codex")
                .join("automode")
                .join("run2")
                .join(PROGRESS_DOC_FILE)
        );
    }

    #[test]
    fn parses_stop_command() {
        assert_eq!(
            parse_automode_slash_args("stop", Path::new("/work/project")).unwrap(),
            AutomodeSlashCommand::Stop
        );
    }

    #[test]
    fn requires_goal() {
        assert!(parse_automode_slash_args("10m", Path::new("/work/project")).is_err());
        assert!(parse_automode_slash_args("10x run", Path::new("/work/project")).is_err());
    }

    #[test]
    fn turn_prompt_points_model_at_progress_document() {
        let request = AutomodeStartRequest {
            project: PathBuf::from("/work/project"),
            duration: Some(Duration::from_secs(600)),
            goal: "ship the feature".to_string(),
            skip_git_repo_check: true,
            resume: false,
            run_name: Some("run1".to_string()),
        };

        let prompt = build_turn_prompt(
            &request,
            "run1",
            Path::new("/work/project/.codex/automode/run1"),
            Path::new("/work/project/.codex/automode/run1/progress.md"),
            3,
            Duration::from_secs(120),
        );

        assert!(prompt.contains("Iteration: 3"));
        assert!(prompt.contains("Automode run:\nrun1"));
        assert!(prompt.contains("/work/project/.codex/automode/run1/progress.md"));
        assert!(prompt.contains("Read the progress document"));
    }

    #[test]
    fn started_lines_render_snapshot() {
        let request = AutomodeStartRequest {
            project: PathBuf::from("/work/project"),
            duration: Some(Duration::from_secs(600)),
            goal: "ship the feature".to_string(),
            skip_git_repo_check: true,
            resume: false,
            run_name: Some("run1".to_string()),
        };
        let state = AutomodeRunState {
            request,
            run_name: "run1".to_string(),
            state_dir: PathBuf::from("/work/project/.codex/automode/run1"),
            progress_path: PathBuf::from("/work/project/.codex/automode/run1/progress.md"),
            deadline: Some(Instant::now()),
            iteration: 0,
            run_id: 1,
        };

        insta::assert_snapshot!(
            "automode_started_event",
            render_lines(&automode_started_lines(&state))
        );
    }

    fn render_lines(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_to_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    fn write_run_goal(project: &Path, run_name: &str, goal: &str) {
        let state_dir = project.join(".codex").join("automode").join(run_name);
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join(TUI_STATE_FILE),
            serde_json::json!({ "goal": goal, "project": project.display().to_string() })
                .to_string(),
        )
        .unwrap();
    }
}
