use std::fs;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use serde_json::json;

use super::state::PROGRESS_DOC_FILE;
use super::state::append_event;
use super::state::write_state;
use super::types::AutomodeDecision;
use super::types::AutomodeMetric;
use super::types::AutomodeState;
use super::types::MetricDirection;
use super::types::TurnSummary;

pub(super) fn apply_decision(
    state_dir: &Path,
    state: &mut AutomodeState,
    decision: AutomodeDecision,
) -> anyhow::Result<()> {
    let progress_document = render_progress_document(state, &decision);
    let progress_path = state_dir.join(PROGRESS_DOC_FILE);
    fs::write(&progress_path, &progress_document)
        .with_context(|| format!("failed to write {}", progress_path.display()))?;

    state.progress_document = progress_document;
    state.last_decision = Some(decision.clone());
    write_state(state_dir, state)?;
    append_event(
        state_dir,
        json!({
            "type": "automode.operatorDecision",
            "iteration": state.iteration,
            "decision": decision,
        }),
    )
}

pub(super) fn operator_prompt(
    project: &Path,
    state_dir: &Path,
    state: &AutomodeState,
    worker_summary: Option<&TurnSummary>,
) -> String {
    let worker_summary_json = worker_summary
        .map(|summary| serde_json::to_string_pretty(summary).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"You are the simulated human operator for Codex automode.

You must read the progress document below before choosing the next step.
The project path is: {project}
The automode state directory is: {state_dir}
The fixed goal is:
{goal}

Rules:
- Return only JSON matching the supplied schema.
- Keep metrics numeric, measurable, and tied to fixed baselines and targets.
- Decide whether the last worker turn moved the project closer to the goal using those metrics.
- Update the metrics or measurement methods when you learned something useful.
- Always produce a non-empty next_prompt. Do not stop because the goal looks complete; choose quality, tests, docs, cleanup, or harder validation next.
- Do not ask the user for input.

Current progress document:
```markdown
{progress_document}
```

Last worker turn summary:
```json
{worker_summary_json}
```
"#,
        project = project.display(),
        state_dir = state_dir.display(),
        goal = state.goal,
        progress_document = state.progress_document,
    )
}

pub(super) fn next_worker_prompt(state: &AutomodeState, state_dir: &Path) -> String {
    let next_prompt = state
        .last_decision
        .as_ref()
        .map(|decision| decision.next_prompt.trim())
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or("Continue working toward the automode goal.");

    format!(
        r#"You are running inside Codex automode.

Read the automode progress document before doing work:
{progress_document}

Goal:
{goal}

Task selected by the simulated operator:
{next_prompt}

Operate autonomously. Do not ask the user for input. If this task appears complete, use the remaining turn to improve verification, tests, documentation, or cleanup that measurably moves the goal forward.
"#,
        progress_document = state_dir.join(PROGRESS_DOC_FILE).display(),
        goal = state.goal,
    )
}

pub(super) fn decision_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["metrics", "assessment", "learned", "document_changes", "next_prompt"],
        "properties": {
            "metrics": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "baseline", "current", "target", "unit", "direction", "measurement_method"],
                    "properties": {
                        "name": { "type": "string" },
                        "baseline": { "type": "number" },
                        "current": { "type": "number" },
                        "target": { "type": "number" },
                        "unit": { "type": "string" },
                        "direction": { "type": "string", "enum": ["increase", "decrease", "equal"] },
                        "measurement_method": { "type": "string" }
                    }
                }
            },
            "assessment": { "type": "string" },
            "learned": { "type": "array", "items": { "type": "string" } },
            "document_changes": { "type": "array", "items": { "type": "string" } },
            "next_prompt": { "type": "string" }
        }
    })
}

pub(super) fn parse_decision(message: &str) -> anyhow::Result<AutomodeDecision> {
    match serde_json::from_str::<AutomodeDecision>(message) {
        Ok(decision) => Ok(decision),
        Err(first_err) => {
            let Some(json_text) = extract_json_object(message) else {
                return Err(first_err.into());
            };
            serde_json::from_str::<AutomodeDecision>(json_text).map_err(anyhow::Error::from)
        }
    }
}

pub(super) fn normalize_decision(
    mut decision: AutomodeDecision,
    state: &AutomodeState,
) -> AutomodeDecision {
    if decision.next_prompt.trim().is_empty() {
        decision.next_prompt = fallback_next_prompt(&state.goal);
    }
    if decision.metrics.is_empty() {
        decision.metrics = fallback_metrics();
    }
    decision
}

pub(super) fn fallback_decision(state: &AutomodeState, reason: &str) -> AutomodeDecision {
    AutomodeDecision {
        metrics: state
            .last_decision
            .as_ref()
            .map(|decision| decision.metrics.clone())
            .filter(|metrics| !metrics.is_empty())
            .unwrap_or_else(fallback_metrics),
        assessment: format!(
            "The operator response was invalid ({reason}). Automode preserved the previous metrics and will continue with a conservative next step."
        ),
        learned: vec![format!(
            "Operator output must stay valid JSON; last parse issue: {reason}"
        )],
        document_changes: vec![
            "Recorded fallback decision after invalid operator output.".to_string(),
        ],
        next_prompt: fallback_next_prompt(&state.goal),
    }
}

