//! Locally loaded Lens packages. Code/config are pinned on load; each session
//! gets fresh JS workers and private state. This is a trusted plugin runtime,
//! not a sandbox: Node code has the user's OS permissions.

mod extension;
mod management;
mod worker;

pub use extension::{ExtensionAccess, ExtensionManifest, ToolAccess};
pub use management::{LensCatalog, ManagedLensSession};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use smallhead_core::loop_policy::{LoopPolicy, PolicyStack};

/// Current wire/manifest contract. Unknown versions are rejected.
pub const API_VERSION: u32 = 1;

/// A self-contained ES module exports `async apply(context, event, next)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LensManifest {
    /// Stable instance identity, unique in an ordered registry.
    pub id: String,
    /// Human-readable release identifier.
    pub version: String,
    /// Short discovery description, without private configuration.
    #[serde(default)]
    pub description: String,
    /// Must equal [`API_VERSION`].
    pub api_version: u32,
    /// Relative module path contained within this package directory.
    pub entry: PathBuf,
    /// Private JSON configuration, passed as `context.config` to JS.
    #[serde(default)]
    pub config: serde_json::Value,
    /// Optional host-registered tools supplied by a separate local process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<ExtensionManifest>,
}

/// Validated package with immutable source/config, independent of later edits.
#[derive(Clone)]
pub struct LensPackage {
    manifest: LensManifest,
    root: PathBuf,
    manifest_path: PathBuf,
    source: String,
    extension: Option<Arc<extension::ExtensionPackage>>,
}

impl LensPackage {
    /// Read a directory containing `lens.json`, or the manifest file itself.
    /// V1 entry modules are self-contained; relative imports are unsupported.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let manifest_path = if path.is_dir() {
            path.join("lens.json")
        } else {
            path.to_owned()
        }
        .canonicalize()?;
        let root = manifest_path
            .parent()
            .context("Lens manifest needs a parent directory")?
            .to_owned();
        let manifest: LensManifest =
            serde_json::from_str(&read_bounded(&manifest_path, 64 * 1024)?)?;
        if manifest.api_version != API_VERSION {
            bail!("unsupported Lens API version {}", manifest.api_version);
        }
        if manifest.id.is_empty()
            || manifest.id.len() > 128
            || !manifest
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            bail!("Lens id must contain 1..128 ASCII letters, digits, '-', '_' or '.'");
        }
        if manifest.version.trim().is_empty() || manifest.version.len() > 128 {
            bail!("Lens version must contain 1..128 bytes");
        }
        if manifest.entry.is_absolute() {
            bail!("Lens entry must be relative");
        }
        let entry = root.join(&manifest.entry).canonicalize()?;
        if !entry.starts_with(&root) {
            bail!("Lens entry escapes package directory");
        }
        let source = read_bounded(&entry, 1024 * 1024)?;
        let extension = manifest
            .extension
            .as_ref()
            .map(|config| extension::ExtensionPackage::load(&root, config))
            .transpose()?
            .map(Arc::new);
        Ok(Self {
            manifest,
            root,
            manifest_path,
            source,
            extension,
        })
    }

    /// Immutable metadata of the loaded generation.
    pub fn manifest(&self) -> &LensManifest {
        &self.manifest
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Lens file too large: {}", path.display());
    }
    String::from_utf8(bytes).context("Lens files must be UTF-8")
}

/// Ordered packages. Loading, reloading and unloading affect future sessions;
/// existing sessions retain their original generation and order.
#[derive(Clone, Default)]
pub struct LensRegistry {
    packages: Arc<RwLock<Vec<Arc<LensPackage>>>>,
}

impl LensRegistry {
    /// Optional local configuration for headless hosts. The value is a JSON
    /// array of package paths; remote requests never choose executable paths.
    pub fn from_env() -> Result<Self> {
        let registry = Self::default();
        if let Some(paths) = std::env::var_os("SMALLHEAD_LENSES") {
            for path in serde_json::from_str::<Vec<PathBuf>>(&paths.to_string_lossy())? {
                registry.load(path)?;
            }
        }
        Ok(registry)
    }

    /// Append an outer-to-inner layer. Duplicate identities are an error.
    pub fn load(&self, path: impl AsRef<Path>) -> Result<LensManifest> {
        let package = Arc::new(LensPackage::load(path)?);
        let mut packages = self.packages.write();
        if packages
            .iter()
            .any(|p| p.manifest.id == package.manifest.id)
        {
            bail!("Lens {} is already loaded; use reload", package.manifest.id);
        }
        extension::validate_names(packages.iter().chain(std::iter::once(&package)))?;
        let manifest = package.manifest.clone();
        packages.push(package);
        Ok(manifest)
    }

