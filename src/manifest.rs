//! `remo-broker.toml` parsing and validation.
//!
//! Schema and validation rules are documented in `docs/manifest-schema.md`.
//! This module is the source-of-truth Rust implementation referenced there.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Schema versions this broker accepts. The validation document promises a
/// transition window of at least one minor release whenever this set changes.
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1];

/// Per-project file paths the broker will check, in priority order.
pub const MANIFEST_CANDIDATES: &[&str] = &[".devcontainer/remo-broker.toml", ".remo/broker.toml"];

const PROJECT_NAME_MAX_LEN: usize = 64;
const DESCRIPTION_MAX_LEN: usize = 256;
const SECRET_NAME_MAX_LEN: usize = 128;
const CACHE_TTL_MAX: u32 = 86_400;
const CACHE_MAX_ENTRIES_MAX: u32 = 1_024;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("no manifest under {0}: expected .devcontainer/remo-broker.toml or .remo/broker.toml")]
    NotFound(PathBuf),

    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("unsupported schema_version: {0} (supported: {SUPPORTED_SCHEMA_VERSIONS:?})")]
    UnsupportedSchemaVersion(u32),

    #[error("invalid project.name {name:?}: must match ^[a-z0-9][a-z0-9_-]{{0,63}}$")]
    InvalidProjectName { name: String },

    #[error("project.name {name:?} does not match project directory basename {dir:?}")]
    NameDirMismatch { name: String, dir: String },

    #[error("project.description too long: {len} chars (max {max})", max = DESCRIPTION_MAX_LEN)]
    DescriptionTooLong { len: usize },

    #[error("invalid allowlist secret name {name:?}: must match ^[A-Za-z0-9_]{{1,128}}$")]
    InvalidSecretName { name: String },

    #[error("duplicate secret name in allowlist: {0:?}")]
    DuplicateSecret(String),

    #[error("cache.ttl_seconds out of range: {value} (must be 1..={max})", max = CACHE_TTL_MAX)]
    CacheTtlOutOfRange { value: u32 },

    #[error(
        "cache.max_entries out of range: {value} (must be 1..={max})",
        max = CACHE_MAX_ENTRIES_MAX
    )]
    CacheMaxEntriesOutOfRange { value: u32 },
}

