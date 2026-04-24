#![cfg(any(not(debug_assertions), test))]
#![cfg_attr(test, allow(dead_code))]

use crate::legacy_core::config::Config;
use crate::update_action;
use crate::update_action::UpdateAction;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_login::default_client::create_client;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::version::CODEX_CLI_VERSION;

pub fn get_upgrade_version(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let update_action = update_action::get_update_action();
    let version_source = match update_action.as_ref() {
        Some(UpdateAction::BrewUpgrade) => VersionSource::HomebrewCask,
        Some(UpdateAction::NpmGlobalLatest)
        | Some(UpdateAction::NpmGlobalVersion(_))
        | Some(UpdateAction::BunGlobalLatest)
        | Some(UpdateAction::BunGlobalVersion(_)) => VersionSource::NpmRegistry,
        Some(UpdateAction::StandaloneUnix) | Some(UpdateAction::StandaloneWindows) | None => {
            VersionSource::GithubRelease
        }
    };
    let info = read_version_info(&version_file).ok();

    if match &info {
        None => true,
        Some(info) => {
            !info.matches_source(version_source)
                || info.last_checked_at < Utc::now() - Duration::hours(20)
        }
    } {
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        tokio::spawn(async move {
            check_for_update(&version_file, version_source)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"))
        });
    }

    info.and_then(|info| {
        if info.matches_source(version_source)
            && is_newer(&info.latest_version, CODEX_CLI_VERSION).unwrap_or(false)
        {
            Some(info.latest_version)
        } else {
            None
        }
    })
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VersionInfo {
    latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    last_checked_at: DateTime<Utc>,
    #[serde(default)]
    dismissed_version: Option<String>,
    #[serde(default)]
    source: Option<VersionSource>,
}

impl VersionInfo {
    fn matches_source(&self, source: VersionSource) -> bool {
        match self.source {
            Some(stored_source) => stored_source == source,
            None => source != VersionSource::NpmRegistry,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum VersionSource {
    GithubRelease,
    HomebrewCask,
    NpmRegistry,
}

const VERSION_FILENAME: &str = "version.json";
// We use the latest version from the cask if installation is via homebrew - homebrew does not immediately pick up the latest release and can lag behind.
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const NPM_CODEX_PACKAGE_URL: &str = "https://registry.npmjs.org/@openai%2fcodex";
const NPM_CODEX_PACKAGE_NAME: &str = "@openai/codex";

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

#[derive(Deserialize, Debug, Clone)]
struct HomebrewCaskInfo {
    version: String,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmPackageInfo {
    #[serde(rename = "dist-tags")]
    dist_tags: NpmDistTags,
    versions: HashMap<String, NpmPackageVersionInfo>,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmDistTags {
    latest: String,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmPackageVersionInfo {
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: HashMap<String, String>,
    dist: Option<NpmPackageDist>,
}

#[derive(Deserialize, Debug, Clone)]
struct NpmPackageDist {
    tarball: Option<String>,
}

#[derive(Clone, Copy)]
struct NpmPlatformPackage {
    npm_name: &'static str,
    npm_tag: &'static str,
}

fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

async fn check_for_update(version_file: &Path, source: VersionSource) -> anyhow::Result<()> {
    let latest_version = match source {
        VersionSource::HomebrewCask => {
            let HomebrewCaskInfo { version } = create_client()
                .get(HOMEBREW_CASK_API_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<HomebrewCaskInfo>()
                .await?;
            version
        }
        VersionSource::GithubRelease => {
            let ReleaseInfo {
                tag_name: latest_tag_name,
            } = create_client()
                .get(LATEST_RELEASE_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<ReleaseInfo>()
                .await?;
            extract_version_from_latest_tag(&latest_tag_name)?
        }
        VersionSource::NpmRegistry => {
            let package_info = create_client()
                .get(NPM_CODEX_PACKAGE_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<NpmPackageInfo>()
                .await?;
            npm_ready_latest_version(&package_info, current_npm_platform_package()?)?
        }
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
        source: Some(source),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

fn current_npm_platform_package() -> anyhow::Result<NpmPlatformPackage> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux" | "android", "x86_64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-linux-x64",
            npm_tag: "linux-x64",
        }),
        ("linux" | "android", "aarch64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-linux-arm64",
            npm_tag: "linux-arm64",
        }),
        ("macos", "x86_64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-darwin-x64",
            npm_tag: "darwin-x64",
        }),
        ("macos", "aarch64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-darwin-arm64",
            npm_tag: "darwin-arm64",
        }),
        ("windows", "x86_64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-win32-x64",
            npm_tag: "win32-x64",
        }),
        ("windows", "aarch64") => Ok(NpmPlatformPackage {
            npm_name: "@openai/codex-win32-arm64",
            npm_tag: "win32-arm64",
        }),
        (os, arch) => anyhow::bail!("unsupported npm platform: {os} ({arch})"),
    }
}

fn npm_ready_latest_version(
    package_info: &NpmPackageInfo,
    platform_package: NpmPlatformPackage,
) -> anyhow::Result<String> {
    let latest = package_info.dist_tags.latest.trim();
    let latest_info = npm_version_info_with_tarball(package_info, latest)?;
    let platform_version = format!("{latest}-{}", platform_package.npm_tag);
    let expected_dependency = format!("npm:{NPM_CODEX_PACKAGE_NAME}@{platform_version}");
    match latest_info
        .optional_dependencies
        .get(platform_package.npm_name)
    {
        Some(dependency) if dependency == &expected_dependency => {}
        Some(dependency) => anyhow::bail!(
            "npm latest {latest} depends on {} as {dependency}, expected {expected_dependency}",
            platform_package.npm_name
        ),
        None => anyhow::bail!(
            "npm latest {latest} is missing optional dependency {}",
            platform_package.npm_name
        ),
    }
    npm_version_info_with_tarball(package_info, &platform_version)?;
    Ok(latest.to_string())
}

fn npm_version_info_with_tarball<'a>(
    package_info: &'a NpmPackageInfo,
    version: &str,
) -> anyhow::Result<&'a NpmPackageVersionInfo> {
    let info = package_info
        .versions
        .get(version)
        .ok_or_else(|| anyhow::anyhow!("npm package version {version} is missing"))?;
    let has_tarball = info
        .dist
        .as_ref()
        .and_then(|dist| dist.tarball.as_deref())
        .is_some_and(|tarball| !tarball.is_empty());
    if !has_tarball {
        anyhow::bail!("npm package version {version} is missing a tarball");
    }
    Ok(info)
}

fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .strip_prefix("rust-v")
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}

/// Persist a dismissal for the current latest version so we don't show
/// the update popup again for this version.
pub async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version_file = version_filepath(config);
    let mut info = match read_version_info(&version_file) {
        Ok(info) => info,
        Err(_) => return Ok(()),
    };
    info.dismissed_version = Some(version.to_string());
    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut iter = v.trim().split('.');
    let maj = iter.next()?.parse::<u64>().ok()?;
    let min = iter.next()?.parse::<u64>().ok()?;
    let pat = iter.next()?.parse::<u64>().ok()?;
    Some((maj, min, pat))
}

