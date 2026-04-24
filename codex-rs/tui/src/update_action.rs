#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::StandalonePlatform;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g @openai/codex@latest`.
    NpmGlobalLatest,
    /// Update via `npm install -g @openai/codex@<version>`.
    NpmGlobalVersion(String),
    /// Update via `bun install -g @openai/codex@latest`.
    BunGlobalLatest,
    /// Update via `bun install -g @openai/codex@<version>`.
    BunGlobalVersion(String),
    /// Update via `brew upgrade codex`.
    BrewUpgrade,
    /// Update via `curl -fsSL https://chatgpt.com/codex/install.sh | sh`.
    StandaloneUnix,
    /// Update via `irm https://chatgpt.com/codex/install.ps1|iex`.
    StandaloneWindows,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match context {
            InstallContext::Npm => Some(UpdateAction::NpmGlobalLatest),
            InstallContext::Bun => Some(UpdateAction::BunGlobalLatest),
            InstallContext::Brew => Some(UpdateAction::BrewUpgrade),
            InstallContext::Standalone { platform, .. } => Some(match platform {
                StandalonePlatform::Unix => UpdateAction::StandaloneUnix,
                StandalonePlatform::Windows => UpdateAction::StandaloneWindows,
            }),
            InstallContext::Other => None,
        }
    }

    /// Pins npm-style update actions to the version that passed readiness checks.
    pub(crate) fn with_target_version(self, version: &str) -> Self {
        match self {
            UpdateAction::NpmGlobalLatest | UpdateAction::NpmGlobalVersion(_) => {
                UpdateAction::NpmGlobalVersion(version.to_string())
            }
            UpdateAction::BunGlobalLatest | UpdateAction::BunGlobalVersion(_) => {
                UpdateAction::BunGlobalVersion(version.to_string())
            }
            UpdateAction::BrewUpgrade
            | UpdateAction::StandaloneUnix
            | UpdateAction::StandaloneWindows => self,
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(&self) -> (String, Vec<String>) {
        match self {
            UpdateAction::NpmGlobalLatest => (
                "npm".to_string(),
                vec!["install".into(), "-g".into(), "@openai/codex@latest".into()],
            ),
            UpdateAction::NpmGlobalVersion(version) => (
                "npm".to_string(),
                vec![
                    "install".into(),
                    "-g".into(),
                    format!("@openai/codex@{version}"),
                ],
            ),
            UpdateAction::BunGlobalLatest => (
                "bun".to_string(),
                vec!["install".into(), "-g".into(), "@openai/codex@latest".into()],
            ),
            UpdateAction::BunGlobalVersion(version) => (
                "bun".to_string(),
                vec![
                    "install".into(),
                    "-g".into(),
                    format!("@openai/codex@{version}"),
                ],
            ),
            UpdateAction::BrewUpgrade => (
                "brew".to_string(),
                vec!["upgrade".into(), "--cask".into(), "codex".into()],
            ),
            UpdateAction::StandaloneUnix => (
                "sh".to_string(),
                vec![
                    "-c".into(),
                    "curl -fsSL https://chatgpt.com/codex/install.sh | sh".into(),
                ],
            ),
            UpdateAction::StandaloneWindows => (
                "powershell".to_string(),
                vec![
                    "-c".into(),
                    "irm https://chatgpt.com/codex/install.ps1|iex".into(),
                ],
            ),
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(&self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

#[cfg(any(not(debug_assertions), test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn maps_install_context_to_update_action() {
        let native_release_dir = PathBuf::from("/tmp/native-release");

        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Other),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Npm),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Bun),
            Some(UpdateAction::BunGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Brew),
            Some(UpdateAction::BrewUpgrade)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Standalone {
                platform: StandalonePlatform::Unix,
                release_dir: native_release_dir.clone(),
                resources_dir: Some(native_release_dir.join("codex-resources")),
            }),
            Some(UpdateAction::StandaloneUnix)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext::Standalone {
                platform: StandalonePlatform::Windows,
                release_dir: native_release_dir.clone(),
                resources_dir: Some(native_release_dir.join("codex-resources")),
            }),
            Some(UpdateAction::StandaloneWindows)
        );
    }

    #[test]
    fn standalone_update_commands_rerun_latest_installer() {
        assert_eq!(
            UpdateAction::StandaloneUnix.command_args(),
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    "curl -fsSL https://chatgpt.com/codex/install.sh | sh".to_string(),
                ],
            )
        );
        assert_eq!(
            UpdateAction::StandaloneWindows.command_args(),
            (
                "powershell".to_string(),
                vec![
                    "-c".to_string(),
                    "irm https://chatgpt.com/codex/install.ps1|iex".to_string(),
                ],
            )
        );
    }

    #[test]
    fn npm_and_bun_update_commands_can_target_verified_version() {
        assert_eq!(
            UpdateAction::NpmGlobalLatest
                .with_target_version("1.2.3")
                .command_args(),
            (
                "npm".to_string(),
                vec![
                    "install".to_string(),
                    "-g".to_string(),
                    "@openai/codex@1.2.3".to_string(),
                ],
            )
        );
        assert_eq!(
            UpdateAction::BunGlobalLatest
                .with_target_version("1.2.3")
                .command_args(),
            (
                "bun".to_string(),
                vec![
                    "install".to_string(),
                    "-g".to_string(),
                    "@openai/codex@1.2.3".to_string(),
                ],
            )
        );
    }
}
