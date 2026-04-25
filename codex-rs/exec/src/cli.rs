use clap::Args;
use clap::FromArgMatches;
use clap::Parser;
use clap::ValueEnum;
use codex_utils_cli::CliConfigOverrides;
use codex_utils_cli::SharedCliOptions;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    version,
    override_usage = "codex exec [OPTIONS] [PROMPT]\n       codex exec [OPTIONS] <COMMAND> [ARGS]"
)]
pub struct Cli {
    /// Action to perform. If omitted, runs a new non-interactive session.
    #[command(subcommand)]
    pub command: Option<Command>,

    #[clap(flatten)]
    pub shared: ExecSharedCliOptions,

    /// Allow running Codex outside a Git repository.
    #[arg(long = "skip-git-repo-check", global = true, default_value_t = false)]
    pub skip_git_repo_check: bool,

    /// Run without persisting session files to disk.
    #[arg(long = "ephemeral", global = true, default_value_t = false)]
    pub ephemeral: bool,

    /// Do not load `$CODEX_HOME/config.toml`; auth still uses `CODEX_HOME`.
    #[arg(long = "ignore-user-config", global = true, default_value_t = false)]
    pub ignore_user_config: bool,

    /// Do not load user or project execpolicy `.rules` files.
    #[arg(long = "ignore-rules", global = true, default_value_t = false)]
    pub ignore_rules: bool,

    /// Path to a JSON Schema file describing the model's final response shape.
    #[arg(long = "output-schema", value_name = "FILE")]
    pub output_schema: Option<PathBuf>,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    /// Specifies color settings for use in the output.
    #[arg(long = "color", value_enum, default_value_t = Color::Auto)]
    pub color: Color,

    /// Print events to stdout as JSONL.
    #[arg(
        long = "json",
        alias = "experimental-json",
        default_value_t = false,
        global = true
    )]
    pub json: bool,

    /// Specifies file where the last message from the agent should be written.
    #[arg(
        long = "output-last-message",
        short = 'o',
        value_name = "FILE",
        global = true
    )]
    pub last_message_file: Option<PathBuf>,

    /// Initial instructions for the agent. If not provided as an argument (or
    /// if `-` is used), instructions are read from stdin. If stdin is piped and
    /// a prompt is also provided, stdin is appended as a `<stdin>` block.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,
}

impl std::ops::Deref for Cli {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.shared.0
    }
}

impl std::ops::DerefMut for Cli {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared.0
    }
}

#[derive(Args, Debug)]
pub struct AutomodeArgs {
    #[clap(flatten)]
    pub shared: ExecSharedCliOptions,

    /// Project directory automode should operate on. Equivalent to -C/--cd.
    #[arg(long = "project", value_name = "DIR")]
    pub project: Option<PathBuf>,

    /// How long automode should keep running, for example 30m, 2h, or 1h30m.
    #[arg(long = "duration", value_name = "DURATION", value_parser = parse_duration)]
    pub duration: Duration,

    /// Goal automode should pursue for the entire run.
    #[arg(long = "goal", value_name = "TEXT")]
    pub goal: String,

    /// Directory for automode state. Relative paths are resolved inside the project.
    #[arg(long = "state-dir", value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Allow running automode outside a Git repository.
    #[arg(long = "skip-git-repo-check", default_value_t = false)]
    pub skip_git_repo_check: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}

#[derive(Debug, Default)]
pub struct ExecSharedCliOptions(SharedCliOptions);

impl ExecSharedCliOptions {
    pub fn into_inner(self) -> SharedCliOptions {
        self.0
    }
}

impl std::ops::Deref for ExecSharedCliOptions {
    type Target = SharedCliOptions;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ExecSharedCliOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Args for ExecSharedCliOptions {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        mark_exec_global_args(SharedCliOptions::augment_args(cmd))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        mark_exec_global_args(SharedCliOptions::augment_args_for_update(cmd))
    }
}

impl FromArgMatches for ExecSharedCliOptions {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        SharedCliOptions::from_arg_matches(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(matches)
    }
}

fn mark_exec_global_args(cmd: clap::Command) -> clap::Command {
    cmd.mut_arg("model", |arg| arg.global(true))
        .mut_arg("full_auto", |arg| arg.global(true))
        .mut_arg("dangerously_bypass_approvals_and_sandbox", |arg| {
            arg.global(true)
        })
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Resume a previous session by id or pick the most recent with --last.
    Resume(ResumeArgs),

    /// Run a code review against the current repository.
    Review(ReviewArgs),
}

#[derive(Args, Debug)]
struct ResumeArgsRaw {
    // Note: This is the direct clap shape. We reinterpret the positional when --last is set
    // so "codex resume --last <prompt>" treats the positional as a prompt, not a session id.
    /// Conversation/session id (UUID) or thread name. UUIDs take precedence if it parses.
    /// If omitted, use --last to pick the most recent recorded session.
    #[arg(value_name = "SESSION_ID")]
    session_id: Option<String>,

    /// Resume the most recent recorded session (newest) without specifying an id.
    #[arg(long = "last", default_value_t = false)]
    last: bool,