/// Validated manifest. Never constructed except via [`Manifest::from_toml_str`]
/// or [`Manifest::load`].
#[derive(Debug, Clone)]
pub struct Manifest {
    pub schema_version: u32,
    pub project: Project,
    pub allowlist: Allowlist,
    pub cache: Cache,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Allowlist {
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Cache {
    pub ttl_seconds: Option<u32>,
    pub max_entries: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema_version: u32,
    project: RawProject,
    allowlist: RawAllowlist,
    #[serde(default)]
    cache: Option<RawCache>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    name: String,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAllowlist {
    secrets: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCache {
    ttl_seconds: Option<u32>,
    max_entries: Option<u32>,
}

impl Manifest {
    /// Parse and validate a manifest from a TOML string.
    ///
    /// `project_dir_name` is the basename of the project directory, used to
    /// enforce validation rule #4 ("project.name matches directory basename").
    /// Pass `None` to skip that check (useful in tests and when callers have
    /// no directory context).
    pub fn from_toml_str(
        toml_src: &str,
        project_dir_name: Option<&str>,
    ) -> Result<Self, ManifestError> {
        let raw: RawManifest = toml::from_str(toml_src).map_err(|source| ManifestError::Parse {
            path: PathBuf::from("<inline>"),
            source,
        })?;
        Self::validate(raw, project_dir_name)
    }

    /// Discover, read, and validate the manifest for `project_path`.
    ///
    /// Tries [`MANIFEST_CANDIDATES`] in order. Returns the validated manifest
    /// and the path that was loaded.
    pub fn load(project_path: &Path) -> Result<(Self, PathBuf), ManifestError> {
        let path = discover(project_path)
            .ok_or_else(|| ManifestError::NotFound(project_path.to_path_buf()))?;
        let src = std::fs::read_to_string(&path).map_err(|source| ManifestError::Io {
            path: path.clone(),
            source,
        })?;
        let raw: RawManifest = toml::from_str(&src).map_err(|source| ManifestError::Parse {
            path: path.clone(),
            source,
        })?;
        let dir_name = project_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_owned);
        let manifest = Self::validate(raw, dir_name.as_deref())?;
        Ok((manifest, path))
    }

    fn validate(raw: RawManifest, project_dir_name: Option<&str>) -> Result<Self, ManifestError> {
        if !SUPPORTED_SCHEMA_VERSIONS.contains(&raw.schema_version) {
            return Err(ManifestError::UnsupportedSchemaVersion(raw.schema_version));
        }

        if !is_valid_project_name(&raw.project.name) {
            return Err(ManifestError::InvalidProjectName {
                name: raw.project.name,
            });
        }

        if let Some(dir) = project_dir_name
            && raw.project.name != dir
        {
            return Err(ManifestError::NameDirMismatch {
                name: raw.project.name,
                dir: dir.to_owned(),
            });
        }

        if let Some(desc) = &raw.project.description
            && desc.chars().count() > DESCRIPTION_MAX_LEN
        {
            return Err(ManifestError::DescriptionTooLong {
                len: desc.chars().count(),
            });
        }

        let mut seen = std::collections::HashSet::with_capacity(raw.allowlist.secrets.len());
        for name in &raw.allowlist.secrets {
            if !is_valid_secret_name(name) {
                return Err(ManifestError::InvalidSecretName { name: name.clone() });
            }
            if !seen.insert(name.as_str()) {
                return Err(ManifestError::DuplicateSecret(name.clone()));
            }
        }

        let cache = match raw.cache {
            None => Cache::default(),
            Some(c) => {
                if let Some(ttl) = c.ttl_seconds
                    && !(1..=CACHE_TTL_MAX).contains(&ttl)
                {
                    return Err(ManifestError::CacheTtlOutOfRange { value: ttl });
                }
                if let Some(max) = c.max_entries
                    && !(1..=CACHE_MAX_ENTRIES_MAX).contains(&max)
                {
                    return Err(ManifestError::CacheMaxEntriesOutOfRange { value: max });
                }
                Cache {
                    ttl_seconds: c.ttl_seconds,
                    max_entries: c.max_entries,
                }
            }
        };

        Ok(Manifest {
            schema_version: raw.schema_version,
            project: Project {
                name: raw.project.name,
                description: raw.project.description,
            },
            allowlist: Allowlist {
                secrets: raw.allowlist.secrets,
            },
            cache,
        })
    }
}

fn discover(project_path: &Path) -> Option<PathBuf> {
    MANIFEST_CANDIDATES
        .iter()
        .map(|rel| project_path.join(rel))
        .find(|p| p.is_file())
}

fn is_valid_project_name(s: &str) -> bool {
    // ^[a-z0-9][a-z0-9_-]{0,63}$
    if s.is_empty() || s.len() > PROJECT_NAME_MAX_LEN {
        return false;
    }
    let mut bytes = s.bytes();
    let first = bytes.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn is_valid_secret_name(s: &str) -> bool {
    // ^[A-Za-z0-9_]{1,128}$
    if s.is_empty() || s.len() > SECRET_NAME_MAX_LEN {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
schema_version = 1
[project]
name = "myrepo"
[allowlist]
secrets = ["GITHUB_TOKEN"]
"#;

    #[test]
    fn parses_minimal_manifest() {
        let m = Manifest::from_toml_str(MINIMAL, Some("myrepo")).unwrap();
        assert_eq!(m.schema_version, 1);
        assert_eq!(m.project.name, "myrepo");
        assert_eq!(m.allowlist.secrets, vec!["GITHUB_TOKEN"]);
        assert_eq!(m.cache.ttl_seconds, None);
        assert_eq!(m.cache.max_entries, None);
    }

    #[test]
    fn parses_full_manifest() {
        let src = r#"
schema_version = 1
[project]
name = "internal-tool"
description = "Has a description."
[allowlist]
secrets = ["GITHUB_TOKEN", "NPM_TOKEN", "ANTHROPIC_API_KEY"]
[cache]
ttl_seconds = 600
max_entries = 16
"#;
        let m = Manifest::from_toml_str(src, Some("internal-tool")).unwrap();
        assert_eq!(m.project.description.as_deref(), Some("Has a description."));
        assert_eq!(m.allowlist.secrets.len(), 3);
        assert_eq!(m.cache.ttl_seconds, Some(600));
        assert_eq!(m.cache.max_entries, Some(16));
    }

    #[test]
    fn empty_allowlist_is_valid() {
        let src = r#"
schema_version = 1
[project]
name = "myrepo"
[allowlist]
secrets = []
"#;
        let m = Manifest::from_toml_str(src, Some("myrepo")).unwrap();
        assert!(m.allowlist.secrets.is_empty());
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let src = MINIMAL.replace("schema_version = 1", "schema_version = 2");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::UnsupportedSchemaVersion(2)));
    }

    #[test]
    fn rejects_invalid_project_name_uppercase() {
        let src = MINIMAL.replace(r#"name = "myrepo""#, r#"name = "MyRepo""#);
        let err = Manifest::from_toml_str(&src, None).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidProjectName { .. }));
    }

    #[test]
    fn rejects_invalid_project_name_leading_dash() {
        let src = MINIMAL.replace(r#"name = "myrepo""#, r#"name = "-foo""#);
        let err = Manifest::from_toml_str(&src, None).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidProjectName { .. }));
    }

    #[test]
    fn rejects_invalid_project_name_too_long() {
        let long = "a".repeat(65);
        let src = MINIMAL.replace(r#"name = "myrepo""#, &format!(r#"name = "{long}""#));
        let err = Manifest::from_toml_str(&src, None).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidProjectName { .. }));
    }