    /// Atomically replace one generation in place after fully validating files.
    pub fn reload(&self, id: &str) -> Result<LensManifest> {
        let old = self
            .packages
            .read()
            .iter()
            .find(|p| p.manifest.id == id)
            .cloned()
            .context("Lens not loaded")?;
        let package = Arc::new(LensPackage::load(&old.manifest_path)?);
        if package.manifest.id != id {
            bail!("reload cannot change Lens id");
        }
        let mut packages = self.packages.write();
        extension::validate_names(
            packages
                .iter()
                .filter(|p| p.manifest.id != id)
                .chain(std::iter::once(&package)),
        )?;
        let slot = packages
            .iter_mut()
            .find(|p| Arc::ptr_eq(p, &old))
            .context("Lens changed during reload; retry")?;
        let manifest = package.manifest.clone();
        *slot = package;
        Ok(manifest)
    }

    /// Remove future admissions; running turns continue with pinned packages.
    pub fn unload(&self, id: &str) -> Result<()> {
        let mut packages = self.packages.write();
        let index = packages
            .iter()
            .position(|p| p.manifest.id == id)
            .context("Lens not loaded")?;
        packages.remove(index);
        Ok(())
    }

    /// Loaded metadata in actual composition order.
    pub fn list(&self) -> Vec<LensManifest> {
        self.packages
            .read()
            .iter()
            .map(|p| p.manifest.clone())
            .collect()
    }

    /// Pin one generation and create isolated policy instances. Workers start
    /// lazily inside the driver's cancellable, timed policy checkpoint.
    pub fn session(&self) -> LensSession {
        let packages = self.packages.read().clone();
        LensSession::from_packages(packages, None)
    }
}

/// Per-turn private JS state. Hosts should call shutdown after dropping the
/// turn future; dropping the final policy handle also kills worker processes.
#[derive(Clone)]
pub struct LensSession {
    packages: Vec<Arc<LensPackage>>,
    workers: Vec<Arc<worker::JsPolicy>>,
    extensions: Vec<Arc<extension::ExtensionClient>>,
}

impl LensSession {
    fn from_packages(packages: Vec<Arc<LensPackage>>, previous: Option<&Self>) -> Self {
        let mut workers = Vec::new();
        let mut extensions = Vec::new();
        for package in &packages {
            let old = previous.and_then(|session| {
                session
                    .packages
                    .iter()
                    .position(|p| Arc::ptr_eq(p, package))
                    .map(|index| (session, index))
            });
            workers.push(match old {
                Some((session, index)) => session.workers[index].clone(),
                None => Arc::new(worker::JsPolicy::new(package.clone())),
            });
            if let Some(extension) = &package.extension {
                let client = match old {
                    Some((session, index)) => {
                        let extension_index = session.packages[..index]
                            .iter()
                            .filter(|p| p.extension.is_some())
                            .count();
                        session.extensions[extension_index].clone()
                    }
                    None => Arc::new(extension::ExtensionClient::new(
                        package.manifest.id.clone(),
                        extension.clone(),
                    )),
                };
                extensions.push(client);
            }
        }
        Self {
            packages,
            workers,
            extensions,
        }
    }

    /// Attach discoverable management tools and permit explicit same-task
    /// capability switches. Other running sessions remain pinned.
    pub fn attach_managed(
        &self,
        agent: &mut smallhead_core::AgentLoop,
        registry: LensRegistry,
        catalog: LensCatalog,
        access: ExtensionAccess,
        allow_paths: bool,
    ) -> Result<ManagedLensSession> {
        ManagedLensSession::attach(self.clone(), agent, registry, catalog, access, allow_paths)
    }

    /// Atomically attach permitted tools and domain prompts to a new loop.
    /// This does not start any external process. The session retains its loaded
    /// generation even if its registry is later reloaded or unloaded.
    pub fn attach(
        &self,
        agent: &mut smallhead_core::AgentLoop,
        access: ExtensionAccess,
    ) -> Result<()> {
        extension::attach(&self.extensions, agent, access)
    }

    /// The same policy interface used by built-in Rust layers.
    pub fn policies(&self) -> PolicyStack {
        PolicyStack::new(
            self.workers
                .iter()
                .map(|p| p.clone() as Arc<dyn LoopPolicy>)
                .collect(),
        )
    }

    /// Stop and reap all workers, including when the turn failed or cancelled.
    pub async fn shutdown(&self) -> Result<()> {
        let mut errors = Vec::new();
        for worker in &self.workers {
            if let Err(error) = worker.shutdown().await {
                errors.push(error.to_string());
            }
        }
        for extension in &self.extensions {
            if let Err(error) = extension.shutdown().await {
                errors.push(error.to_string());
            }
        }
        if !errors.is_empty() {
            bail!("Lens cleanup failed: {}", errors.join("; "));
        }
        Ok(())
    }
}
