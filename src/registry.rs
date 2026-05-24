//! Project registry — owns the in-memory project list and project sockets.
//!
//! Implements:
//! - FR-007: bind per-project Unix sockets at `<socket_dir>/<name>.sock`
//!   with mode 0660.
//! - FR-008: surface project handles so the server can remove sockets on
//!   `unregister` and shutdown.
//! - FR-010: discover + validate the project manifest at register time.
//! - FR-011: atomic manifest swap on reload (`ArcSwap`), so no in-flight
//!   fetch ever sees a partially-updated allowlist.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::UnixListener;
use tokio::sync::{Notify, RwLock};

use crate::cache::BoundedCache;
use crate::manifest::{Manifest, ManifestError};

const PROJECT_SOCKET_MODE: u32 = 0o660;

/// Errors surfaced by the registry. Mapped to admin wire-protocol error codes
/// by the server's admin dispatch.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("project {0:?} already registered")]
    ProjectExists(String),

    #[error("project {0:?} not registered")]
    ProjectUnknown(String),

    #[error("manifest not found: {0}")]
    ManifestNotFound(#[source] ManifestError),

    #[error("manifest invalid: {0}")]
    ManifestInvalid(#[source] ManifestError),

    #[error("failed to bind project socket {path}: {source}")]
    BindSocket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to set project socket permissions on {path}: {source}")]
    SocketPerms {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A registered project. Cloned (via `Arc<Project>`) into the project's
/// accept loop and connection handlers; mutation goes through `manifest`'s
/// `ArcSwap` (atomic), `cache`'s internal mutex, and `stop` (notify).
#[derive(Debug)]
pub struct Project {
    pub name: String,
    pub project_path: PathBuf,
    pub socket_path: PathBuf,
    pub manifest_path: PathBuf,
    manifest: ArcSwap<Manifest>,
    /// Per-project in-memory secret cache (FR-014/015/016). Dropped — and
    /// therefore zeroized — when the project is unregistered.
    pub cache: BoundedCache,
    /// Notified by the registry/server to ask the project's accept loop to
    /// stop (on `unregister` or daemon shutdown). The accept loop selects on
    /// this and drains its in-flight connections before exiting.
    pub stop: Notify,
}

impl Project {
    /// Atomic snapshot of the current manifest.
    pub fn manifest(&self) -> Arc<Manifest> {
        self.manifest.load_full()
    }
}

/// Daemon-wide defaults for cache bounds; per-project overrides land in
/// the manifest's `[cache]` block.
#[derive(Debug, Clone, Copy)]
pub struct CacheDefaults {
    pub max_entries: u32,
    pub ttl: Duration,
}

pub struct ProjectRegistry {
    socket_dir: PathBuf,
    cache_defaults: CacheDefaults,
    inner: RwLock<HashMap<String, Arc<Project>>>,
}

impl ProjectRegistry {
    pub fn new(socket_dir: PathBuf, cache_defaults: CacheDefaults) -> Self {
        Self {
            socket_dir,
            cache_defaults,
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub fn socket_path_for(&self, name: &str) -> PathBuf {
        self.socket_dir.join(format!("{name}.sock"))
    }

    /// Register a project: load + validate the manifest, bind the socket
    /// (mode 0660), and insert into the map. Returns the project handle and
    /// the bound listener — the caller (server) owns the accept loop.
    pub async fn register(
        &self,
        name: &str,
        project_path: &Path,
    ) -> Result<(Arc<Project>, UnixListener), RegistryError> {
        // Load + validate before we acquire the write lock, so a slow disk
        // doesn't stall concurrent unregisters or reloads on other projects.
        let (manifest, manifest_path) = load_manifest(project_path)?;
        if manifest.project.name != name {
            return Err(RegistryError::ManifestInvalid(
                ManifestError::NameDirMismatch {
                    name: manifest.project.name.clone(),
                    dir: name.to_string(),
                },
            ));
        }

        let socket_path = self.socket_path_for(name);
        let mut guard = self.inner.write().await;
        if guard.contains_key(name) {
            return Err(RegistryError::ProjectExists(name.to_string()));
        }
        let listener = bind_project_socket(&socket_path)?;
        let (cache_max, cache_ttl) = resolve_cache_bounds(&manifest, self.cache_defaults);
        let project = Arc::new(Project {
            name: name.to_string(),
            project_path: project_path.to_path_buf(),
            socket_path: socket_path.clone(),
            manifest_path,
            manifest: ArcSwap::from_pointee(manifest),
            cache: BoundedCache::new(cache_max, cache_ttl),
            stop: Notify::new(),
        });
        guard.insert(name.to_string(), project.clone());
        Ok((project, listener))
    }

    /// Remove a project from the map and return its handle. The caller must
    /// signal `project.stop`, await the accept loop, and remove the socket
    /// file.
    pub async fn unregister(&self, name: &str) -> Result<Arc<Project>, RegistryError> {
        self.inner
            .write()
            .await
            .remove(name)
            .ok_or_else(|| RegistryError::ProjectUnknown(name.to_string()))
    }

    /// Re-read the manifest from disk and atomically swap it in. FR-011
    /// guarantees no fetch sees a partial allowlist: `ArcSwap::store` is the
    /// single observable point of transition. Cache bounds from the new
    /// `[cache]` block are pushed into the existing `BoundedCache` so future
    /// inserts pick them up; current entries keep their original TTL until
    /// they expire or are evicted (matches `BoundedCache::set_config`).
    pub async fn reload(&self, name: &str) -> Result<Arc<Manifest>, RegistryError> {
        let project = {
            let guard = self.inner.read().await;
            guard
                .get(name)
                .cloned()
                .ok_or_else(|| RegistryError::ProjectUnknown(name.to_string()))?
        };
        let (new_manifest, _path) = load_manifest(&project.project_path)?;
        if new_manifest.project.name != name {
            return Err(RegistryError::ManifestInvalid(
                ManifestError::NameDirMismatch {
                    name: new_manifest.project.name.clone(),
                    dir: name.to_string(),
                },
            ));
        }
        let (cache_max, cache_ttl) = resolve_cache_bounds(&new_manifest, self.cache_defaults);
        project.cache.set_config(cache_max, cache_ttl);
        let arc = Arc::new(new_manifest);
        project.manifest.store(arc.clone());
        Ok(arc)
    }

    /// Cloned handles of every currently-registered project. Cheap (one
    /// `Arc::clone` per project); used by status + shutdown paths.
    pub async fn snapshot(&self) -> Vec<Arc<Project>> {
        self.inner.read().await.values().cloned().collect()
    }
}

/// Resolve the (max_entries, ttl) the cache will use for this project.
/// Manifest values override the daemon-wide defaults; absence means
/// "inherit from `cache_defaults`".
fn resolve_cache_bounds(manifest: &Manifest, defaults: CacheDefaults) -> (usize, Duration) {
    let max = manifest.cache.max_entries.unwrap_or(defaults.max_entries) as usize;
    let ttl = manifest
        .cache
        .ttl_seconds
        .map(|s| Duration::from_secs(s as u64))
        .unwrap_or(defaults.ttl);
    (max, ttl)
}

fn load_manifest(project_path: &Path) -> Result<(Manifest, PathBuf), RegistryError> {
    Manifest::load(project_path).map_err(|e| match e {
        ManifestError::NotFound(_) => RegistryError::ManifestNotFound(e),
        _ => RegistryError::ManifestInvalid(e),
    })
}

fn bind_project_socket(path: &Path) -> Result<UnixListener, RegistryError> {
    // FR-009-style stale-file cleanup, same pattern as the admin socket.
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(RegistryError::BindSocket {
            path: path.to_path_buf(),
            source: e,
        });
    }
    let listener = UnixListener::bind(path).map_err(|source| RegistryError::BindSocket {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PROJECT_SOCKET_MODE)).map_err(
        |source| RegistryError::SocketPerms {
            path: path.to_path_buf(),
            source,
        },
    )?;
    Ok(listener)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "remo-broker-test-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
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

    fn defaults() -> CacheDefaults {
        CacheDefaults {
            max_entries: 32,
            ttl: Duration::from_secs(900),
        }
    }

    /// Build a project directory `<root>/<name>` with a valid manifest under
    /// `.remo/broker.toml`. Returns the project root path.
    fn write_project(root: &Path, name: &str, allowlist: &[&str]) -> PathBuf {
        let project_dir = root.join(name);
        let remo_dir = project_dir.join(".remo");
        std::fs::create_dir_all(&remo_dir).unwrap();
        let secrets = allowlist
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!(
            "schema_version = 1\n\n\
             [project]\n\
             name = \"{name}\"\n\n\
             [allowlist]\n\
             secrets = [{secrets}]\n"
        );
        std::fs::write(remo_dir.join("broker.toml"), manifest).unwrap();
        project_dir
    }

    #[tokio::test]
    async fn register_creates_socket_with_0660() {
        let dir = tempdir("register-perms");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir.clone(), defaults());

        let (project, _listener) = registry.register("alpha", &project_dir).await.unwrap();
        assert_eq!(project.name, "alpha");
        assert_eq!(project.socket_path, socket_dir.join("alpha.sock"));
        assert_eq!(
            project.manifest().allowlist.secrets,
            vec!["FOO".to_string()]
        );

        let mode = std::fs::metadata(&project.socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o660, "project socket must be mode 0660");
    }

    #[tokio::test]
    async fn duplicate_register_returns_project_exists() {
        let dir = tempdir("dup");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let _ = registry.register("alpha", &project_dir).await.unwrap();
        let err = registry.register("alpha", &project_dir).await.unwrap_err();
        assert!(
            matches!(err, RegistryError::ProjectExists(ref n) if n == "alpha"),
            "expected ProjectExists, got {err:?}",
        );
    }

    #[tokio::test]
    async fn register_rejects_name_mismatch() {
        let dir = tempdir("mismatch");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let err = registry.register("beta", &project_dir).await.unwrap_err();
        assert!(
            matches!(err, RegistryError::ManifestInvalid(_)),
            "expected ManifestInvalid, got {err:?}",
        );
    }

    #[tokio::test]
    async fn register_missing_manifest_returns_manifest_not_found() {
        let dir = tempdir("missing");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = dir.path().join("alpha");
        std::fs::create_dir_all(&project_dir).unwrap();
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let err = registry.register("alpha", &project_dir).await.unwrap_err();
        assert!(
            matches!(err, RegistryError::ManifestNotFound(_)),
            "expected ManifestNotFound, got {err:?}",
        );
    }

    #[tokio::test]
    async fn unregister_returns_handle_and_removes_from_map() {
        let dir = tempdir("unreg");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let _ = registry.register("alpha", &project_dir).await.unwrap();

        let project = registry.unregister("alpha").await.unwrap();
        assert_eq!(project.name, "alpha");
        assert!(registry.snapshot().await.is_empty());

        // Second unregister fails with ProjectUnknown.
        let err = registry.unregister("alpha").await.unwrap_err();
        assert!(matches!(err, RegistryError::ProjectUnknown(_)));
    }

    #[tokio::test]
    async fn reload_swaps_manifest_atomically() {
        let dir = tempdir("reload");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let (project, _listener) = registry.register("alpha", &project_dir).await.unwrap();
        assert_eq!(
            project.manifest().allowlist.secrets,
            vec!["FOO".to_string()]
        );

        // Rewrite the manifest with a different allowlist; reload.
        let _ = write_project(dir.path(), "alpha", &["FOO", "BAR"]);
        let new_manifest = registry.reload("alpha").await.unwrap();
        assert_eq!(new_manifest.allowlist.secrets, vec!["FOO", "BAR"]);

        // The handle we held from register sees the new manifest via the
        // atomic swap — same Arc<Project>, new contents.
        assert_eq!(
            project.manifest().allowlist.secrets,
            vec!["FOO".to_string(), "BAR".to_string()]
        );
    }

    #[tokio::test]
    async fn reload_unknown_project_returns_project_unknown() {
        let dir = tempdir("reload-unknown");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let err = registry.reload("nope").await.unwrap_err();
        assert!(matches!(err, RegistryError::ProjectUnknown(_)));
    }

    #[tokio::test]
    async fn reload_after_manifest_becomes_invalid_returns_manifest_invalid() {
        let dir = tempdir("reload-invalid");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        let registry = ProjectRegistry::new(socket_dir, defaults());
        let _ = registry.register("alpha", &project_dir).await.unwrap();

        // Truncate the manifest to break TOML parse.
        std::fs::write(project_dir.join(".remo/broker.toml"), b"!!! not toml !!!").unwrap();
        let err = registry.reload("alpha").await.unwrap_err();
        assert!(matches!(err, RegistryError::ManifestInvalid(_)));
    }

    #[tokio::test]
    async fn register_replaces_stale_socket_file() {
        let dir = tempdir("stale");
        let socket_dir = dir.path().join("run");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let project_dir = write_project(dir.path(), "alpha", &["FOO"]);
        // Pre-create a stale file where the bind would land.
        std::fs::write(socket_dir.join("alpha.sock"), b"stale").unwrap();

        let registry = ProjectRegistry::new(socket_dir.clone(), defaults());
        let (project, _listener) = registry.register("alpha", &project_dir).await.unwrap();
        let mode = std::fs::metadata(&project.socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o660);
    }
}
