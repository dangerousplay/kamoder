//! GitHub Releases discovery (OTA-14).
//!
//! Stable needs no API call at all: `releases/latest/download/<asset>` is a
//! redirect GitHub maintains to the newest non-prerelease, non-draft release,
//! and it does not count against the API limit. Beta needs the release list
//! (`GET /repos/{repo}/releases`), which unauthenticated is limited to 60
//! calls per hour per IP — shared with everything else behind the same NAT —
//! so the response's `x-ratelimit-*` headers are surfaced and an exhausted
//! limit is its own error ([`Error::RateLimited`]) instead of a bare 403.
//!
//! The channel enum is local on purpose: this crate stays generic and does not
//! depend on the manifest crate.

use std::time::{SystemTime, UNIX_EPOCH};

use hyper::header::RETRY_AFTER;
use hyper::HeaderMap;
use serde::Deserialize;
use snafu::ResultExt;

use crate::error::{Error, JsonSnafu, Result};
use crate::Fetcher;

/// Public API root.
pub const API_BASE: &str = "https://api.github.com";

/// Largest release-list body accepted. Ten releases with notes and a handful
/// of assets each are ~100–300 KB of JSON; parsing keeps only the fields
/// below, but the body itself is buffered.
pub const MAX_RELEASES_JSON: u64 = 1024 * 1024;

/// Pinned REST API version.
pub const API_VERSION: &str = "2022-11-28";

/// Update channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Newest release that is neither a prerelease nor a draft.
    Stable,
    /// Newest non-draft release, prereleases included.
    Beta,
}

/// The fields of a GitHub release this project uses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub html_url: String,
    /// RFC 3339 UTC (`2026-10-17T12:00:00Z`); `None` for drafts.
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// The asset with exactly this file name.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// A release asset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// Rate-limit state reported by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RateLimit {
    /// `x-ratelimit-remaining`.
    pub remaining: Option<u32>,
    /// `x-ratelimit-reset`, unix seconds.
    pub reset: Option<u64>,
}

impl RateLimit {
    /// Reads the `x-ratelimit-*` headers.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let num = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        Self {
            remaining: num("x-ratelimit-remaining").map(|n| n.min(u64::from(u32::MAX)) as u32),
            reset: num("x-ratelimit-reset"),
        }
    }
}

/// A fetched release list and the rate-limit state after the call.
#[derive(Debug, Clone)]
pub struct ReleaseList {
    pub releases: Vec<Release>,
    pub rate_limit: RateLimit,
}

/// URL of `asset` in the newest stable release; no API call involved.
pub fn latest_stable_asset_url(repo: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/{asset}")
}

/// Lists the newest `per_page` releases of `repo` (`owner/name`).
pub async fn list_releases(fetcher: &Fetcher, repo: &str, per_page: u8) -> Result<ReleaseList> {
    list_releases_at(fetcher, API_BASE, repo, per_page).await
}

/// [`list_releases`] against another API root (GitHub Enterprise, tests).
pub async fn list_releases_at(
    fetcher: &Fetcher,
    api_base: &str,
    repo: &str,
    per_page: u8,
) -> Result<ReleaseList> {
    let url = format!(
        "{}/repos/{repo}/releases?per_page={per_page}",
        api_base.trim_end_matches('/')
    );
    let headers = crate::headers([
        ("accept", "application/vnd.github+json"),
        ("x-github-api-version", API_VERSION),
    ])?;
    let resp = fetcher.get(&url, &headers, MAX_RELEASES_JSON).await?;
    let rate_limit = RateLimit::from_headers(&resp.headers);
    let status = resp.status.as_u16();
    if status == 403 || status == 429 {
        let retry_after = resp
            .headers
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        if rate_limit.remaining == Some(0) || retry_after.is_some() || status == 429 {
            let reset = retry_after.map(|s| now_unix() + s).or(rate_limit.reset);
            return Err(Error::RateLimited { reset });
        }
    }
    if !resp.status.is_success() {
        return Err(Error::Http {
            url: resp.url,
            status,
        });
    }
    let releases = parse_releases(&resp.body).context(JsonSnafu { url: resp.url })?;
    Ok(ReleaseList {
        releases,
        rate_limit,
    })
}

/// Parses a `GET /repos/{repo}/releases` body.
pub fn parse_releases(json: &[u8]) -> serde_json::Result<Vec<Release>> {
    serde_json::from_slice(json)
}

/// The newest release for `channel`, by `published_at` (the API's own order
/// is by creation, which differs when a draft is published late). Drafts are
/// never chosen; ties keep the earlier list entry.
pub fn newest(releases: &[Release], channel: Channel) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| !r.draft && (channel == Channel::Beta || !r.prerelease))
        .fold(None, |best: Option<&Release>, r| match best {
            Some(b) if published(b) >= published(r) => Some(b),
            _ => Some(r),
        })
}

/// Sort key: RFC 3339 UTC timestamps in GitHub's fixed format compare
/// correctly as strings; a missing one sorts first.
fn published(r: &Release) -> &str {
    r.published_at.as_deref().unwrap_or("")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/data/releases.json");

    #[test]
    fn parses_fixture() {
        let releases = parse_releases(FIXTURE).unwrap();
        assert_eq!(releases.len(), 4);
        let r = &releases[1];
        assert_eq!(r.tag_name, "v0.3.0");
        assert_eq!(r.name.as_deref(), Some("Kamoder 0.3.0"));
        assert!(!r.prerelease && !r.draft);
        let kup = r.asset("kamoder-0.3.0-update.kup").unwrap();
        assert_eq!(kup.size, 5_242_880);
        assert!(kup
            .browser_download_url
            .ends_with("/v0.3.0/kamoder-0.3.0-update.kup"));
        assert!(r.asset("missing").is_none());
    }

    #[test]
    fn newest_per_channel() {
        let releases = parse_releases(FIXTURE).unwrap();
        // Fixture: [0] draft v0.5.0, [1] stable v0.3.0, [2] prerelease
        // v0.4.0-rc1 (published after v0.3.0), [3] stable v0.2.0.
        assert_eq!(
            newest(&releases, Channel::Stable).unwrap().tag_name,
            "v0.3.0"
        );
        assert_eq!(
            newest(&releases, Channel::Beta).unwrap().tag_name,
            "v0.4.0-rc1"
        );
        assert!(newest(&[], Channel::Beta).is_none());
        let drafts: Vec<_> = releases.iter().filter(|r| r.draft).cloned().collect();
        assert!(newest(&drafts, Channel::Beta).is_none());
    }

    #[test]
    fn stable_url() {
        assert_eq!(
            latest_stable_asset_url("owner/repo", "manifest.json"),
            "https://github.com/owner/repo/releases/latest/download/manifest.json"
        );
    }

    #[test]
    fn rate_limit_headers() {
        let h = crate::headers([
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1760000000"),
        ])
        .unwrap();
        assert_eq!(
            RateLimit::from_headers(&h),
            RateLimit {
                remaining: Some(0),
                reset: Some(1_760_000_000)
            }
        );
        assert_eq!(
            RateLimit::from_headers(&HeaderMap::new()),
            RateLimit::default()
        );
    }

    #[test]
    fn channel_serde() {
        let c: Channel = serde_json::from_str("\"beta\"").unwrap();
        assert_eq!(c, Channel::Beta);
    }
}
