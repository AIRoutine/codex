use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AutomodeState {
    pub(crate) goal: String,
    pub(crate) project: String,
    pub(crate) started_at: i64,
    pub(crate) deadline_at: i64,
    pub(crate) iteration: u64,
    pub(crate) progress_document: String,
    pub(crate) last_worker_summary: Option<TurnSummary>,
    pub(crate) last_decision: Option<AutomodeDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomodeDecision {
    pub(crate) metrics: Vec<AutomodeMetric>,
    pub(crate) assessment: String,
    pub(crate) learned: Vec<String>,
    pub(crate) document_changes: Vec<String>,
    pub(crate) next_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutomodeMetric {
    pub(crate) name: String,
    pub(crate) baseline: f64,
    pub(crate) current: f64,
    pub(crate) target: f64,
    pub(crate) unit: String,
    pub(crate) direction: MetricDirection,
    pub(crate) measurement_method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricDirection {
    Increase,
    Decrease,
    Equal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TurnSummary {
    pub(crate) role: TurnRole,
    pub(crate) turn_id: String,
    pub(crate) status: String,
    pub(crate) final_message: Option<String>,
    pub(crate) commands: Vec<String>,
    pub(crate) file_changes: Vec<String>,
    pub(crate) errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnRole {
    Operator,
    Worker,
}
