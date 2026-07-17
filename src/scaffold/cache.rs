//! Local cache for contract artifacts, SDK metadata, and template repositories.
//!
//! The cache lives in the platform cache directory under `pacto-bot-api` and is
//! created with restrictive permissions (`0o700` on Unix). Cached entries are
//! reused when they already satisfy the resolved version; `--refresh` forces a
//! re-fetch, and `--prune-cache` removes stale entries older than a TTL.

use pacto_bot_api::errors::DaemonError;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

/// Default cache entry TTL in days.
const DEFAULT_TTL_DAYS: u64 = 90;

/// Cache root subdirectory name.
const CACHE_DIR_NAME: &str = "pacto-bot-api";

/// Local cache for remote artifacts.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Create or open the cache directory, ensuring restrictive permissions.
    ///
    /// The cache directory can be overridden with the `PACTO_CACHE_DIR`
    /// environment variable for testing.
    pub fn new() -> Result<Self, DaemonError> {
        let root = std::env::var("PACTO_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::cache_dir()
                    .map(|d| d.join(CACHE_DIR_NAME))
                    .unwrap_or_else(|| PathBuf::from(".pacto-cache"))
            });
        fs::create_dir_all(&root).map_err(DaemonError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o700);
            fs::set_permissions(&root, permissions).map_err(DaemonError::Io)?;
        }
        Ok(Self { root })
    }

    /// Open a cache at an explicit path, used by tests.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Result<Self, DaemonError> {
        fs::create_dir_all(&path).map_err(DaemonError::Io)?;
        Ok(Self { root: path })
    }

    /// Return the directory where a specific git ref of a template repository
    /// is cached. The caller may `git clone` or `git fetch` into this path.
    pub fn repo_dir(&self, repo_url: &str, ref_name: &str) -> PathBuf {
        self.root
            .join("repos")
            .join(sanitize_dir_name(repo_url))
            .join(sanitize_dir_name(ref_name))
    }

    /// Ensure the template repository is cloned at the requested ref. Returns
    /// the local repository path.
    pub fn ensure_repo(
        &self,
        repo_url: &str,
        ref_name: &str,
        refresh: bool,
    ) -> Result<PathBuf, DaemonError> {
        let dir = self.repo_dir(repo_url, ref_name);

        if refresh && dir.exists() {
            fs::remove_dir_all(&dir).map_err(DaemonError::Io)?;
        }

        if dir.join(".git").is_dir() {
            // Already cloned; fetch latest ref to stay current.
            let output = std::process::Command::new("git")
                .args(["fetch", "origin", ref_name])
                .current_dir(&dir)
                .output()
                .map_err(DaemonError::Io)?;
            if !output.status.success() {
                return Err(DaemonError::Config(format!(
                    "failed to fetch {ref_name} from {repo_url}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            let output = std::process::Command::new("git")
                .args(["checkout", "FETCH_HEAD"])
                .current_dir(&dir)
                .output()
                .map_err(DaemonError::Io)?;
            if !output.status.success() {
                return Err(DaemonError::Config(format!(
                    "failed to checkout {ref_name} in cached repo: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            return Ok(dir);
        }

        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(DaemonError::Io)?;
        }
        fs::create_dir_all(&dir).map_err(DaemonError::Io)?;
        let output = std::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", ref_name, repo_url, "."])
            .current_dir(&dir)
            .output()
            .map_err(DaemonError::Io)?;
        if !output.status.success() {
            return Err(DaemonError::Config(format!(
                "failed to clone {repo_url} at {ref_name}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(dir)
    }

    /// Resolve a contract version that satisfies `range`, downloading it if
    /// necessary. Returns the local path to the cached contract file.
    #[allow(dead_code)]
    pub async fn resolve_contract(
        &self,
        name: &str,
        range: &semver::VersionReq,
        refresh: bool,
    ) -> Result<(semver::Version, PathBuf), DaemonError> {
        let version = self.resolve_contract_version(name, range, refresh).await?;
        let path = self.contract_path(name, &version);
        if path.exists() && !refresh {
            return Ok((version, path));
        }
        self.download_contract(name, &version).await?;
        Ok((version, path))
    }

    /// Resolve a contract version that satisfies `range` from the cache or
    /// remote index.
    pub async fn resolve_contract_version(
        &self,
        name: &str,
        range: &semver::VersionReq,
        refresh: bool,
    ) -> Result<semver::Version, DaemonError> {
        let cached = self.list_cached_versions("contracts", name)?;
        if let Some(v) = cached.iter().find(|v| range.matches(v))
            && !refresh
        {
            return Ok(v.clone());
        }

        let remote = list_github_contract_versions(name)
            .await
            .unwrap_or_default();
        let choice = remote
            .iter()
            .filter(|v| range.matches(v))
            .max()
            .cloned()
            .or_else(|| cached.into_iter().filter(|v| range.matches(v)).max())
            .or_else(|| bundled_contract_version().ok().filter(|v| range.matches(v)))
            .ok_or_else(|| {
                DaemonError::Config(format!(
                    "no published contract version satisfies {range} for {name}"
                ))
            })?;
        Ok(choice)
    }

    /// Path to a cached contract file.
    #[allow(dead_code)]
    pub fn contract_path(&self, name: &str, version: &semver::Version) -> PathBuf {
        self.root
            .join("contracts")
            .join(format!("{}-{}.json", name, version))
    }

    #[allow(dead_code)]
    async fn download_contract(
        &self,
        name: &str,
        version: &semver::Version,
    ) -> Result<(), DaemonError> {
        let path = self.contract_path(name, version);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(DaemonError::Io)?;
        }

        let url = contract_asset_url(name, version)?;
        let body = github_api_get(&url).await?;
        fs::write(&path, body).map_err(DaemonError::Io)?;
        Ok(())
    }

    /// Resolve an SDK version that satisfies `range` from the cache or PyPI.
    pub async fn resolve_sdk_version(
        &self,
        name: &str,
        range: &semver::VersionReq,
        refresh: bool,
    ) -> Result<semver::Version, DaemonError> {
        let cached = self.list_cached_versions("sdks", name)?;
        if let Some(v) = cached.iter().find(|v| range.matches(v))
            && !refresh
        {
            return Ok(v.clone());
        }

        let remote = list_pypi_versions(name).await.unwrap_or_default();
        let choice = remote
            .iter()
            .filter(|v| range.matches(v))
            .max()
            .cloned()
            .or_else(|| cached.into_iter().filter(|v| range.matches(v)).max())
            .or_else(|| bundled_sdk_version().ok().filter(|v| range.matches(v)))
            .ok_or_else(|| {
                DaemonError::Config(format!(
                    "no published SDK version satisfies {range} for {name}"
                ))
            })?;
        Ok(choice)
    }

    /// List semver versions found in a cached artifact subdirectory.
    fn list_cached_versions(
        &self,
        subdir: &str,
        name: &str,
    ) -> Result<Vec<semver::Version>, DaemonError> {
        let dir = self.root.join(subdir);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let prefix = format!("{}-", name);
        let suffix = ".json";
        let mut versions = Vec::new();
        for entry in fs::read_dir(&dir).map_err(DaemonError::Io)? {
            let entry = entry.map_err(DaemonError::Io)?;
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            if let Some(mid) = fname
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(suffix))
                && let Ok(v) = semver::Version::parse(mid)
            {
                versions.push(v);
            }
        }
        Ok(versions)
    }

    /// Remove cache entries older than the default TTL.
    pub fn prune(&self) -> Result<usize, DaemonError> {
        self.prune_older_than(DEFAULT_TTL_DAYS)
    }

    /// Remove cache entries older than `ttl_days`.
    pub fn prune_older_than(&self, ttl_days: u64) -> Result<usize, DaemonError> {
        let ttl = std::time::Duration::from_secs(ttl_days * 24 * 60 * 60);
        let now = SystemTime::now();
        let mut removed = 0usize;

        for subdir in ["contracts", "sdks", "repos"] {
            let dir = self.root.join(subdir);
            if !dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir).map_err(DaemonError::Io)? {
                let entry = entry.map_err(DaemonError::Io)?;
                let path = entry.path();
                let modified = entry.metadata().map_err(DaemonError::Io)?.modified()?;
                if now.duration_since(modified).unwrap_or_default() > ttl {
                    if path.is_dir() {
                        fs::remove_dir_all(&path).map_err(DaemonError::Io)?;
                    } else {
                        fs::remove_file(&path).map_err(DaemonError::Io)?;
                    }
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

const BUNDLED_SDK_PYPROJECT: &str = include_str!("../../python/pyproject.toml");

fn bundled_sdk_version() -> Result<semver::Version, DaemonError> {
    let manifest: toml::Value = toml::from_str(BUNDLED_SDK_PYPROJECT)
        .map_err(|e| DaemonError::Config(format!("bundled SDK pyproject.toml is invalid: {e}")))?;
    let version_str = manifest
        .get("project")
        .and_then(|project| project.get("version"))
        .and_then(|version| version.as_str())
        .ok_or_else(|| {
            DaemonError::Config("bundled SDK pyproject.toml missing project.version".into())
        })?;
    semver::Version::parse(version_str)
        .map_err(|e| DaemonError::Config(format!("bundled SDK version is invalid: {e}")))
}

const BUNDLED_CONTRACT_JSON: &str = include_str!("../../schemas/jsonrpc.json");

fn bundled_contract_version() -> Result<semver::Version, DaemonError> {
    let value: serde_json::Value = serde_json::from_str(BUNDLED_CONTRACT_JSON)
        .map_err(|e| DaemonError::Config(format!("bundled contract JSON is invalid: {e}")))?;
    let version_str = value
        .get("info")
        .and_then(|info| info.get("version"))
        .and_then(|version| version.as_str())
        .ok_or_else(|| DaemonError::Config("bundled contract JSON missing info.version".into()))?;
    semver::Version::parse(version_str)
        .map_err(|e| DaemonError::Config(format!("bundled contract version is invalid: {e}")))
}

fn sanitize_dir_name(url: &str) -> String {
    url.replace(['/', ':', '?', '#', '&', '=', '%'], "_")
}

async fn http_get(url: &str) -> Result<String, DaemonError> {
    http_get_with_headers(url, HeaderMap::new()).await
}

/// Send a GET request with optional headers and return the body as text.
async fn http_get_with_headers(url: &str, headers: HeaderMap) -> Result<String, DaemonError> {
    let response = reqwest::Client::new()
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| DaemonError::Config(format!("failed to fetch {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(DaemonError::Config(format!(
            "failed to fetch {url}: HTTP {} (set GITHUB_TOKEN to avoid GitHub API rate limits)",
            response.status()
        )));
    }
    response
        .text()
        .await
        .map_err(|e| DaemonError::Config(format!("failed to read {url}: {e}")))
}

/// Send a GitHub API request with required Accept header and optional token auth.
async fn github_api_get(url: &str) -> Result<String, DaemonError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("pacto-bot-admin"));
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| DaemonError::Config(format!("invalid GITHUB_TOKEN: {e}")))?;
        headers.insert(AUTHORIZATION, value);
    }
    http_get_with_headers(url, headers).await
}

async fn list_github_contract_versions(name: &str) -> Result<Vec<semver::Version>, DaemonError> {
    let owner = std::env::var("PACTO_GITHUB_OWNER").unwrap_or_else(|_| "covenant-gov".into());
    let repo = std::env::var("PACTO_GITHUB_REPO").unwrap_or_else(|_| "pacto-bot-api".into());
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases");
    let body = github_api_get(&url).await?;
    let releases: Vec<serde_json::Value> = serde_json::from_str(&body)
        .map_err(|e| DaemonError::Config(format!("invalid GitHub releases response: {e}")))?;

    let mut versions = Vec::new();
    for release in releases {
        let assets = release.get("assets").and_then(|a| a.as_array());
        let Some(assets) = assets else {
            continue;
        };
        for asset in assets {
            let asset_name = asset.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let prefix = format!("{}-", name);
            let suffix = ".json";
            if let Some(mid) = asset_name
                .strip_prefix(&prefix)
                .and_then(|s| s.strip_suffix(suffix))
                && let Ok(v) = semver::Version::parse(mid)
            {
                versions.push(v);
            }
        }
    }
    Ok(versions)
}

async fn list_pypi_versions(name: &str) -> Result<Vec<semver::Version>, DaemonError> {
    let url = format!("https://pypi.org/pypi/{name}/json");
    let body = http_get(&url).await?;
    let data: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| DaemonError::Config(format!("invalid PyPI response: {e}")))?;

    let mut versions = Vec::new();
    if let Some(releases) = data.get("releases").and_then(|r| r.as_object()) {
        for key in releases.keys() {
            if let Ok(v) = semver::Version::parse(key) {
                versions.push(v);
            }
        }
    }
    Ok(versions)
}

#[allow(dead_code)]
fn contract_asset_url(name: &str, version: &semver::Version) -> Result<String, DaemonError> {
    let owner = std::env::var("PACTO_GITHUB_OWNER").unwrap_or_else(|_| "covenant-gov".into());
    let repo = std::env::var("PACTO_GITHUB_REPO").unwrap_or_else(|_| "pacto-bot-api".into());
    let crate_version = env!("CARGO_PKG_VERSION");
    Ok(format!(
        "https://github.com/{owner}/{repo}/releases/download/v{crate_version}/{}-{version}.json",
        name
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn cache_creates_restricted_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().to_path_buf()).unwrap();
        assert!(cache.root.exists());
    }

    #[test]
    fn prune_removes_stale_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().to_path_buf()).unwrap();
        let stale = cache
            .root
            .join("contracts")
            .join("pacto-contract-0.1.0.json");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "{}").unwrap();
        let removed = cache.prune_older_than(0).unwrap();
        assert_eq!(removed, 1);
        assert!(!stale.exists());
    }

    #[test]
    fn bundled_contract_version_parses() {
        let version = bundled_contract_version().unwrap();
        assert_eq!(version, semver::Version::new(0, 8, 0));
    }

    #[test]
    fn bundled_sdk_version_parses() {
        let version = bundled_sdk_version().unwrap();
        assert_eq!(version, semver::Version::new(0, 5, 0));
    }
}