    /// Show all sessions (disables cwd filtering).
    #[arg(long = "all", default_value_t = false)]
    all: bool,

    /// Optional image(s) to attach to the prompt sent after resuming.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1
    )]
    images: Vec<PathBuf>,

    /// Prompt to send after resuming the session. If `-` is used, read from stdin.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    prompt: Option<String>,
}

#[derive(Debug)]
pub struct ResumeArgs {
    /// Conversation/session id (UUID) or thread name. UUIDs take precedence if it parses.
    /// If omitted, use --last to pick the most recent recorded session.
    pub session_id: Option<String>,

    /// Resume the most recent recorded session (newest) without specifying an id.
    pub last: bool,

    /// Show all sessions (disables cwd filtering).
    pub all: bool,

    /// Optional image(s) to attach to the prompt sent after resuming.
    pub images: Vec<PathBuf>,

    /// Prompt to send after resuming the session. If `-` is used, read from stdin.
    pub prompt: Option<String>,
}

impl From<ResumeArgsRaw> for ResumeArgs {
    fn from(raw: ResumeArgsRaw) -> Self {
        // When --last is used without an explicit prompt, treat the positional as the prompt
        // (clap can’t express this conditional positional meaning cleanly).
        let (session_id, prompt) = if raw.last && raw.prompt.is_none() {
            (None, raw.session_id)
        } else {
            (raw.session_id, raw.prompt)
        };
        Self {
            session_id,
            last: raw.last,
            all: raw.all,
            images: raw.images,
            prompt,
        }
    }
}

impl Args for ResumeArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        ResumeArgsRaw::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        ResumeArgsRaw::augment_args_for_update(cmd)
    }
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("duration must not be empty".to_string());
    }

    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        let seconds = trimmed
            .parse::<u64>()
            .map_err(|err| format!("invalid duration `{raw}`: {err}"))?;
        return Ok(Duration::from_secs(seconds));
    }

    let mut total_secs = 0_u64;
    let mut digits = String::new();
    let mut saw_segment = false;

    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }

        if digits.is_empty() {
            return Err(format!("invalid duration `{raw}`"));
        }

        let value = digits
            .parse::<u64>()
            .map_err(|err| format!("invalid duration `{raw}`: {err}"))?;
        digits.clear();

        let multiplier = match ch {
            's' | 'S' => 1,
            'm' | 'M' => 60,
            'h' | 'H' => 60 * 60,
            'd' | 'D' => 24 * 60 * 60,
            _ => {
                return Err(format!(
                    "invalid duration unit `{ch}` in `{raw}`; use s, m, h, or d"
                ));
            }
        };

        total_secs = total_secs
            .checked_add(
                value
                    .checked_mul(multiplier)
                    .ok_or_else(|| format!("duration `{raw}` is too large"))?,
            )
            .ok_or_else(|| format!("duration `{raw}` is too large"))?;
        saw_segment = true;
    }

    if !digits.is_empty() {
        return Err(format!(
            "invalid duration `{raw}`; trailing number needs a unit"
        ));
    }

    if !saw_segment || total_secs == 0 {
        return Err(format!("invalid duration `{raw}`"));
    }

    Ok(Duration::from_secs(total_secs))
}

#[cfg(test)]
mod automode_cli_tests {
    use super::parse_duration;
    use std::time::Duration;

    #[test]
    fn parse_duration_accepts_seconds_minutes_hours_and_days() {
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("1d2h").unwrap(), Duration::from_secs(93600));
    }

    #[test]
    fn parse_duration_rejects_invalid_values() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration("10m5").is_err());
        assert!(parse_duration("0m").is_err());
    }
}

impl FromArgMatches for ResumeArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        ResumeArgsRaw::from_arg_matches(matches).map(Self::from)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = ResumeArgsRaw::from_arg_matches(matches).map(Self::from)?;
        Ok(())
    }
}

#[derive(Parser, Debug)]
pub struct ReviewArgs {
    /// Review staged, unstaged, and untracked changes.
    #[arg(
        long = "uncommitted",
        default_value_t = false,
        conflicts_with_all = ["base", "commit", "prompt"]
    )]
    pub uncommitted: bool,

    /// Review changes against the given base branch.
    #[arg(
        long = "base",
        value_name = "BRANCH",
        conflicts_with_all = ["uncommitted", "commit", "prompt"]
    )]
    pub base: Option<String>,

    /// Review the changes introduced by a commit.
    #[arg(
        long = "commit",
        value_name = "SHA",
        conflicts_with_all = ["uncommitted", "base", "prompt"]
    )]
    pub commit: Option<String>,

    /// Optional commit title to display in the review summary.
    #[arg(long = "title", value_name = "TITLE", requires = "commit")]
    pub commit_title: Option<String>,

    /// Custom review instructions. If `-` is used, read from stdin.
    #[arg(value_name = "PROMPT", value_hint = clap::ValueHint::Other)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Color {
    Always,
    Never,
    #[default]
    Auto,
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