fn is_source_build_version(version: &str) -> bool {
    parse_version(version) == Some((0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const TEST_PLATFORM: NpmPlatformPackage = NpmPlatformPackage {
        npm_name: "@openai/codex-darwin-arm64",
        npm_tag: "darwin-arm64",
    };

    fn npm_package_info(
        latest: &str,
        platform_dependency: Option<&str>,
        include_platform_version: bool,
    ) -> NpmPackageInfo {
        let mut versions = serde_json::Map::new();
        let mut root_version = serde_json::json!({
            "dist": { "tarball": format!("https://registry.npmjs.org/@openai/codex/-/codex-{latest}.tgz") }
        });
        if let Some(dependency) = platform_dependency {
            let mut optional_dependencies = serde_json::Map::new();
            optional_dependencies.insert(
                TEST_PLATFORM.npm_name.to_string(),
                serde_json::json!(dependency),
            );
            root_version["optionalDependencies"] = serde_json::Value::Object(optional_dependencies);
        }
        versions.insert(latest.to_string(), root_version);

        if include_platform_version {
            let platform_version = format!("{latest}-{}", TEST_PLATFORM.npm_tag);
            versions.insert(
                platform_version.clone(),
                serde_json::json!({
                    "dist": {
                        "tarball": format!(
                            "https://registry.npmjs.org/@openai/codex/-/codex-{platform_version}.tgz"
                        )
                    }
                }),
            );
        }

        serde_json::from_value(serde_json::json!({
            "dist-tags": { "latest": latest },
            "versions": serde_json::Value::Object(versions),
        }))
        .expect("valid npm package metadata")
    }

    #[test]
    fn extract_version_from_brew_api_json() {
        //
        // https://formulae.brew.sh/api/cask/codex.json
        let cask_json = r#"{
            "token": "codex",
            "full_token": "codex",
            "tap": "homebrew/cask",
            "version": "0.96.0"
        }"#;
        let HomebrewCaskInfo { version } = serde_json::from_str::<HomebrewCaskInfo>(cask_json)
            .expect("failed to parse version from cask json");
        assert_eq!(version, "0.96.0");
    }

    #[test]
    fn extracts_version_from_latest_tag() {
        assert_eq!(
            extract_version_from_latest_tag("rust-v1.5.0").expect("failed to parse version"),
            "1.5.0"
        );
    }

    #[test]
    fn latest_tag_without_prefix_is_invalid() {
        assert!(extract_version_from_latest_tag("v1.5.0").is_err());
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), None);
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), None);
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(parse_version(" 1.2.3 \n"), Some((1, 2, 3)));
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }

    #[test]
    fn old_cache_without_source_is_not_trusted_for_npm_registry() {
        let info = VersionInfo {
            latest_version: "1.2.3".to_string(),
            last_checked_at: Utc::now(),
            dismissed_version: None,
            source: None,
        };

        assert!(info.matches_source(VersionSource::GithubRelease));
        assert!(!info.matches_source(VersionSource::NpmRegistry));
    }

    #[test]
    fn npm_ready_latest_version_requires_platform_optional_dependency() {
        let latest = "1.2.3";
        let platform_version = format!("{latest}-{}", TEST_PLATFORM.npm_tag);
        let package_info = npm_package_info(
            latest,
            Some(&format!("npm:{NPM_CODEX_PACKAGE_NAME}@{platform_version}")),
            /*include_platform_version*/ true,
        );

        assert_eq!(
            npm_ready_latest_version(&package_info, TEST_PLATFORM).expect("npm package is ready"),
            latest
        );
    }

    #[test]
    fn npm_ready_latest_version_rejects_missing_platform_version() {
        let latest = "1.2.3";
        let platform_version = format!("{latest}-{}", TEST_PLATFORM.npm_tag);
        let package_info = npm_package_info(
            latest,
            Some(&format!("npm:{NPM_CODEX_PACKAGE_NAME}@{platform_version}")),
            /*include_platform_version*/ false,
        );

        let err = npm_ready_latest_version(&package_info, TEST_PLATFORM)
            .expect_err("platform tarball must be published");
        assert!(
            err.to_string().contains(&platform_version),
            "error should name missing platform version: {err}"
        );
    }

    #[test]
    fn npm_ready_latest_version_rejects_wrong_platform_dependency() {
        let latest = "1.2.3";
        let package_info = npm_package_info(
            latest,
            Some("npm:@openai/codex@1.2.3-linux-x64"),
            /*include_platform_version*/ true,
        );

        let err = npm_ready_latest_version(&package_info, TEST_PLATFORM)
            .expect_err("platform dependency must match current platform");
        assert!(
            err.to_string().contains("expected"),
            "error should explain expected dependency: {err}"
        );
    }
}