    #[test]
    fn accepts_project_name_at_max_length() {
        let long = "a".repeat(64);
        let src = MINIMAL.replace(r#"name = "myrepo""#, &format!(r#"name = "{long}""#));
        let m = Manifest::from_toml_str(&src, Some(&long)).unwrap();
        assert_eq!(m.project.name.len(), 64);
    }

    #[test]
    fn rejects_name_dir_mismatch() {
        let err = Manifest::from_toml_str(MINIMAL, Some("other")).unwrap_err();
        assert!(matches!(err, ManifestError::NameDirMismatch { .. }));
    }

    #[test]
    fn rejects_invalid_secret_name_hyphen() {
        let src = MINIMAL.replace(r#"["GITHUB_TOKEN"]"#, r#"["NOT-OK"]"#);
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidSecretName { .. }));
    }

    #[test]
    fn rejects_invalid_secret_name_too_long() {
        let long = "A".repeat(129);
        let src = MINIMAL.replace(r#"["GITHUB_TOKEN"]"#, &format!(r#"["{long}"]"#));
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidSecretName { .. }));
    }

    #[test]
    fn rejects_duplicate_secrets() {
        let src = MINIMAL.replace(r#"["GITHUB_TOKEN"]"#, r#"["FOO", "FOO"]"#);
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateSecret(ref n) if n == "FOO"));
    }

    #[test]
    fn rejects_cache_ttl_out_of_range_zero() {
        let src = format!("{MINIMAL}\n[cache]\nttl_seconds = 0\n");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::CacheTtlOutOfRange { value: 0 }
        ));
    }

    #[test]
    fn rejects_cache_ttl_out_of_range_high() {
        let src = format!("{MINIMAL}\n[cache]\nttl_seconds = 86401\n");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::CacheTtlOutOfRange { value: 86401 }
        ));
    }

    #[test]
    fn rejects_cache_max_entries_out_of_range() {
        let src = format!("{MINIMAL}\n[cache]\nmax_entries = 2048\n");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::CacheMaxEntriesOutOfRange { value: 2048 }
        ));
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let src = format!("{MINIMAL}\n[extra]\nfoo = 1\n");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::Parse { .. }));
    }

    #[test]
    fn rejects_unknown_key_in_known_table() {
        let src = MINIMAL.replace("[project]\nname", "[project]\nbogus = true\nname");
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::Parse { .. }));
    }

    #[test]
    fn rejects_description_too_long() {
        let huge = "x".repeat(257);
        let src = MINIMAL.replace(
            "[project]\nname = \"myrepo\"",
            &format!("[project]\nname = \"myrepo\"\ndescription = \"{huge}\""),
        );
        let err = Manifest::from_toml_str(&src, Some("myrepo")).unwrap_err();
        assert!(matches!(err, ManifestError::DescriptionTooLong { .. }));
    }

    #[test]
    fn load_discovers_devcontainer_path_first() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".devcontainer")).unwrap();
        std::fs::create_dir_all(dir.path().join(".remo")).unwrap();
        let project_name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let dc_manifest = MINIMAL.replace("myrepo", &project_name);
        let remo_manifest = MINIMAL
            .replace("myrepo", &project_name)
            .replace(r#"["GITHUB_TOKEN"]"#, r#"["NPM_TOKEN"]"#);
        std::fs::write(
            dir.path().join(".devcontainer/remo-broker.toml"),
            dc_manifest,
        )
        .unwrap();
        std::fs::write(dir.path().join(".remo/broker.toml"), remo_manifest).unwrap();

        let (m, path) = Manifest::load(dir.path()).unwrap();
        assert!(path.ends_with(".devcontainer/remo-broker.toml"));
        assert_eq!(m.allowlist.secrets, vec!["GITHUB_TOKEN"]);
    }

    #[test]
    fn load_falls_back_to_remo_path() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.path().join(".remo")).unwrap();
        let project_name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let src = MINIMAL.replace("myrepo", &project_name);
        std::fs::write(dir.path().join(".remo/broker.toml"), src).unwrap();

        let (_m, path) = Manifest::load(dir.path()).unwrap();
        assert!(path.ends_with(".remo/broker.toml"));
    }

    #[test]
    fn load_errors_when_no_manifest_present() {
        let dir = tempdir();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(matches!(err, ManifestError::NotFound(_)));
    }

    /// Minimal tempdir helper. Uses the system tempdir, returns an RAII guard
    /// that removes the directory on drop. Avoids pulling in the `tempfile`
    /// crate just for these tests.
    fn tempdir() -> TempDir {
        let base = std::env::temp_dir();
        let unique = format!(
            "remo-broker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = base.join(unique);
        std::fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
