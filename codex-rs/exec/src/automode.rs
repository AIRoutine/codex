mod decision;
mod runtime;
mod state;
mod turn;
mod types;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SandboxMode as ApiSandboxMode;
use codex_app_server_protocol::ThreadStartResponse;
use codex_arg0::Arg0DispatchPaths;
use codex_core::config::Config;
use codex_protocol::protocol::AskForApproval;
use serde_json::json;
use tokio::time::Instant;
use tracing::warn;

use crate::AutomodeArgs;
use crate::RequestIdSequencer;
use crate::send_request_with_response;
use crate::thread_start_params_from_config;

use self::decision::apply_decision;
use self::decision::decision_schema;
use self::decision::fallback_decision;
use self::decision::next_worker_prompt;
use self::decision::normalize_decision;
use self::decision::operator_prompt;
use self::decision::parse_decision;
use self::runtime::AutomodeRunArgs;
use self::runtime::AutomodeRuntime;
use self::state::PROGRESS_DOC_FILE;
use self::state::append_event;
use self::state::load_state;
use self::state::unix_timestamp_secs;
use self::state::write_state;
use self::turn::run_turn;
use self::types::AutomodeDecision;
use self::types::AutomodeMetric;
use self::types::MetricDirection;
use self::types::TurnRole;
use self::types::TurnSummary;

#[derive(Clone)]
pub struct AutomodeEventSink {
    handler: Arc<dyn Fn(AutomodeEvent) + Send + Sync + 'static>,
}

impl std::fmt::Debug for AutomodeEventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomodeEventSink").finish_non_exhaustive()
    }
}

impl AutomodeEventSink {
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(AutomodeEvent) + Send + Sync + 'static,
    {
        Self {
            handler: Arc::new(handler),
        }
    }

