//! Compatibility resolution for the scaffold bootstrapper.
//!
//! Reads a template's `manifest.toml`, queries the template repository, GitHub
//! releases, and PyPI, and selects a contract/SDK/template triple that
//! satisfies all declared version ranges. The resolved triple is written to
//! the per-bot lock file and consumed by the renderer and updater.

use crate::scaffold::cache::Cache;
use crate::scaffold::lock::{AdminLock, ArtifactLock, ResolvedTriple, TemplateLock};
use pacto_bot_api::errors::DaemonError;
use serde::Deserialize;
use std::path::PathBuf;

/// Parsed template manifest with compatibility metadata.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct TemplateManifest {
    pub manifest_version: u32,

    pub language: String,

    pub kind: String,

    #[serde(default)]
    pub description: String,

    #[serde(rename = "compatibility")]
    pub compatibility: Compatibility,

    #[serde(default)]
    pub protected_files: Vec<String>,
}

/// Compatibility ranges declared by a template.
#[derive(Debug, Clone, Deserialize)]
pub struct Compatibility {
    pub contract: ArtifactRequirement,

    pub sdk: ArtifactRequirement,

    pub daemon: DaemonRequirement,
}

/// Named artifact requirement (contract or SDK).
#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactRequirement {
    pub name: String,

    #[serde(with = "serde_semver_range")]
    pub range: semver::VersionReq,
}

/// Daemon version requirement.
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonRequirement {
    #[serde(with = "serde_semver_range")]
    pub range: semver::VersionReq,
}

/// Source configuration for resolution.
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    /// Template repository URL or local path. Defaults to the upstream
    /// `pacto-bot-templates` repository if not set.
    pub template_repo: String,

    /// Optional git ref to pin. If `None`, the latest semver tag is used.
    pub template_ref: Option<String>,

    /// Language and kind of the template, e.g. `python` / `llm`.
    pub language: String,
    pub kind: String,

    /// Force re-fetching cached artifacts before resolving.
    pub refresh: bool,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            template_repo: std::env::var("PACTO_TEMPLATE_REPO")
                .unwrap_or_else(|_| "https://github.com/covenant-gov/pacto-bot-templates".into()),
            template_ref: None,
            language: "python".into(),
            kind: "llm".into(),
            refresh: false,
        }
    }
}

/// Resolved bundle containing the triple and the manifest that produced it.
#[derive(Debug, Clone)]
pub struct ResolvedBundle {
    pub triple: ResolvedTriple,
    pub manifest: TemplateManifest,
    pub template_dir: PathBuf,
}

/// Resolves a compatible contract/SDK/template triple from remote sources.
#[derive(Debug, Clone)]
pub struct Resolver {
    config: ResolverConfig,
    cache: Cache,
    admin_version: semver::Version,
}

impl Resolver {
    pub fn new(config: ResolverConfig) -> Result<Self, DaemonError> {
        let cache = Cache::new()?;
        Self::with_cache(config, cache)
    }

