use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_exec::AutomodeEvent;
use codex_exec::AutomodeMetricSnapshot;
use codex_exec::AutomodeTurnSummarySnapshot;
use codex_exec::parse_automode_duration;
use codex_utils_cli::CliConfigOverrides;
use ratatui::style::Stylize;
use ratatui::text::Line;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomodeStartRequest {
    pub(crate) project: PathBuf,
    pub(crate) duration: Duration,
    pub(crate) goal: String,
    pub(crate) skip_git_repo_check: bool,
}

impl AutomodeStartRequest {
    pub(crate) fn into_exec_args(self) -> codex_exec::AutomodeArgs {
        codex_exec::AutomodeArgs {
            shared: codex_exec::ExecSharedCliOptions::default(),
            project: Some(self.project),
            duration: self.duration,
            goal: self.goal,
            state_dir: None,
            skip_git_repo_check: self.skip_git_repo_check,
            config_overrides: CliConfigOverrides::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomodeSlashCommand {
    Start(AutomodeStartRequest),
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AutomodeUiEvent {
    Runtime(AutomodeEvent),
    Failed { message: String },
    Stopped,
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

pub(crate) fn format_automode_command(request: &AutomodeStartRequest) -> String {
    format!(
        "/automode --project {} --duration {} --goal {}",
        request.project.display(),
        format_duration(request.duration),
        request.goal
    )
}

pub(crate) fn render_automode_event(event: &AutomodeUiEvent) -> Vec<Line<'static>> {
    match event {
        AutomodeUiEvent::Runtime(event) => render_runtime_event(event),
        AutomodeUiEvent::Failed { message } => vec![
            vec![
                "Automode ".magenta().bold(),
                "failed: ".red(),
                truncate(message, 180).into(),
            ]
            .into(),
        ],
        AutomodeUiEvent::Stopped => {
            vec![vec!["Automode ".magenta().bold(), "stopped by user.".red()].into()]
        }
    }
}

pub(crate) fn automode_full_access_warning_lines() -> Vec<Line<'static>> {
    vec![
        vec![
            "Automode uses ".into(),
            "danger-full-access".red(),
            " with approval_policy=never.".into(),
        ]
        .into(),
        "It runs independently in the selected project until the duration expires."
            .dim()
            .into(),
    ]
}

fn render_runtime_event(event: &AutomodeEvent) -> Vec<Line<'static>> {
    match event {
        AutomodeEvent::Started {
            project,
            state_dir,
            progress_path,
            deadline_at,
            ..
        } => vec![
            vec![
                "Automode ".magenta().bold(),
                "started".green(),
                format!(" for {}", project.display()).into(),
            ]
            .into(),
            vec!["  state: ".dim(), state_dir.display().to_string().cyan()].into(),
            vec![
                "  progress: ".dim(),
                progress_path.display().to_string().cyan(),
                format!("; deadline_at={deadline_at}").dim(),
            ]
            .into(),
        ],
        AutomodeEvent::OperatorTurnStarted { iteration } => vec![
            vec![
                "Automode ".magenta().bold(),
                format!("operator iteration {iteration}").into(),
                " reading progress document".dim(),
            ]
            .into(),
        ],
        AutomodeEvent::OperatorDecision {
            iteration,
            progress_path,
            assessment,
            metrics,
            next_prompt,
        } => {
            let mut lines = vec![
                vec![
                    "Automode ".magenta().bold(),
                    format!("operator decision for iteration {iteration}").green(),
                ]
                .into(),
                vec!["  metrics: ".dim(), format_metrics(metrics).into()].into(),
                vec!["  assessment: ".dim(), truncate(assessment, 160).into()].into(),
                vec!["  next: ".dim(), truncate(next_prompt, 160).into()].into(),
            ];
            lines.push(
                vec![
                    "  progress: ".dim(),
                    progress_path.display().to_string().cyan(),
                ]
                .into(),
            );
            lines
        }
        AutomodeEvent::WorkerTurnStarted { iteration } => vec![
            vec![
                "Automode ".magenta().bold(),
                format!("worker iteration {iteration}").into(),
                " started".dim(),
            ]
            .into(),
        ],
        AutomodeEvent::WorkerTurnCompleted { iteration, summary } => {
            render_worker_completed(*iteration, summary)
        }
        AutomodeEvent::Finished {
            iteration,
            progress_path,
            error_seen,
        } => {
            let status = if *error_seen {
                "finished with recorded errors".red()
            } else {
                "finished".green()
            };
            vec![
                vec![
                    "Automode ".magenta().bold(),
                    status,
                    format!(" after {iteration} iteration(s)").into(),
                ]
                .into(),
                vec![
                    "  progress: ".dim(),
                    progress_path.display().to_string().cyan(),
                ]
                .into(),
            ]
        }
    }
}

fn render_worker_completed(
    iteration: u64,
    summary: &AutomodeTurnSummarySnapshot,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        vec![
            "Automode ".magenta().bold(),
            format!("worker iteration {iteration} ").into(),
            summary.status.clone().green(),
            format!(
                " ({} command(s), {} file change(s), {} error(s))",
                summary.commands.len(),
                summary.file_changes.len(),
                summary.errors.len()
            )
            .dim(),
        ]
        .into(),
    ];

    if let Some(message) = summary.final_message.as_deref() {
        lines.push(vec!["  message: ".dim(), truncate(message, 180).into()].into());
    }
    if let Some(file_change) = summary.file_changes.first() {
        lines.push(vec!["  first change: ".dim(), truncate(file_change, 160).into()].into());
    }
    if let Some(error) = summary.errors.first() {
        lines.push(vec!["  first error: ".dim(), truncate(error, 160).red()].into());
    }

    lines
}

fn format_metrics(metrics: &[AutomodeMetricSnapshot]) -> String {
    if metrics.is_empty() {
        return "none".to_string();
    }

    let mut parts = metrics
        .iter()
        .take(3)
        .map(|metric| {
            format!(
                "{}={}/{} {}",
                metric.name, metric.current, metric.target, metric.unit
            )
        })
        .collect::<Vec<_>>();
    if metrics.len() > 3 {
        parts.push(format!("+{} more", metrics.len() - 3));
    }
    parts.join(", ")
}

fn resolve_project_arg(raw: &str, default_project: &Path) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        default_project.join(path)
    }
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
    fn automode_runtime_event_render_snapshot() {
        let event = AutomodeUiEvent::Runtime(AutomodeEvent::OperatorDecision {
            iteration: 2,
            progress_path: PathBuf::from("/work/project/.codex/automode/progress.md"),
            assessment: "Coverage improved and the next validation target is clear.".to_string(),
            metrics: vec![AutomodeMetricSnapshot {
                name: "coverage_percent".to_string(),
                current: 71.5,
                target: 80.0,
                unit: "percent".to_string(),
                direction: "increase".to_string(),
            }],
            next_prompt: "Add focused tests for the remaining parser edge cases.".to_string(),
        });

        insta::assert_snapshot!(
            "automode_operator_decision_event",
            render_lines(&render_automode_event(&event))
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
