use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use codex_app_server_client::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
use codex_app_server_client::EnvironmentManager;
use codex_app_server_client::EnvironmentManagerArgs;
use codex_app_server_client::ExecServerRuntimePaths;
use codex_app_server_client::InProcessClientStartArgs;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_arg0::Arg0DispatchPaths;
use codex_cloud_requirements::cloud_requirements_loader_for_storage;
use codex_core::check_execpolicy_for_warnings;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::find_codex_home;
use codex_core::config::load_config_as_toml_with_cli_and_loader_overrides;
use codex_core::config::resolve_oss_provider;
use codex_core::config_loader::ConfigLoadError;
use codex_core::config_loader::LoaderOverrides;
use codex_core::config_loader::format_config_error_with_source;
use codex_core::format_exec_policy_error_with_source;
use codex_feedback::CodexFeedback;
use codex_git_utils::get_git_repo_root;
use codex_login::AuthConfig;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::enforce_login_restrictions;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::canonicalize_existing_preserving_symlinks;
use codex_utils_oss::ensure_oss_provider_ready;
use codex_utils_oss::get_default_model_for_oss_provider;

use crate::AutomodeArgs;

use super::automode_developer_instructions;
use super::state::resolve_state_dir;

pub(crate) struct AutomodeRuntime {
    pub(crate) args: AutomodeRunArgs,
    pub(crate) config: Config,
    pub(crate) in_process_start_args: InProcessClientStartArgs,
}

pub(crate) struct AutomodeRunArgs {
    pub(crate) goal: String,
    pub(crate) project: AbsolutePathBuf,
    pub(crate) duration: Duration,
    pub(crate) state_dir: PathBuf,
}

impl AutomodeRuntime {
    pub(crate) async fn build(
        args: AutomodeArgs,
        arg0_paths: Arg0DispatchPaths,
    ) -> anyhow::Result<Self> {
        let AutomodeArgs {
            shared,
            project,
            duration,
            goal,
            state_dir,
            skip_git_repo_check,
            config_overrides,
        } = args;

        let mut shared = shared.into_inner();
        let project = resolve_project(project, shared.cwd.take())?;
        if !skip_git_repo_check && get_git_repo_root(&project).is_none() {
            anyhow::bail!(
                "Not inside a trusted directory and --skip-git-repo-check was not specified."
            );
        }

        warn_ignored_sandbox_flags(
            shared.sandbox_mode.is_some(),
            shared.full_auto,
            shared.dangerously_bypass_approvals_and_sandbox,
        );

        let state_dir = resolve_state_dir(&project, state_dir)?;
        let codex_home = find_codex_home().context("Error finding codex home")?;
        let cli_kv_overrides = config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;
        let loader_overrides = LoaderOverrides::default();
        let config_toml = load_config_as_toml_with_cli_and_loader_overrides(
            &codex_home,
            Some(&project),
            cli_kv_overrides.clone(),
            loader_overrides.clone(),
        )
        .await
        .map_err(format_load_config_error)?;

        let chatgpt_base_url = config_toml
            .chatgpt_base_url
            .clone()
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string());
        let cloud_requirements = cloud_requirements_loader_for_storage(
            codex_home.to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            config_toml.cli_auth_credentials_store.unwrap_or_default(),
            chatgpt_base_url,
        );

        let model_provider = if shared.oss {
            resolve_oss_provider(
                shared.oss_provider.as_deref(),
                &config_toml,
                shared.config_profile.clone(),
            )
            .or_else(|| {
                eprintln!(
                    "No default OSS provider configured. Use --local-provider=provider or set oss_provider to one of: {LMSTUDIO_OSS_PROVIDER_ID}, {OLLAMA_OSS_PROVIDER_ID} in config.toml"
                );
                None
            })
        } else {
            None
        };
        if shared.oss && model_provider.is_none() {
            anyhow::bail!(
                "No default OSS provider configured. Use --local-provider=provider or set oss_provider to one of: {LMSTUDIO_OSS_PROVIDER_ID}, {OLLAMA_OSS_PROVIDER_ID} in config.toml"
            );
        }

        let model = if shared.model.is_some() {
            shared.model
        } else if shared.oss {
            model_provider
                .as_ref()
                .and_then(|provider_id| get_default_model_for_oss_provider(provider_id))
                .map(std::borrow::ToOwned::to_owned)
        } else {
            None
        };

        let overrides = ConfigOverrides {
            model,
            review_model: None,
            config_profile: shared.config_profile,
            approval_policy: Some(AskForApproval::Never),
            approvals_reviewer: None,
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            permission_profile: None,
            cwd: Some(project.to_path_buf()),
            model_provider: model_provider.clone(),
            service_tier: None,
            codex_self_exe: arg0_paths.codex_self_exe.clone(),
            codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe.clone(),
            zsh_path: None,
            base_instructions: None,
            developer_instructions: Some(automode_developer_instructions()),
            personality: None,
            compact_prompt: None,
            include_apply_patch_tool: None,
            show_raw_agent_reasoning: shared.oss.then_some(true),
            tools_web_search_request: None,
            ephemeral: None,
            additional_writable_roots: shared.add_dir,
        };

