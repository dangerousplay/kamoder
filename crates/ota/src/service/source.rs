//! Where updates come from: the [`Source`] trait, its GitHub implementation
//! over the `fetch` crate, and the discovery rules (OTA-14).
//!
//! ```text
//! stable  GET github.com/<repo>/releases/latest/download/manifest.json (+ .minisig)
//!         package: …/releases/latest/download/kamoder-<version>-update.kup
//! beta    GET api.github.com/repos/<repo>/releases?per_page=10   (60 calls/h shared)
//!         newest non-draft release → its manifest.json / .minisig / kamoder-<v>-update.kup assets
//! ```
//!
//! The trait is three byte-level operations so the service's flow (URLs,
//! verification, state) is tested against local files instead of the
//! network; [`GithubSource`] is the only real implementation.

use std::future::Future;
use std::path::Path;

use fetch::github::{self, Release};
use fetch::{Fetcher, RetryPolicy};
use update_proto::{code, Channel};

/// Asset name of the signed manifest.
pub const MANIFEST_ASSET: &str = "manifest.json";
/// Asset name of its minisign signature.
pub const SIGNATURE_ASSET: &str = "manifest.json.minisig";
/// Largest signature accepted (a minisign signature is ~300 bytes).
pub const MAX_SIGNATURE: u64 = 4096;
/// Releases listed for the beta channel (one API call per check).
pub const BETA_PER_PAGE: u8 = 10;

/// `kamoder-<version>-update.kup`, the package asset of a release.
pub fn kup_name(version: &str) -> String {
    format!("kamoder-{version}-update.kup")
}

/// A failure of a [`Source`] operation, already sorted into a wire code
/// ([`code::NETWORK`], [`code::NOT_FOUND`], [`code::RATE_LIMITED`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    pub code: &'static str,
    pub message: String,
}

impl SourceError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<fetch::Error> for SourceError {
    fn from(error: fetch::Error) -> Self {
        let code = match &error {
            fetch::Error::RateLimited { .. } => code::RATE_LIMITED,
            fetch::Error::Http { status: 404, .. } => code::NOT_FOUND,
            _ => code::NETWORK,
        };
        SourceError::new(code, error.to_string())
    }
}

/// Download progress callback: `(bytes so far, total if known)`.
pub type DownloadProgress = Box<dyn FnMut(u64, Option<u64>) + Send>;

/// Byte-level access to releases.
pub trait Source: Send + Sync + 'static {
    /// GETs a small body (manifest, signature), at most `max_len` bytes.
    fn get(
        &self,
        url: &str,
        max_len: u64,
    ) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send;

    /// The newest releases of `repo` (`owner/name`), for the beta channel.
    fn releases(
        &self,
        repo: &str,
    ) -> impl Future<Output = Result<Vec<Release>, SourceError>> + Send;

    /// Downloads `url` into `dest` (resuming `<dest>.part`); returns the size.
    fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: DownloadProgress,
    ) -> impl Future<Output = Result<u64, SourceError>> + Send;
}

/// GitHub over HTTPS.
#[derive(Debug)]
pub struct GithubSource {
    fetcher: Fetcher,
    retry: RetryPolicy,
}

impl GithubSource {
    pub fn new(fetcher: Fetcher) -> Self {
        Self {
            fetcher,
            retry: RetryPolicy::default(),
        }
    }
}

impl Source for GithubSource {
    async fn get(&self, url: &str, max_len: u64) -> Result<Vec<u8>, SourceError> {
        Ok(self.fetcher.get_bytes(url, max_len).await?.to_vec())
    }

    async fn releases(&self, repo: &str) -> Result<Vec<Release>, SourceError> {
        let list = github::list_releases(&self.fetcher, repo, BETA_PER_PAGE).await?;
        if let Some(remaining) = list.rate_limit.remaining {
            tracing::debug!(remaining, "GitHub API calls left this hour");
        }
        Ok(list.releases)
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        mut progress: DownloadProgress,
    ) -> Result<u64, SourceError> {
        let done = self
            .fetcher
            .download_with_retry(url, dest, None, self.retry, |p| {
                progress(p.downloaded, p.total)
            })
            .await?;
        Ok(done.size)
    }
}

/// The signed manifest of the channel's newest release, and how to find
/// its package once the manifest's version is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub manifest: Vec<u8>,
    pub signature: String,
    pub release: ReleaseRef,
}

/// Where a release's assets are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseRef {
    /// `releases/latest/download/<asset>` of the repo.
    Latest { repo: String },
    /// A listed release (beta).
    Listed(Release),
}

impl ReleaseRef {
    /// URL and size (if listed) of the package for `version`.
    pub fn kup(&self, version: &str) -> Result<(String, Option<u64>), SourceError> {
        let name = kup_name(version);
        match self {
            ReleaseRef::Latest { repo } => Ok((github::latest_stable_asset_url(repo, &name), None)),
            ReleaseRef::Listed(release) => release
                .asset(&name)
                .map(|a| {
                    (
                        a.browser_download_url.clone(),
                        Some(a.size).filter(|s| *s > 0),
                    )
                })
                .ok_or_else(|| {
                    SourceError::new(
                        code::NOT_FOUND,
                        format!("release {} has no asset {name}", release.tag_name),
                    )
                }),
        }
    }
}

/// Fetches the newest manifest and signature of `channel` from `repo`.
pub async fn discover<S: Source>(
    source: &S,
    repo: &str,
    channel: Channel,
) -> Result<Discovered, SourceError> {
    let max = ota_manifest::MAX_MANIFEST_BYTES as u64;
    let (manifest_url, signature_url, release) = match channel {
        Channel::Stable => (
            github::latest_stable_asset_url(repo, MANIFEST_ASSET),
            github::latest_stable_asset_url(repo, SIGNATURE_ASSET),
            ReleaseRef::Latest {
                repo: repo.to_owned(),
            },
        ),
        Channel::Beta => {
            let releases = source.releases(repo).await?;
            let release = github::newest(&releases, github::Channel::Beta)
                .cloned()
                .ok_or_else(|| {
                    SourceError::new(code::NOT_FOUND, format!("{repo} has no releases"))
                })?;
            let url = |name: &str| {
                release
                    .asset(name)
                    .map(|a| a.browser_download_url.clone())
                    .ok_or_else(|| {
                        SourceError::new(
                            code::NOT_FOUND,
                            format!("release {} has no asset {name}", release.tag_name),
                        )
                    })
            };
            (
                url(MANIFEST_ASSET)?,
                url(SIGNATURE_ASSET)?,
                ReleaseRef::Listed(release),
            )
        }
    };
    let manifest = source.get(&manifest_url, max).await?;
    let signature = source.get(&signature_url, MAX_SIGNATURE).await?;
    let signature = String::from_utf8(signature).map_err(|_| {
        SourceError::new(
            code::BAD_PACKAGE,
            format!("{signature_url} is not a text signature"),
        )
    })?;
    Ok(Discovered {
        manifest,
        signature,
        release,
    })
}
