use codex_app_server_client::InProcessAppServerClient;
use codex_app_server_client::InProcessServerEvent;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::SandboxPolicy as ApiSandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::Config;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::user_input::UserInput;
use serde_json::Value;
use tokio::time::Instant;

use crate::RequestIdSequencer;
use crate::handle_server_request;
use crate::maybe_backfill_turn_completed_items;
use crate::send_request_with_response;

use super::types::TurnRole;
use super::types::TurnSummary;

#[expect(
    clippy::too_many_arguments,
    reason = "turn start requires explicit session context"
)]
pub(super) async fn run_turn(
    client: &mut InProcessAppServerClient,
    request_ids: &mut RequestIdSequencer,
    thread_id: &str,
    config: &Config,
    role: TurnRole,
    prompt: String,
    output_schema: Option<Value>,
    deadline: Instant,
    error_seen: &mut bool,
) -> anyhow::Result<TurnSummary> {
    if Instant::now() >= deadline {
        anyhow::bail!("automode deadline reached before starting next turn");
    }

    let response: TurnStartResponse = send_request_with_response(
        client,
        ClientRequest::TurnStart {
            request_id: request_ids.next(),
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![
                    UserInput::Text {
                        text: prompt,
                        text_elements: Vec::new(),
                    }
                    .into(),
                ],
                responsesapi_client_metadata: None,
                environments: None,
                cwd: Some(config.cwd.to_path_buf()),
                approval_policy: Some(AskForApproval::Never.into()),
                approvals_reviewer: None,
                sandbox_policy: Some(ApiSandboxPolicy::DangerFullAccess),
                permission_profile: None,
                model: None,
                service_tier: None,
                effort: config.model_reasoning_effort,
                summary: None,
                personality: None,
                output_schema,
                collaboration_mode: None,
            },
        },
        "turn/start",
    )
    .await
    .map_err(anyhow::Error::msg)?;

    let turn_id = response.turn.id;
    let mut collector = TurnCollector::new(role, turn_id.clone());
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);

    loop {
        let event = tokio::select! {
            () = &mut sleep => {
                let _ = send_request_with_response::<TurnInterruptResponse>(
                    client,
                    ClientRequest::TurnInterrupt {
                        request_id: request_ids.next(),
                        params: TurnInterruptParams {
                            thread_id: thread_id.to_string(),
                            turn_id: turn_id.clone(),
                        },
                    },
                    "turn/interrupt",
                )
                .await;
                collector.status = "interrupted_deadline".to_string();
                collector.errors.push("automode deadline interrupted the active turn".to_string());
                return Ok(collector.finish());
            }
            maybe_event = client.next_event() => maybe_event,
        };

        let Some(event) = event else {
            collector.status = "event_stream_closed".to_string();
            return Ok(collector.finish());
        };

        match event {
            InProcessServerEvent::ServerRequest(request) => {
                handle_server_request(client, request, error_seen).await;
            }
            InProcessServerEvent::ServerNotification(mut notification) => {
                maybe_backfill_turn_completed_items(
                    config.ephemeral,
                    client,
                    request_ids,
                    &mut notification,
                )
                .await;
                let completed = collector.record_notification(notification, thread_id, &turn_id);
                if completed {
                    return Ok(collector.finish());
                }
            }
            InProcessServerEvent::Lagged { skipped } => {
                let message =
                    format!("in-process app-server event stream lagged; dropped {skipped} events");
                collector.errors.push(message);
            }
        }
    }
}

struct TurnCollector {
    role: TurnRole,
    turn_id: String,
    status: String,
    final_message: Option<String>,
    commands: Vec<String>,
    file_changes: Vec<String>,
    errors: Vec<String>,
}

impl TurnCollector {
    fn new(role: TurnRole, turn_id: String) -> Self {
        Self {
            role,
            turn_id,
            status: "running".to_string(),
            final_message: None,
            commands: Vec::new(),
            file_changes: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn record_notification(
        &mut self,
        notification: ServerNotification,
        thread_id: &str,
        turn_id: &str,
    ) -> bool {
        match notification {
            ServerNotification::Error(error)
                if error.thread_id == thread_id && error.turn_id == turn_id =>
            {
                self.errors.push(error.error.message);
            }
            ServerNotification::ItemCompleted(item)
                if item.thread_id == thread_id && item.turn_id == turn_id =>
            {
                self.record_item(item.item);
            }
            ServerNotification::TurnCompleted(completed)
                if completed.thread_id == thread_id && completed.turn.id == turn_id =>
            {
                self.status = match completed.turn.status {
                    TurnStatus::Completed => "completed".to_string(),
                    TurnStatus::Failed => "failed".to_string(),
                    TurnStatus::Interrupted => "interrupted".to_string(),
                    TurnStatus::InProgress => "in_progress".to_string(),
                };
                if let Some(error) = completed.turn.error {
                    self.errors.push(error.message);
                }
                for item in completed.turn.items {
                    self.record_item(item);
                }
                return true;
            }
            _ => {}
        }

        false
    }

    fn record_item(&mut self, item: ThreadItem) {
        match item {
            ThreadItem::AgentMessage { text, .. } => {
                self.final_message = Some(text);
            }
            ThreadItem::CommandExecution {
                command,
                exit_code,
                aggregated_output,
                ..
            } => {
                let mut summary = match exit_code {
                    Some(code) => format!("{command} (exit {code})"),
                    None => format!("{command} (exit unknown)"),
                };
                if let Some(output) = aggregated_output
                    && !output.trim().is_empty()
                {
                    summary.push_str(": ");
                    summary.push_str(&truncate_for_state(output.trim(), 600));
                }
                self.commands.push(summary);
            }
            ThreadItem::FileChange { changes, .. } => {
                for change in changes {
                    self.file_changes
                        .push(format!("{:?} {}", change.kind, change.path));
                }
            }
            ThreadItem::McpToolCall {
                server,
                tool,
                error: Some(error),
                ..
            } => {
                self.errors.push(format!(
                    "MCP tool {server}/{tool} failed: {}",
                    error.message
                ));
            }
            ThreadItem::McpToolCall { .. } => {}
            ThreadItem::DynamicToolCall {
                tool,
                success,
                content_items,
                ..
            } => {
                if matches!(success, Some(false)) {
                    self.errors.push(format!("dynamic tool {tool} failed"));
                }
                if let Some(content_items) = content_items
                    && !content_items.is_empty()
                {
                    self.commands.push(format!(
                        "dynamic tool {tool} returned {} content item(s)",
                        content_items.len()
                    ));
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> TurnSummary {
        TurnSummary {
            role: self.role,
            turn_id: self.turn_id,
            status: self.status,
            final_message: self
                .final_message
                .map(|message| truncate_for_state(&message, 4000)),
            commands: truncate_vec(self.commands, 20),
            file_changes: truncate_vec(self.file_changes, 50),
            errors: truncate_vec(self.errors, 20),
        }
    }
}

fn truncate_vec(values: Vec<String>, max_len: usize) -> Vec<String> {
    values
        .into_iter()
        .take(max_len)
        .map(|value| truncate_for_state(&value, 1200))
        .collect()
}

fn truncate_for_state(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    truncated.push_str("...[truncated]");
    truncated
}
