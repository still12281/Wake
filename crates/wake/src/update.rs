use std::time::Duration;

use anyhow::{Context as _, Result};
use semver::Version;
use serde::Deserialize;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/still12281/Wake/releases/latest";
pub(crate) const LATEST_RELEASE_PAGE: &str = "https://github.com/still12281/Wake/releases/latest";

#[derive(Clone, Default)]
pub(crate) enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        latest: String,
    },
    Available {
        latest: String,
    },
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UpdateInfo {
    pub latest_version: Version,
    pub update_available: bool,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

fn evaluate_release(current_version: &str, tag_name: &str) -> Result<UpdateInfo> {
    let current = Version::parse(current_version)
        .with_context(|| format!("invalid current version {current_version:?}"))?;
    let latest_text = tag_name
        .strip_prefix('v')
        .or_else(|| tag_name.strip_prefix('V'))
        .unwrap_or(tag_name);
    let latest = Version::parse(latest_text)
        .with_context(|| format!("invalid GitHub release tag {tag_name:?}"))?;

    Ok(UpdateInfo {
        update_available: latest > current,
        latest_version: latest,
    })
}

/// Only called from the explicit Settings action. Wake never checks in the
/// background, so opening the app still performs no network request.
pub(crate) fn check_latest_release(current_version: &str) -> Result<UpdateInfo> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building update client")?;
    let release = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .header("User-Agent", format!("Wake/{current_version}"))
        .send()
        .context("requesting the latest Wake release")?
        .error_for_status()
        .context("GitHub returned an error while checking for updates")?
        .json::<GithubRelease>()
        .context("reading the latest Wake release")?;

    evaluate_release(current_version, &release.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_release_is_available() {
        let info = evaluate_release("0.2.9", "v0.3.0").unwrap();
        assert_eq!(info.latest_version, Version::new(0, 3, 0));
        assert!(info.update_available);
    }

    #[test]
    fn current_or_older_release_is_not_an_update() {
        assert!(
            !evaluate_release("0.2.9", "v0.2.9")
                .unwrap()
                .update_available
        );
        assert!(
            !evaluate_release("0.3.0", "v0.2.9")
                .unwrap()
                .update_available
        );
    }

    #[test]
    fn malformed_release_tag_is_rejected() {
        assert!(evaluate_release("0.2.9", "latest").is_err());
    }

    #[test]
    #[ignore = "performs a live GitHub request"]
    fn live_latest_release_check_runs_on_a_plain_thread() {
        let info = std::thread::spawn(|| check_latest_release("0.2.8"))
            .join()
            .expect("update check thread should not panic")
            .expect("GitHub release request should succeed");
        assert!(info.update_available);
    }
}
