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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodeStartRequest {
    pub(crate) project: PathBuf,
    pub(crate) duration: Duration,
    pub(crate) goal: String,
    pub(crate) skip_git_repo_check: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomodeSlashCommand {
    Start(AutomodeStartRequest),
    Stop,
}

#[derive(Debug)]
pub(crate) struct AutomodeRunState {
    request: AutomodeStartRequest,
    state_dir: PathBuf,
    progress_path: PathBuf,
    deadline: Instant,
    iteration: u64,
    run_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodeTurnPrompt {
    pub(crate) prompt: String,
    pub(crate) display_text: String,
    pub(crate) cwd: PathBuf,
}

pub(crate) const AUTOMODE_USAGE: &str =
    "Usage: /automode <duration> <goal> [--project DIR] [--skip-git-repo-check]";

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
            token if duration.is_none() => {
                duration = Some(parse_automode_duration(token).map_err(|err| {
                    format!("{err}. First positional argument must be the duration.")
                })?);
                index += 1;
            }
            token => {
                goal_parts.push(token.to_string());
                index += 1;
            }
        }
    }

    let duration = duration.ok_or_else(|| AUTOMODE_USAGE.to_string())?;
    if duration.is_zero() {
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
    }))
}

impl AutomodeRunState {
    pub(crate) fn start(request: AutomodeStartRequest, run_id: u64) -> Result<Self, String> {
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

        let state_dir = request.project.join(".codex").join("automode");
        fs::create_dir_all(&state_dir)
            .map_err(|err| format!("Failed to create {}: {err}", state_dir.display()))?;

        let progress_path = state_dir.join(PROGRESS_DOC_FILE);
        if !progress_path.exists() {
            fs::write(
                &progress_path,
                initial_progress_document(&request.goal, &request.project),
            )
            .map_err(|err| format!("Failed to write {}: {err}", progress_path.display()))?;
        }

        let deadline = Instant::now()
            .checked_add(request.duration)
            .unwrap_or_else(Instant::now);

        Ok(Self {
            request,
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

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn deadline_reached(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn progress_path(&self) -> &Path {
        &self.progress_path
    }

    pub(crate) fn next_turn_prompt(&mut self) -> AutomodeTurnPrompt {
        self.iteration += 1;
        let iteration = self.iteration;
        let prompt = build_turn_prompt(
            &self.request,
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
        self.deadline.saturating_duration_since(Instant::now())
    }
}

pub(crate) fn format_automode_command(request: &AutomodeStartRequest) -> String {
    format!(
        "/automode --project {} --duration {} --goal {}",
        request.project.display(),
        format_duration(request.duration),
        request.goal
    )
}

pub(crate) fn automode_started_lines(state: &AutomodeRunState) -> Vec<Line<'static>> {
    let mut lines = vec![
        format_automode_command(&state.request).magenta().into(),
        vec![
            "Automode ".magenta().bold(),
            "started".green(),
            format!(" for {}", state.request.project.display()).into(),
        ]
        .into(),
        vec![
            "  progress: ".dim(),
            state.progress_path.display().to_string().cyan(),
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
        "It runs in this interactive thread, so live output and interrupts work like a normal turn."
            .dim()
            .into(),
    ]
}

fn resolve_project_arg(raw: &str, default_project: &Path) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        default_project.join(path)
    }
}

fn initial_progress_document(goal: &str, project: &Path) -> String {
    format!(
        r#"# Automode Progress

Goal: {goal}

Project: {project}

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
"#,
        project = project.display()
    )
}

fn build_turn_prompt(
    request: &AutomodeStartRequest,
    state_dir: &Path,
    progress_path: &Path,
    iteration: u64,
    remaining: Duration,
) -> String {
    let remaining = format_duration(remaining);
    format!(
        r#"You are running inside Codex automode in the interactive TUI.

Project:
{project}

Goal:
{goal}

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
"#,
        project = request.project.display(),
        goal = request.goal,
        state_dir = state_dir.display(),
        progress_path = progress_path.display(),
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
                duration: Duration::from_secs(600),
                goal: "find the best beach destination".to_string(),
                skip_git_repo_check: false,
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
                duration: Duration::from_secs(7200),
                goal: "run validation".to_string(),
                skip_git_repo_check: true,
            })
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
    }

    #[test]
    fn turn_prompt_points_model_at_progress_document() {
        let request = AutomodeStartRequest {
            project: PathBuf::from("/work/project"),
            duration: Duration::from_secs(600),
            goal: "ship the feature".to_string(),
            skip_git_repo_check: true,
        };

        let prompt = build_turn_prompt(
            &request,
            Path::new("/work/project/.codex/automode"),
            Path::new("/work/project/.codex/automode/progress.md"),
            3,
            Duration::from_secs(120),
        );

        assert!(prompt.contains("Iteration: 3"));
        assert!(prompt.contains("/work/project/.codex/automode/progress.md"));
        assert!(prompt.contains("Read the progress document"));
    }

    #[test]
    fn started_lines_render_snapshot() {
        let request = AutomodeStartRequest {
            project: PathBuf::from("/work/project"),
            duration: Duration::from_secs(600),
            goal: "ship the feature".to_string(),
            skip_git_repo_check: true,
        };
        let state = AutomodeRunState {
            request,
            state_dir: PathBuf::from("/work/project/.codex/automode"),
            progress_path: PathBuf::from("/work/project/.codex/automode/progress.md"),
            deadline: Instant::now(),
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
}