    fn emit(&self, event: AutomodeEvent) {
        (self.handler)(event);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AutomodeEvent {
    Started {
        goal: String,
        project: PathBuf,
        state_dir: PathBuf,
        progress_path: PathBuf,
        deadline_at: i64,
    },
    OperatorTurnStarted {
        iteration: u64,
    },
    OperatorDecision {
        iteration: u64,
        progress_path: PathBuf,
        assessment: String,
        metrics: Vec<AutomodeMetricSnapshot>,
        next_prompt: String,
    },
    WorkerTurnStarted {
        iteration: u64,
    },
    WorkerTurnCompleted {
        iteration: u64,
        summary: AutomodeTurnSummarySnapshot,
    },
    Finished {
        iteration: u64,
        progress_path: PathBuf,
        error_seen: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutomodeMetricSnapshot {
    pub name: String,
    pub current: f64,
    pub target: f64,
    pub unit: String,
    pub direction: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodeTurnSummarySnapshot {
    pub role: String,
    pub status: String,
    pub final_message: Option<String>,
    pub commands: Vec<String>,
    pub file_changes: Vec<String>,
    pub errors: Vec<String>,
}

pub async fn run_main(args: AutomodeArgs, arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    run_main_with_events(args, arg0_paths, None).await
}

pub async fn run_main_with_events(
    args: AutomodeArgs,
    arg0_paths: Arg0DispatchPaths,
    event_sink: Option<AutomodeEventSink>,
) -> anyhow::Result<()> {
    if let Err(err) =
        codex_login::default_client::set_default_originator("codex_automode".to_string())
    {
        tracing::warn!(?err, "failed to set codex automode originator override");
    }

    let runtime = AutomodeRuntime::build(args, arg0_paths).await?;
    run_loop(runtime, event_sink).await
}

async fn run_loop(
    runtime: AutomodeRuntime,
    event_sink: Option<AutomodeEventSink>,
) -> anyhow::Result<()> {
    let AutomodeRuntime {
        args,
        config,
        in_process_start_args,
    } = runtime;
    let AutomodeRunArgs {
        goal,
        project,
        duration,
        state_dir,
    } = args;

    fs::create_dir_all(&state_dir).with_context(|| {
        format!(
            "failed to create automode state dir {}",
            state_dir.display()
        )
    })?;

    let started_at = unix_timestamp_secs();
    let deadline_at = started_at.saturating_add(duration.as_secs() as i64);
    let deadline = Instant::now() + duration;
    let mut state = load_state(&state_dir, &goal, &project, started_at, deadline_at)?;
    append_event(
        &state_dir,
        json!({
            "type": "automode.started",
            "goal": goal.clone(),
            "project": project.to_string_lossy(),
            "deadlineAt": deadline_at,
        }),
    )?;
    emit_event(
        &event_sink,
        AutomodeEvent::Started {
            goal: goal.clone(),
            project: project.to_path_buf(),
            state_dir: state_dir.clone(),
            progress_path: state_dir.join(PROGRESS_DOC_FILE),
            deadline_at,
        },
    );

    let mut request_ids = RequestIdSequencer::new();
    let mut client = InProcessAppServerClient::start(in_process_start_args)
        .await
        .map_err(|err| {
            anyhow::anyhow!("failed to initialize in-process app-server client: {err}")
        })?;

    let mut thread_params = thread_start_params_from_config(&config);
    thread_params.cwd = Some(project.to_string_lossy().to_string());
    thread_params.approval_policy = Some(AskForApproval::Never.into());
    thread_params.sandbox = Some(ApiSandboxMode::DangerFullAccess);
    thread_params.permission_profile = None;
    thread_params.developer_instructions = Some(automode_developer_instructions());
    let response: ThreadStartResponse = send_request_with_response(
        &client,
        ClientRequest::ThreadStart {
            request_id: request_ids.next(),
            params: thread_params,
        },
        "thread/start",
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let thread_id = response.thread.id;

    eprintln!(
        "Automode started for {} until {} (state: {}).",
        project.display(),
        deadline_at,
        state_dir.display()
    );

    let mut error_seen = false;
    emit_event(
        &event_sink,
        AutomodeEvent::OperatorTurnStarted {
            iteration: state.iteration,
        },
    );
    let first_decision = run_operator_turn(
        &mut client,
        &mut request_ids,
        &thread_id,
        &config,
        &project,
        &state_dir,
        &state,
        None,
        deadline,
    )
    .await?;
    apply_decision(&state_dir, &mut state, first_decision.clone())?;
    emit_operator_decision(&event_sink, state.iteration, &state_dir, &first_decision);

    while Instant::now() < deadline {
        let next_prompt = next_worker_prompt(&state, &state_dir);
        state.iteration = state.iteration.saturating_add(1);
        write_state(&state_dir, &state)?;
        emit_event(
            &event_sink,
            AutomodeEvent::WorkerTurnStarted {
                iteration: state.iteration,
            },
        );

        let worker_summary = run_worker_turn(
            &mut client,
            &mut request_ids,
            &thread_id,
            &config,
            next_prompt,
            deadline,
            &mut error_seen,
        )
        .await?;
        append_event(
            &state_dir,
            json!({
                "type": "automode.workerTurnCompleted",
                "iteration": state.iteration,
                "summary": worker_summary,
            }),
        )?;
        emit_event(
            &event_sink,
            AutomodeEvent::WorkerTurnCompleted {
                iteration: state.iteration,
                summary: AutomodeTurnSummarySnapshot::from_summary(&worker_summary),
            },
        );
        state.last_worker_summary = Some(worker_summary.clone());
        write_state(&state_dir, &state)?;

        if Instant::now() >= deadline {
            break;
        }

        emit_event(
            &event_sink,
            AutomodeEvent::OperatorTurnStarted {
                iteration: state.iteration,
            },
        );
        let decision = run_operator_turn(
            &mut client,
            &mut request_ids,
            &thread_id,
            &config,
            &project,
            &state_dir,
            &state,
            Some(&worker_summary),
            deadline,
        )
        .await?;
        apply_decision(&state_dir, &mut state, decision.clone())?;
        emit_operator_decision(&event_sink, state.iteration, &state_dir, &decision);
    }

    append_event(
        &state_dir,
        json!({
            "type": "automode.finished",
            "iteration": state.iteration,
            "errorSeen": error_seen,
        }),
    )?;
    emit_event(
        &event_sink,
        AutomodeEvent::Finished {
            iteration: state.iteration,
            progress_path: state_dir.join(PROGRESS_DOC_FILE),
            error_seen,
        },
    );
    if let Err(err) = client.shutdown().await {
        warn!("in-process app-server shutdown failed: {err}");
    }
    eprintln!(
        "Automode stopped after {} iteration(s). Progress document: {}",
        state.iteration,
        state_dir.join(PROGRESS_DOC_FILE).display()
    );

    Ok(())
}

fn emit_event(event_sink: &Option<AutomodeEventSink>, event: AutomodeEvent) {
    if let Some(event_sink) = event_sink {
        event_sink.emit(event);
    }
}

fn emit_operator_decision(
    event_sink: &Option<AutomodeEventSink>,
    iteration: u64,
    state_dir: &Path,
    decision: &AutomodeDecision,
) {
    emit_event(
        event_sink,
        AutomodeEvent::OperatorDecision {
            iteration,
            progress_path: state_dir.join(PROGRESS_DOC_FILE),
            assessment: decision.assessment.clone(),
            metrics: decision
                .metrics
                .iter()
                .map(AutomodeMetricSnapshot::from_metric)
                .collect(),
            next_prompt: decision.next_prompt.clone(),
        },
    );
}

impl AutomodeMetricSnapshot {
    fn from_metric(metric: &AutomodeMetric) -> Self {
        Self {
            name: metric.name.clone(),
            current: metric.current,
            target: metric.target,
            unit: metric.unit.clone(),
            direction: match &metric.direction {
                MetricDirection::Increase => "increase",
                MetricDirection::Decrease => "decrease",
                MetricDirection::Equal => "equal",
            }
            .to_string(),
        }
    }
}

impl AutomodeTurnSummarySnapshot {
    fn from_summary(summary: &TurnSummary) -> Self {
        Self {
            role: match summary.role {
                TurnRole::Operator => "operator",
                TurnRole::Worker => "worker",
            }
            .to_string(),
            status: summary.status.clone(),
            final_message: summary.final_message.clone(),
            commands: summary.commands.clone(),
            file_changes: summary.file_changes.clone(),
            errors: summary.errors.clone(),
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "turn start requires explicit session context"
)]
async fn run_operator_turn(
    client: &mut InProcessAppServerClient,
    request_ids: &mut RequestIdSequencer,
    thread_id: &str,
    config: &Config,
    project: &Path,
    state_dir: &Path,
    state: &types::AutomodeState,
    worker_summary: Option<&TurnSummary>,
    deadline: Instant,
) -> anyhow::Result<AutomodeDecision> {
    let prompt = operator_prompt(project, state_dir, state, worker_summary);
    let summary = run_turn(
        client,
        request_ids,
        thread_id,
        config,
        TurnRole::Operator,
        prompt,
        Some(decision_schema()),
        deadline,
        &mut false,
    )
    .await?;

    let Some(message) = summary.final_message.as_deref() else {
        append_event(
            state_dir,
            json!({
                "type": "automode.operatorInvalid",
                "reason": "missing final message",
                "summary": summary,
            }),
        )?;
        return Ok(fallback_decision(
            state,
            "operator turn produced no final message",
        ));
    };

    match parse_decision(message) {
        Ok(decision) => Ok(normalize_decision(decision, state)),
        Err(err) => {
            append_event(
                state_dir,
                json!({
                    "type": "automode.operatorInvalid",
                    "reason": err.to_string(),
                    "message": message,
                }),
            )?;
            Ok(fallback_decision(state, &err.to_string()))
        }
    }
}

async fn run_worker_turn(
    client: &mut InProcessAppServerClient,
    request_ids: &mut RequestIdSequencer,
    thread_id: &str,
    config: &Config,
    prompt: String,
    deadline: Instant,
    error_seen: &mut bool,
) -> anyhow::Result<TurnSummary> {
    run_turn(
        client,
        request_ids,
        thread_id,
        config,
        TurnRole::Worker,
        prompt,
        None,
        deadline,
        error_seen,
    )
    .await
}

fn automode_developer_instructions() -> String {
    "You are running under Codex automode. The environment is intentionally non-interactive. Never ask the user for input; make a reasonable assumption, act, measure progress, and continue. Approval policy is never and sandbox mode is danger-full-access for this session.".to_string()
}