    /// Build a resolver with an explicit cache, used by tests.
    pub(crate) fn with_cache(config: ResolverConfig, cache: Cache) -> Result<Self, DaemonError> {
        let admin_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|e| DaemonError::Config(format!("invalid package version: {e}")))?;
        Ok(Self {
            config,
            cache,
            admin_version,
        })
    }

    /// Resolve the triple and the template manifest that produced it.
    pub async fn resolve(&self) -> Result<ResolvedBundle, DaemonError> {
        let template_path = format!("{}-{}", self.config.language, self.config.kind);

        let template = self.resolve_template(&template_path)?;
        let manifest = self.load_manifest(&template)?;

        if !manifest
            .compatibility
            .daemon
            .range
            .matches(&self.admin_version)
        {
            return Err(DaemonError::Config(format!(
                "daemon version {} does not satisfy template requirement {}",
                self.admin_version, manifest.compatibility.daemon.range
            )));
        }

        let contract = self
            .resolve_contract(&manifest.compatibility.contract)
            .await?;
        let sdk = self.resolve_sdk(&manifest.compatibility.sdk).await?;

        Ok(ResolvedBundle {
            triple: ResolvedTriple {
                template: TemplateLock {
                    path: template_path,
                    r#ref: template.ref_name,
                    resolved_commit: template.resolved_commit,
                },
                contract,
                sdk,
                admin: AdminLock {
                    version: self.admin_version.to_string(),
                },
            },
            manifest,
            template_dir: template.dir,
        })
    }

    fn resolve_template(&self, template_path: &str) -> Result<ResolvedTemplate, DaemonError> {
        let is_local = PathBuf::from(&self.config.template_repo).is_dir();

        if is_local {
            let repo_root = PathBuf::from(&self.config.template_repo);
            let resolved_ref = self
                .config
                .template_ref
                .clone()
                .unwrap_or_else(|| "HEAD".to_string());
            let resolved_commit = if repo_root.join(".git").is_dir() {
                git_rev_parse(&repo_root, &resolved_ref)?
            } else {
                "local".to_string()
            };
            let template_dir = repo_root.join(template_path);
            if !template_dir.is_dir() {
                return Err(DaemonError::Config(format!(
                    "template not found at {} in local repository {}",
                    template_path,
                    repo_root.display()
                )));
            }
            return Ok(ResolvedTemplate {
                dir: template_dir,
                ref_name: resolved_ref,
                resolved_commit,
            });
        }

        let ref_name = match &self.config.template_ref {
            Some(r) => r.clone(),
            None => git_latest_semver_tag(&self.config.template_repo)?,
        };

        let local_repo =
            self.cache
                .ensure_repo(&self.config.template_repo, &ref_name, self.config.refresh)?;
        let resolved_commit = git_rev_parse(&local_repo, &ref_name)?;
        let template_dir = local_repo.join(template_path);
        if !template_dir.is_dir() {
            return Err(DaemonError::Config(format!(
                "template not found at {} in {}",
                template_path, self.config.template_repo
            )));
        }

        Ok(ResolvedTemplate {
            dir: template_dir,
            ref_name,
            resolved_commit,
        })
    }

    fn load_manifest(&self, template: &ResolvedTemplate) -> Result<TemplateManifest, DaemonError> {
        let manifest_path = template.dir.join("manifest.toml");
        if !manifest_path.exists() {
            return Err(DaemonError::Config(format!(
                "template manifest not found: {}",
                manifest_path.display()
            )));
        }
        let raw = std::fs::read_to_string(&manifest_path).map_err(DaemonError::Io)?;
        let manifest: TemplateManifest = toml::from_str(&raw).map_err(|e| {
            DaemonError::Config(format!(
                "invalid manifest.toml at {}: {e}",
                manifest_path.display()
            ))
        })?;

        if manifest.manifest_version != 1 {
            return Err(DaemonError::Config(format!(
                "unsupported manifest version {} in {} (expected 1)",
                manifest.manifest_version,
                manifest_path.display()
            )));
        }

        Ok(manifest)
    }

    async fn resolve_contract(
        &self,
        req: &ArtifactRequirement,
    ) -> Result<ArtifactLock, DaemonError> {
        let version = self
            .cache
            .resolve_contract_version(&req.name, &req.range, self.config.refresh)
            .await?;
        Ok(ArtifactLock {
            name: req.name.clone(),
            version: version.to_string(),
        })
    }

    async fn resolve_sdk(&self, req: &ArtifactRequirement) -> Result<ArtifactLock, DaemonError> {
        let version = self
            .cache
            .resolve_sdk_version(&req.name, &req.range, self.config.refresh)
            .await?;
        Ok(ArtifactLock {
            name: req.name.clone(),
            version: version.to_string(),
        })
    }
}

struct ResolvedTemplate {
    dir: PathBuf,
    ref_name: String,
    resolved_commit: String,
}

fn git_rev_parse(repo: &std::path::Path, rev: &str) -> Result<String, DaemonError> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", rev])
        .current_dir(repo)
        .output()
        .map_err(DaemonError::Io)?;
    if !output.status.success() {
        return Err(DaemonError::Config(format!(
            "failed to resolve git ref {rev} in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_latest_semver_tag(repo_url: &str) -> Result<String, DaemonError> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--tags", repo_url])
        .output()
        .map_err(|e| {
            DaemonError::Config(format!(
                "failed to list tags for {repo_url}: {e}. Ensure git is installed and the template repository is reachable."
            ))
        })?;
    if !output.status.success() {
        return Err(DaemonError::Config(format!(
            "failed to list tags for {repo_url}: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let mut latest: Option<semver::Version> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Lines look like: <sha>\trefs/tags/v0.1.0
        let Some(tag) = line.split('\t').nth(1) else {
            continue;
        };
        let tag = tag.strip_prefix("refs/tags/").unwrap_or(tag);
        let version_str = tag.strip_prefix('v').unwrap_or(tag);
        if let Ok(v) = semver::Version::parse(version_str)
            && latest.as_ref().is_none_or(|l| v > *l)
        {
            latest = Some(v);
        }
    }

    latest.map(|v| format!("v{v}")).ok_or_else(|| {
        DaemonError::Config(format!(
            "no semver tag found in {repo_url}. Supply --template-ref to pin a branch or commit."
        ))
    })
}

mod serde_semver_range {
    use serde::Deserialize;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<semver::VersionReq, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        semver::VersionReq::parse(&s).map_err(serde::de::Error::custom)
    }

    #[allow(dead_code)]
    pub fn serialize<S>(req: &semver::VersionReq, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&req.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn default_resolver_config_reads_env_override() {
        // This test documents the env-var default; it does not change env.
        let config = ResolverConfig::default();
        assert_eq!(config.language, "python");
        assert_eq!(config.kind, "llm");
        assert!(config.template_repo.starts_with("https://"));
    }
}