        let config = ConfigBuilder::default()
            .cli_overrides(cli_kv_overrides.clone())
            .harness_overrides(overrides)
            .loader_overrides(loader_overrides.clone())
            .cloud_requirements(cloud_requirements.clone())
            .build()
            .await?;

        match check_execpolicy_for_warnings(&config.config_layer_stack).await {
            Ok(None) => {}
            Ok(Some(err)) | Err(err) => {
                anyhow::bail!(
                    "Error loading rules:\n{}",
                    format_exec_policy_error_with_source(&err)
                );
            }
        }

        set_default_client_residency_requirement(config.enforce_residency.value());
        enforce_login_restrictions(&AuthConfig {
            codex_home: config.codex_home.to_path_buf(),
            auth_credentials_store_mode: config.cli_auth_credentials_store_mode,
            forced_login_method: config.forced_login_method,
            forced_chatgpt_workspace_id: config.forced_chatgpt_workspace_id.clone(),
        })?;

        if let Some(provider_id) = model_provider.as_ref() {
            ensure_oss_provider_ready(provider_id, &config)
                .await
                .map_err(|err| anyhow::anyhow!("OSS setup failed: {err}"))?;
        }

        let config_warnings: Vec<ConfigWarningNotification> = config
            .startup_warnings
            .iter()
            .map(|warning| ConfigWarningNotification {
                summary: warning.clone(),
                details: None,
                path: None,
                range: None,
            })
            .collect();
        let local_runtime_paths = ExecServerRuntimePaths::from_optional_paths(
            arg0_paths.codex_self_exe.clone(),
            arg0_paths.codex_linux_sandbox_exe.clone(),
        )?;
        let in_process_start_args = InProcessClientStartArgs {
            arg0_paths,
            config: Arc::new(config.clone()),
            cli_overrides: cli_kv_overrides,
            loader_overrides,
            cloud_requirements,
            feedback: CodexFeedback::new(),
            log_db: None,
            environment_manager: Arc::new(EnvironmentManager::new(
                EnvironmentManagerArgs::from_env(local_runtime_paths),
            )),
            config_warnings,
            session_source: SessionSource::Exec,
            enable_codex_api_key_env: true,
            client_name: "codex_automode".to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            experimental_api: true,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        };

        Ok(Self {
            args: AutomodeRunArgs {
                goal,
                project,
                duration,
                state_dir,
            },
            config,
            in_process_start_args,
        })
    }
}

fn resolve_project(
    project_arg: Option<PathBuf>,
    cwd_arg: Option<PathBuf>,
) -> anyhow::Result<AbsolutePathBuf> {
    let raw_project = match (project_arg, cwd_arg) {
        (Some(project), Some(cwd)) => {
            let project_abs = canonicalize_existing_preserving_symlinks(&project)
                .with_context(|| format!("failed to resolve --project {}", project.display()))?;
            let cwd_abs = canonicalize_existing_preserving_symlinks(&cwd)
                .with_context(|| format!("failed to resolve -C/--cd {}", cwd.display()))?;
            if project_abs != cwd_abs {
                anyhow::bail!("--project and -C/--cd point to different directories");
            }
            project
        }
        (Some(project), None) => project,
        (None, Some(cwd)) => cwd,
        (None, None) => anyhow::bail!("automode requires --project DIR or -C/--cd DIR"),
    };
    let canonical = canonicalize_existing_preserving_symlinks(&raw_project)
        .with_context(|| format!("failed to resolve project path {}", raw_project.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("project path {} is not a directory", canonical.display());
    }
    AbsolutePathBuf::from_absolute_path(canonical)
        .map_err(|err| anyhow::anyhow!("project path is not absolute: {err}"))
}

fn warn_ignored_sandbox_flags(sandbox_set: bool, full_auto: bool, yolo: bool) {
    if sandbox_set || full_auto {
        eprintln!(
            "Automode always uses danger-full-access with approval_policy=never; ignoring sandbox/full-auto overrides."
        );
    } else if !yolo {
        eprintln!("Automode uses danger-full-access with approval_policy=never.");
    }
}

fn format_load_config_error(err: std::io::Error) -> anyhow::Error {
    let config_error = err
        .get_ref()
        .and_then(|source| source.downcast_ref::<ConfigLoadError>())
        .map(ConfigLoadError::config_error);
    if let Some(config_error) = config_error {
        anyhow::anyhow!(
            "Error loading config.toml:\n{}",
            format_config_error_with_source(config_error)
        )
    } else {
        anyhow::anyhow!("Error loading config.toml: {err}")
    }
}
