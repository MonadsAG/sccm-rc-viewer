//! The release feed: an OCI artifact in the GitHub Container Registry
//! (`ghcr.io/<repo>`), pushed by `.github/workflows/publish-ghcr.yml`.
//!
//! The tag `latest` points at a manifest whose single layer is the `.dmg`
//! and whose annotations carry the release label and build number. Public
//! packages are read with an anonymous pull token. This file only builds
//! URLs and parses responses; the HTTP calls live in `mod.rs`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

pub const DEFAULT_REPO: &str = "monadsag/sccm-rc-viewer";
pub const REGISTRY: &str = "https://ghcr.io";
pub const TAG: &str = "latest";
pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json";

pub const ANN_VERSION: &str = "org.opencontainers.image.version";
pub const ANN_BUILD: &str = "ch.monads.sccm-rc.build";
pub const ANN_NOTES: &str = "ch.monads.sccm-rc.release-url";

/// The repository to read, `SCCM_RC_UPDATE_REPO` overriding the default
/// (tests against a separate package).
pub fn repo() -> String {
    std::env::var("SCCM_RC_UPDATE_REPO")
        .ok()
        .map(|r| r.trim().trim_start_matches("ghcr.io/").to_lowercase())
        .filter(|r| valid_repo(r))
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

fn valid_repo(r: &str) -> bool {
    let mut parts = r.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

pub fn token_url(repo: &str) -> String {
    format!("{REGISTRY}/token?scope=repository:{repo}:pull")
}

pub fn manifest_url(repo: &str) -> String {
    format!("{REGISTRY}/v2/{repo}/manifests/{TAG}")
}

pub fn blob_url(repo: &str, digest: &str) -> String {
    format!("{REGISTRY}/v2/{repo}/blobs/{digest}")
}

/// What the feed offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Shown to the user, e.g. `0.9.14` or `0.9.13-m2`.
    pub label: String,
    /// `CFBundleVersion` of the app inside; compared with the running one.
    pub build: u64,
    /// `sha256:<64 hex>` of the `.dmg`.
    pub digest: String,
    pub size: u64,
    pub notes_url: Option<String>,
}

impl Release {
    /// The lowercase hex SHA-256 from `digest`.
    pub fn sha256(&self) -> &str {
        self.digest.trim_start_matches("sha256:")
    }
}

pub fn parse_token(body: &str) -> Result<String> {
    let v: Value = serde_json::from_str(body).context("token response is not JSON")?;
    v.get("token")
        .or_else(|| v.get("access_token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("token response has no token"))
}

/// Parse the manifest of `latest` into a [`Release`].
pub fn parse_manifest(body: &str) -> Result<Release> {
    let v: Value = serde_json::from_str(body).context("manifest is not JSON")?;
    let ann = |k: &str| {
        v.get("annotations")
            .and_then(|a| a.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let label = ann(ANN_VERSION)
        .ok_or_else(|| anyhow!("manifest lacks {ANN_VERSION}"))?
        .to_string();
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        bail!("unexpected release label {label:?}");
    }
    let build: u64 = ann(ANN_BUILD)
        .ok_or_else(|| anyhow!("manifest lacks {ANN_BUILD}"))?
        .parse()
        .context("build number is not a number")?;
    let notes_url = ann(ANN_NOTES)
        .filter(|u| u.starts_with("https://github.com/"))
        .map(str::to_string);
    let layers = v
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    let [layer] = layers.as_slice() else {
        bail!("expected one layer, found {}", layers.len());
    };
    let digest = layer
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("layer has no digest"))?
        .to_string();
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("layer digest is not sha256"))?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("malformed layer digest");
    }
    let size = layer.get("size").and_then(Value::as_u64).unwrap_or(0);
    Ok(Release {
        label,
        build,
        digest: digest.to_ascii_lowercase(),
        size,
        notes_url,
    })
}

/// Offer `r` to a viewer running `installed`, unless the user skipped it.
pub fn is_offer(r: &Release, installed: u64, skipped: Option<u64>) -> bool {
    r.build > installed && skipped != Some(r.build)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "4d2c7f0e8b6a5d3c1f0e9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4";

    fn manifest(build: &str, layers: &str) -> String {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
            "artifactType":"application/vnd.monads.sccm-rc.dmg",
            "config":{{"mediaType":"application/vnd.oci.empty.v1+json","digest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","size":2}},
            "layers":[{layers}],
            "annotations":{{"org.opencontainers.image.version":"0.9.14-m1","ch.monads.sccm-rc.build":"{build}",
              "ch.monads.sccm-rc.release-url":"https://github.com/MonadsAG/sccm-rc-viewer/releases/tag/v0.9.14-m1"}}}}"#
        )
    }

    fn layer() -> String {
        format!(
            r#"{{"mediaType":"application/x-apple-diskimage","digest":"sha256:{HEX}","size":13242898,
            "annotations":{{"org.opencontainers.image.title":"SCCM-RC-0.9.14-m1-macos-universal.dmg"}}}}"#
        )
    }

    #[test]
    fn parses_release_manifest() {
        let r = parse_manifest(&manifest("57", &layer())).unwrap();
        assert_eq!(r.label, "0.9.14-m1");
        assert_eq!(r.build, 57);
        assert_eq!(r.sha256(), HEX);
        assert_eq!(r.size, 13242898);
        assert!(r.notes_url.unwrap().ends_with("v0.9.14-m1"));
    }

    #[test]
    fn rejects_bad_manifests() {
        assert!(parse_manifest("nope").is_err());
        assert!(parse_manifest(&manifest("x", &layer())).is_err());
        assert!(parse_manifest(&manifest("5", "")).is_err());
        assert!(parse_manifest(&manifest("5", &format!("{},{}", layer(), layer()))).is_err());
        let short = layer().replace(HEX, "abc");
        assert!(parse_manifest(&manifest("5", &short)).is_err());
    }

    #[test]
    fn ignores_foreign_notes_url() {
        let m = manifest("5", &layer()).replace("https://github.com/", "https://evil.example/");
        assert_eq!(parse_manifest(&m).unwrap().notes_url, None);
    }

    #[test]
    fn parses_token() {
        assert_eq!(parse_token(r#"{"token":"abc"}"#).unwrap(), "abc");
        assert_eq!(parse_token(r#"{"access_token":"x"}"#).unwrap(), "x");
        assert!(parse_token(r#"{"errors":[]}"#).is_err());
    }

    #[test]
    fn offers_only_newer_unskipped_builds() {
        let r = parse_manifest(&manifest("57", &layer())).unwrap();
        assert!(is_offer(&r, 56, None));
        assert!(!is_offer(&r, 57, None));
        assert!(!is_offer(&r, 99, None));
        assert!(!is_offer(&r, 56, Some(57)));
        assert!(is_offer(&r, 56, Some(55)));
    }

    #[test]
    fn repo_validation() {
        assert!(valid_repo("monadsag/sccm-rc-viewer"));
        assert!(!valid_repo("monadsag"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("Monads/x"));
        assert!(!valid_repo("a/b?x"));
        assert_eq!(
            manifest_url(DEFAULT_REPO),
            "https://ghcr.io/v2/monadsag/sccm-rc-viewer/manifests/latest"
        );
    }
}