fn render_progress_document(state: &AutomodeState, decision: &AutomodeDecision) -> String {
    let mut document = String::new();
    document.push_str("# Automode Progress\n\n");
    document.push_str(&format!("Goal: {}\n\n", state.goal));
    document.push_str(&format!("Project: {}\n\n", state.project));
    document.push_str(&format!("Started at: {}\n\n", state.started_at));
    document.push_str(&format!("Deadline at: {}\n\n", state.deadline_at));
    document.push_str(&format!("Iteration: {}\n\n", state.iteration));
    document.push_str("## Metrics\n\n");
    document.push_str("| Name | Baseline | Current | Target | Direction | Unit | Measurement |\n");
    document.push_str("| --- | ---: | ---: | ---: | --- | --- | --- |\n");
    for metric in &decision.metrics {
        document.push_str(&format!(
            "| {} | {} | {} | {} | {:?} | {} | {} |\n",
            escape_table(&metric.name),
            format_metric_number(metric.baseline),
            format_metric_number(metric.current),
            format_metric_number(metric.target),
            metric.direction,
            escape_table(&metric.unit),
            escape_table(&metric.measurement_method),
        ));
    }
    document.push_str("\n## Assessment\n\n");
    document.push_str(decision.assessment.trim());
    document.push_str("\n\n## Learnings\n\n");
    if decision.learned.is_empty() {
        document.push_str("- None\n");
    } else {
        for item in &decision.learned {
            document.push_str(&format!("- {}\n", item.trim()));
        }
    }
    document.push_str("\n## Document Changes\n\n");
    if decision.document_changes.is_empty() {
        document.push_str("- None\n");
    } else {
        for item in &decision.document_changes {
            document.push_str(&format!("- {}\n", item.trim()));
        }
    }
    document.push_str("\n## Next Prompt\n\n");
    document.push_str(decision.next_prompt.trim());
    document.push('\n');
    document
}

fn extract_json_object(message: &str) -> Option<&str> {
    let start = message.find('{')?;
    let end = message.rfind('}')?;
    (start < end).then(|| &message[start..=end])
}

fn fallback_metrics() -> Vec<AutomodeMetric> {
    vec![AutomodeMetric {
        name: "goal_progress_percent".to_string(),
        baseline: 0.0,
        current: 0.0,
        target: 100.0,
        unit: "percent".to_string(),
        direction: MetricDirection::Increase,
        measurement_method: "Simulated operator estimate based on completed implementation, tests, and validation evidence.".to_string(),
    }]
}

fn fallback_next_prompt(goal: &str) -> String {
    format!(
        "Continue making measurable progress toward this goal: {goal}. Inspect the repository, identify the highest-impact incomplete validation or implementation step, execute it, and report concrete evidence."
    )
}

fn escape_table(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn format_metric_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn apply_decision_writes_progress_document_and_state() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let mut state = AutomodeState {
            goal: "ship feature".to_string(),
            project: "/repo".to_string(),
            started_at: 1,
            deadline_at: 2,
            iteration: 3,
            progress_document: String::new(),
            last_worker_summary: None,
            last_decision: None,
        };
        let decision = AutomodeDecision {
            metrics: fallback_metrics(),
            assessment: "moved from 0 to 10".to_string(),
            learned: vec!["tests exist".to_string()],
            document_changes: vec!["created metrics".to_string()],
            next_prompt: "run tests".to_string(),
        };

        apply_decision(tmp.path(), &mut state, decision)?;

        let progress = fs::read_to_string(tmp.path().join(PROGRESS_DOC_FILE))?;
        assert!(progress.contains("goal_progress_percent"));
        assert!(progress.contains("run tests"));
        let state_json = fs::read_to_string(tmp.path().join(super::super::state::STATE_FILE))?;
        let persisted: AutomodeState = serde_json::from_str(&state_json)?;
        assert_eq!(persisted.iteration, 3);
        assert_eq!(
            persisted.last_decision.expect("decision").next_prompt,
            "run tests"
        );
        Ok(())
    }

    #[test]
    fn parse_decision_accepts_embedded_json_object() -> anyhow::Result<()> {
        let message = r#"prefix {"metrics":[{"name":"tests","baseline":1,"current":2,"target":3,"unit":"count","direction":"increase","measurement_method":"cargo test"}],"assessment":"better","learned":[],"document_changes":[],"next_prompt":"continue"} suffix"#;
        let decision = parse_decision(message)?;
        assert_eq!(decision.metrics[0].name, "tests");
        assert_eq!(decision.next_prompt, "continue");
        Ok(())
    }
}
