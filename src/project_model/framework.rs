//! Framework channel resolution and persisted dependency selection.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::project::Project;
use cargo_lock::{Dependency as LockedDependency, Lockfile};
use cargo_toml::{Dependency, DependencyDetail, PatchSet};
use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smol::process::Command;
use zenwave::{Client as _, Method, StatusCode};

/// A framework distribution channel, independent of the Rust toolchain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameworkChannel {
    /// The integration branch, resolved to an exact compilation-checked
    /// commit; the Apple backend's `dev` HEAD resolves the same way.
    Dev,
    /// An immutable revision certified by the complete nightly suite,
    /// including the backend pin the suite's certification records.
    Nightly,
    /// Published packages and the compatible native backends bundled with the CLI.
    #[default]
    Stable,
}

impl fmt::Display for FrameworkChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Dev => "dev",
            Self::Nightly => "nightly",
            Self::Stable => "stable",
        })
    }
}

impl FromStr for FrameworkChannel {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "dev" => Ok(Self::Dev),
            "nightly" => Ok(Self::Nightly),
            "stable" => Ok(Self::Stable),
            _ => Err("framework channel must be dev, nightly or stable".into()),
        }
    }
}

/// The GitHub release a certified channel resolved from — the provenance the
/// persisted selection keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrameworkRelease {
    /// The repository the release lives in.
    repository: String,
    /// The commit the release certifies.
    revision: String,
    /// The release tag the manifest rode in on.
    tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "lowercase")]
enum Source {
    /// A published framework release whose `framework.json` resolved every
    /// scaffold pin.
    Stable {
        /// The release the selection resolved from — absent in manifests
        /// written before the stable channel carried a manifest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<FrameworkRelease>,
    },
    Dev {
        repository: String,
        revision: String,
        lock_sha256: String,
    },
    Nightly {
        repository: String,
        revision: String,
        tag: String,
        lock_sha256: String,
    },
    /// A local checkout reached through `waterui_path`: a filesystem source,
    /// not a channel. Never persisted — `waterui_path` itself is the record.
    #[serde(skip)]
    Local { root: PathBuf },
}

/// The persisted framework and backend selection used without channel re-resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedFramework {
    #[serde(flatten)]
    source: Source,
    #[serde(
        default,
        rename = "minimum-cli-version",
        skip_serializing_if = "Option::is_none"
    )]
    minimum_cli_version: Option<cargo_toml::SemVer>,
    /// The framework's `[package.metadata.waterui]` table at the selected
    /// revision, carried verbatim from its manifest.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    metadata: toml::Table,
    scaffold: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    packages: BTreeMap<String, DependencyDetail>,
    #[serde(default, skip_serializing_if = "PatchSet::is_empty")]
    patches: PatchSet,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
}

impl From<&cargo_lock::Package> for LockedPackage {
    fn from(package: &cargo_lock::Package) -> Self {
        Self {
            name: package.name.to_string(),
            version: package.version.to_string(),
            source: package.source.as_ref().map(ToString::to_string),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct Certification {
    schema_version: u32,
    channel: FrameworkChannel,
    repository: String,
    revision: String,
    tag: String,
    lockfiles: BTreeMap<String, String>,
    /// Submodule path -> commit the certification recorded for the revision.
    #[serde(default)]
    submodules: BTreeMap<String, String>,
    scaffold: BTreeMap<String, String>,
    /// The framework's `[package.metadata.waterui]` table, verbatim — the CLI
    /// floor and every future framework-owned fact ride inside it.
    metadata: toml::Table,
}

/// Make `document`'s `[patch]` tables carry `patches` in place of `previous`:
/// the entries of `previous` are removed, those of `patches` written, and
/// sources left empty are dropped, so a manifest moving between a channel, a
/// local checkout and the registry never keeps a stale override.
pub(crate) fn rewrite_patch_tables(
    document: &mut toml_edit::DocumentMut,
    previous: &PatchSet,
    patches: &PatchSet,
) -> Result<()> {
    for (source, dependencies) in previous {
        if let Some(table) = document
            .get_mut("patch")
            .and_then(|patch| patch.get_mut(source))
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for name in dependencies.keys() {
                table.remove(name);
            }
        }
    }
    let patches = toml_edit::ser::to_document(patches)?;
    for (source, dependencies) in patches.iter() {
        // `[patch]` and `[patch.<source>]` are written as explicit tables:
        // indexing into a missing key would vivify an inline value and
        // hoist `patch = { … }` above `[package]`.
        let patch = document
            .entry("patch")
            .or_insert_with(toml_edit::table)
            .as_table_mut()
            .ok_or_else(|| eyre!("[patch] is not a table"))?;
        patch.set_implicit(true);
        let table = patch
            .entry(source)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
            .ok_or_else(|| eyre!("[patch.{source}] is not a table"))?;
        for (name, dependency) in dependencies
            .as_table_like()
            .expect("serialized patch dependencies are tables")
            .iter()
        {
            table.insert(name, dependency.clone());
        }
    }
    if let Some(patch) = document
        .get_mut("patch")
        .and_then(toml_edit::Item::as_table_mut)
    {
        let empty: Vec<String> = patch
            .iter()
            .filter(|(_, sources)| {
                sources
                    .as_table_like()
                    .is_some_and(toml_edit::TableLike::is_empty)
            })
            .map(|(source, _)| source.to_owned())
            .collect();
        for source in empty {
            patch.remove(&source);
        }
        if patch.is_empty() {
            document.remove("patch");
        }
    }
    Ok(())
}

impl ResolvedFramework {
    /// The selected distribution channel — `None` for a local checkout, which
    /// is a filesystem source rather than a channel.
    #[must_use]
    pub const fn channel(&self) -> Option<FrameworkChannel> {
        match self.source {
            Source::Stable { .. } => Some(FrameworkChannel::Stable),
            Source::Dev { .. } => Some(FrameworkChannel::Dev),
            Source::Nightly { .. } => Some(FrameworkChannel::Nightly),
            Source::Local { .. } => None,
        }
    }

    /// The framework a manifest resolves its generated code against: the
    /// channel selection `framework` records, or the checkout `waterui_path`
    /// names. A manifest carrying neither has no framework to resolve — an
    /// explicit channel selection creates the record.
    ///
    /// # Errors
    /// Returns an error when the manifest records no framework source, or the
    /// local checkout's framework facts cannot be read.
    pub(crate) async fn for_manifest(
        manifest: &crate::project::Manifest,
        project_root: &Path,
    ) -> Result<Self> {
        if let Some(framework) = &manifest.framework {
            return framework.clone().validated().wrap_err(
                "the recorded framework selection predates a metadata key this CLI \
                 requires; re-run `water channel` to resolve it again",
            );
        }
        let Some(waterui_path) = &manifest.waterui_path else {
            bail!(
                "the project records no framework selection; run `water channel` \
                 to select one or point `waterui_path` at a checkout"
            );
        };
        Self::for_local_checkout(&project_root.join(waterui_path)).await
    }

    /// The framework facts a local checkout supplies: its own
    /// `[package.metadata.waterui]` table, the scaffold requirements its
    /// `[workspace.dependencies]` declares, and the backend/workspace pins its
    /// gitlinks and lockfile record.
    pub(crate) async fn for_local_checkout(root: &Path) -> Result<Self> {
        let manifest: toml::Value = toml::from_str(
            &smol::fs::read_to_string(root.join("Cargo.toml"))
                .await
                .wrap_err_with(|| {
                    format!(
                        "the WaterUI checkout at {} has no Cargo.toml",
                        root.display()
                    )
                })?,
        )?;
        let metadata = framework_metadata(&manifest)?;
        let minimum_cli_version = minimum_cli_version(&metadata)?;
        if let Some(minimum) = &minimum_cli_version {
            validate_installed_cli(minimum, &checkout_cli_update())?;
        }
        let mut scaffold = framework_scaffold(&manifest)?;
        let lock: Lockfile = smol::fs::read_to_string(root.join("Cargo.lock"))
            .await
            .wrap_err_with(|| {
                format!(
                    "the WaterUI checkout at {} has no Cargo.lock",
                    root.display()
                )
            })?
            .parse()?;
        let mut submodules = BTreeMap::new();
        // A checkout from before a backend's revision was declared in the
        // manifest still carries its gitlink; a manifest declaring
        // `{name}-backend-revision` has none to read.
        for path in BACKEND_SUBMODULES {
            if declares_backend_revision(&scaffold, path) {
                continue;
            }
            submodules.insert(
                (*path).to_owned(),
                local_submodule_revision(root, path).await?,
            );
        }
        // A checkout from before the Apple backend left the tree still carries
        // its `backends/apple` gitlink. A declared version or revision supplies
        // the pin without a gitlink.
        if !declares_apple_backend_pin(&scaffold) {
            submodules.insert(
                "backends/apple".to_owned(),
                local_submodule_revision(root, "backends/apple").await?,
            );
        }
        complete_scaffold(&mut scaffold, &submodules, &lock)?;
        Self {
            source: Source::Local {
                root: root.to_path_buf(),
            },
            minimum_cli_version,
            metadata,
            scaffold,
            packages: BTreeMap::new(),
            patches: PatchSet::default(),
        }
        .validated()
    }

    /// Hold the framework to the metadata keys the CLI reads later without a
    /// `Result` in hand — the scaffold's `minSdk` above all. A framework that
    /// reaches a template context has passed here, so a template accessor
    /// failing on it is an internal invariant, not an input error.
    fn validated(self) -> Result<Self> {
        self.android_min_api_level()?;
        Ok(self)
    }

    /// Resolve a certified framework manifest (`framework.json`) from disk —
    /// the `--framework-manifest` source that pins a project to the channel
    /// and revision it declares. The file is verified exactly as a manifest
    /// downloaded from its release is.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or parsed, fails
    /// verification, or the revision it certifies cannot be fetched.
    pub(crate) async fn resolve_manifest(path: &Path) -> Result<(Self, Option<Vec<u8>>)> {
        let repository = framework_repository();
        let slug = repository_slug(repository)?;
        let certification = load_manifest(path, repository).await?;
        let revision = certification.revision.clone();
        Self::construct(repository, slug, &revision, Some(certification)).await
    }

    pub(crate) fn validate_cli(&self) -> Result<()> {
        if let Some(minimum) = &self.minimum_cli_version {
            let update = match &self.source {
                Source::Stable { .. } => registry_cli_update(minimum),
                Source::Dev { .. } | Source::Nightly { .. } | Source::Local { .. } => {
                    checkout_cli_update()
                }
            };
            validate_installed_cli(minimum, &update)?;
        }
        Ok(())
    }

    pub(crate) fn scaffold_value(&self, key: &str) -> &str {
        self.scaffold
            .get(key)
            .unwrap_or_else(|| panic!("resolved framework carries no `{key}` scaffold metadata"))
    }

    /// The Apple backend release a scaffolded project pins, when the
    /// framework declares one; a framework older than the submodule's
    /// removal pins `apple-backend-revision` — a gitlink commit — instead.
    pub(crate) fn apple_backend_version(&self) -> Option<&str> {
        self.scaffold
            .get("apple-backend-version")
            .map(String::as_str)
    }

    /// The Apple backend commit a `dev` or `nightly` selection pins — the
    /// backend's `dev` HEAD `dev` resolved at selection time, or the
    /// revision a certification records — and the gitlink pin a framework
    /// from before the backend's extraction carries on every channel.
    pub(crate) fn apple_backend_revision(&self) -> Option<&str> {
        self.scaffold
            .get("apple-backend-revision")
            .map(String::as_str)
    }

    /// The Android API floor the selected framework's native runtime
    /// supports — the `android-min-api-level` its
    /// `[package.metadata.waterui]` table declares. The backend's Gradle
    /// `minSdk` declares the same floor independently; CI holds the two to
    /// agreement.
    ///
    /// # Errors
    /// Returns an error when the resolved framework's metadata does not
    /// declare a valid `android-min-api-level` integer.
    pub(crate) fn android_min_api_level(&self) -> Result<u32> {
        const KEY: &str = "package.metadata.waterui.android-min-api-level";
        let origin = match &self.source {
            Source::Stable { release } => release.as_ref().map_or_else(
                || "the stable framework manifest".to_owned(),
                |release| format!("the framework manifest certified by {}", release.tag),
            ),
            Source::Dev {
                repository,
                revision,
                ..
            }
            | Source::Nightly {
                repository,
                revision,
                ..
            } => format!("the framework manifest at {repository}@{revision}"),
            Source::Local { root } => format!("{}", root.join("Cargo.toml").display()),
        };
        let value = self
            .metadata
            .get("android-min-api-level")
            .ok_or_else(|| eyre!("{origin} does not declare {KEY}"))?;
        value
            .as_integer()
            .and_then(|level| u32::try_from(level).ok())
            .ok_or_else(|| eyre!("{origin} declares an invalid {KEY}: {value}"))
    }

    pub(crate) fn patches(&self) -> PatchSet {
        self.patches.clone()
    }

    /// Rewrite a project manifest's dependencies and `[patch]` tables for this
    /// framework, clearing `previous_patches` first: the entries the manifest
    /// carried for whatever it was built against before, a channel's or a
    /// local checkout's.
    pub(crate) fn update_manifest(
        &self,
        document: &mut toml_edit::DocumentMut,
        previous_patches: &PatchSet,
    ) -> Result<()> {
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = document
                .get_mut(section)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                self.update_dependencies(dependencies)?;
            }
        }
        if let Some(targets) = document
            .get_mut("target")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for (_, target) in targets.iter_mut() {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(dependencies) = target
                        .get_mut(section)
                        .and_then(toml_edit::Item::as_table_like_mut)
                    {
                        self.update_dependencies(dependencies)?;
                    }
                }
            }
        }
        rewrite_patch_tables(document, previous_patches, &self.patches)
    }

    fn update_dependencies(&self, dependencies: &mut dyn toml_edit::TableLike) -> Result<()> {
        for (name, dependency) in dependencies.iter_mut() {
            let package = dependency
                .get("package")
                .and_then(toml_edit::Item::as_str)
                .unwrap_or(&name)
                .to_owned();
            if !self.scaffold.contains_key(&format!("{package}-version")) {
                continue;
            }
            if dependency.is_str() {
                let decor = dependency
                    .as_value()
                    .expect("string dependency")
                    .decor()
                    .clone();
                let mut value = toml_edit::Value::InlineTable(toml_edit::InlineTable::new());
                *value.decor_mut() = decor;
                *dependency = toml_edit::Item::Value(value);
            }
            let table = dependency
                .as_table_like_mut()
                .ok_or_else(|| eyre!("invalid dependency {name}"))?;
            if table.contains_key("workspace") {
                bail!(
                    "{name} inherits its source; select the framework at its Cargo workspace root"
                );
            }
            for key in [
                "version",
                "git",
                "rev",
                "branch",
                "tag",
                "path",
                "registry",
                "registry-index",
            ] {
                table.remove(key);
            }
            let source = toml_edit::ser::to_document(&self.dependency(&package))?;
            for key in ["version", "git", "rev"] {
                if let Some(value) = source.get(key) {
                    table.insert(key, value.clone());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_dependencies(
        &self,
        metadata: &cargo_metadata::Metadata,
        contents: &[u8],
    ) -> Result<()> {
        let source = match &self.source {
            Source::Stable { .. } | Source::Local { .. } => return Ok(()),
            Source::Dev {
                repository,
                revision,
                ..
            }
            | Source::Nightly {
                repository,
                revision,
                ..
            } => format!("git+{repository}?rev={revision}#{revision}"),
        };
        let locked = self.cargo_lock(contents)?;
        let allowed = self.allowed_packages(&locked.packages);
        // The names the framework contract knows: `Water.lock`'s packages and
        // the scaffold's extracted crates. Anything else — an extracted
        // crate's private dependencies, which can never enter `Water.lock` —
        // is foreign to the check.
        let ecosystem: BTreeSet<&str> = locked
            .packages
            .iter()
            .map(|package| package.name.as_str())
            .chain(self.packages.keys().map(String::as_str))
            .collect();
        let packages: BTreeMap<_, _> = metadata
            .packages
            .iter()
            .map(|package| (package.id.clone(), package))
            .collect();
        let resolve = metadata
            .resolve
            .as_ref()
            .ok_or_else(|| eyre!("framework verification requires a resolved Cargo graph"))?;
        let nodes: BTreeMap<_, _> = resolve
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node))
            .collect();
        let is_framework_source = |package: &cargo_metadata::Package| {
            package
                .source
                .as_ref()
                .is_some_and(|candidate| candidate.repr == source)
        };
        if !metadata.packages.iter().any(is_framework_source) {
            bail!("the project does not resolve its selected framework revision");
        }
        let mut pending: Vec<_> = metadata
            .packages
            .iter()
            .filter(|package| {
                is_framework_source(package) || self.packages.contains_key(package.name.as_str())
            })
            .map(|package| package.id.clone())
            .collect();
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let package = packages[&id];
            let identity = LockedPackage {
                name: package.name.to_string(),
                version: package.version.to_string(),
                source: package.source.as_ref().map(|source| source.repr.clone()),
            };
            if !allowed.contains(&identity)
                && ecosystem.contains(identity.name.as_str())
                && !self.sanctioned_source(&identity)
            {
                bail!(
                    "framework dependency {} {} differs from Water.lock; select a compatible channel explicitly",
                    identity.name,
                    identity.version
                );
            }
            pending.extend(nodes[&id].dependencies.iter().cloned());
        }
        Ok(())
    }

    pub(crate) async fn prepare_build(
        &self,
        project: &Project,
        directory: &std::path::Path,
        features: &[String],
    ) -> Result<()> {
        self.validate_cli()?;
        let project_lock: Lockfile = smol::fs::read_to_string(project.lockfile_path().await?)
            .await?
            .parse()?;
        let lock_path = directory.join("Cargo.lock");
        let previous: Option<Lockfile> = match smol::fs::read_to_string(&lock_path).await {
            Ok(contents) => Some(contents.parse()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let allow_new = previous.as_ref().is_none_or(|previous| {
            let previous: BTreeSet<_> = previous
                .packages
                .iter()
                .map(LockedDependency::from)
                .collect();
            project_lock
                .packages
                .iter()
                .filter(|package| package.name.as_str() == "waterui")
                .any(|package| !previous.contains(&LockedDependency::from(package)))
        });
        let canonical = if self.channel() == Some(FrameworkChannel::Stable) {
            None
        } else {
            Some(smol::fs::read(project.root().join("Water.lock")).await?)
        };
        let mut packages = BTreeMap::new();
        if let Some(previous) = &previous {
            packages.extend(
                previous
                    .packages
                    .iter()
                    .map(|package| (LockedDependency::from(package), package.clone())),
            );
        }
        if let Some(canonical) = &canonical {
            packages.extend(
                self.cargo_lock(canonical)?
                    .packages
                    .into_iter()
                    .map(|package| (LockedDependency::from(&package), package)),
            );
        }
        packages.extend(
            project_lock
                .packages
                .iter()
                .map(|package| (LockedDependency::from(package), package.clone())),
        );
        let allowed = self.allowed_packages(packages.values());
        let mut seed = project_lock;
        seed.packages = packages.into_values().collect();
        smol::fs::write(&lock_path, seed.to_string()).await?;
        let root = directory.to_path_buf();
        let features = features.to_vec();
        let result = async {
            let metadata = smol::unblock(move || {
                cargo_metadata::MetadataCommand::new().current_dir(root)
                    .features(cargo_metadata::CargoOpt::SomeFeatures(features)).exec()
            }).await?;
            validate_resolved_cli(&metadata)?;
            if !allow_new {
                for package in &metadata.packages {
                    if package.source.is_some() && !allowed.contains(&LockedPackage {
                        name: package.name.to_string(),
                        version: package.version.to_string(),
                        source: package.source.as_ref().map(|source| source.repr.clone()),
                    }) {
                        bail!("generated build would change locked dependency {}; update the framework channel explicitly", package.name);
                    }
                }
            }
            if let Some(canonical) = canonical {
                self.validate_dependencies(&metadata, &canonical)?;
            }
            Ok(())
        }.await;
        if let Err(error) = &result {
            let restore = if let Some(previous) = previous {
                smol::fs::write(&lock_path, previous.to_string()).await
            } else {
                smol::fs::remove_file(&lock_path).await
            };
            restore.wrap_err_with(|| {
                format!("failed to preserve the previous lock after resolution failed: {error}")
            })?;
        }
        result
    }

    pub(crate) fn cargo_lock(&self, contents: &[u8]) -> Result<Lockfile> {
        let (repository, revision, expected) = match &self.source {
            Source::Stable { .. } => bail!("stable uses the application's Cargo.lock"),
            Source::Local { .. } => {
                bail!("a local framework checkout has no canonical lock")
            }
            Source::Dev {
                repository,
                revision,
                lock_sha256,
            }
            | Source::Nightly {
                repository,
                revision,
                lock_sha256,
                ..
            } => (repository, revision, lock_sha256),
        };
        if hex::encode(Sha256::digest(contents)) != *expected {
            bail!("Water.lock does not match the selected framework revision");
        }
        let mut lock: Lockfile = std::str::from_utf8(contents)?.parse()?;
        let source = format!("git+{repository}?rev={revision}#{revision}")
            .parse::<cargo_lock::SourceId>()?;
        let mut local = BTreeMap::new();
        for package in &mut lock.packages {
            if package.source.is_none() {
                package.source = Some(source.clone());
                local.insert(
                    (package.name.clone(), package.version.clone()),
                    LockedDependency::from(&*package),
                );
            }
        }
        for package in &mut lock.packages {
            for dependency in &mut package.dependencies {
                if dependency.source.is_none()
                    && let Some(replacement) =
                        local.get(&(dependency.name.clone(), dependency.version.clone()))
                {
                    *dependency = replacement.clone();
                }
            }
        }
        Ok(lock)
    }

    /// The `(repository, revision)` a channel framework resolves its packages
    /// from — `None` on the stable channel, which resolves from the registry,
    /// and on a local checkout, which resolves by path.
    ///
    /// Generated crates use this to point `[patch]` entries the framework's own
    /// table does not carry at the same source the framework resolves to.
    pub(crate) const fn git_source(&self) -> Option<(&str, &str)> {
        match &self.source {
            Source::Stable { .. } | Source::Local { .. } => None,
            Source::Dev {
                repository,
                revision,
                ..
            }
            | Source::Nightly {
                repository,
                revision,
                ..
            } => Some((repository.as_str(), revision.as_str())),
        }
    }

    /// The locked identities a generated project's resolution may produce
    /// for each recorded package: the recorded one, plus — for a crate the
    /// patch tables pin to a repository of its own — the same package at the
    /// pin and at the framework's own source. Cargo vendors a git
    /// dependency's submodules, so a submodule crate's path edges resolve
    /// inside the framework's source while its `[patch]` edge resolves at
    /// the submodule repository — the same commit either way (#807).
    fn allowed_packages<'p>(
        &self,
        packages: impl IntoIterator<Item = &'p cargo_lock::Package>,
    ) -> BTreeSet<LockedPackage> {
        let mut allowed = BTreeSet::new();
        let Some((repository, revision)) = self.git_source() else {
            return packages.into_iter().map(LockedPackage::from).collect();
        };
        let framework_source = format!("git+{repository}?rev={revision}#{revision}");
        // Crate name → the `git+<repo>?rev=<rev>` source its patch pins it to.
        let pinned: BTreeMap<&str, String> = self
            .patches
            .values()
            .flatten()
            .filter_map(|(name, dependency)| {
                let Dependency::Detailed(detail) = dependency else {
                    return None;
                };
                let (git, rev) = detail.git.as_deref().zip(detail.rev.as_deref())?;
                Some((name.as_str(), format!("git+{git}?rev={rev}#{rev}")))
            })
            .collect();
        for package in packages {
            let identity = LockedPackage::from(package);
            if let Some(source) = &identity.source
                && let Some(pin) = pinned.get(identity.name.as_str())
            {
                if source == &framework_source {
                    allowed.insert(LockedPackage {
                        source: Some(pin.clone()),
                        ..identity.clone()
                    });
                } else if source == pin {
                    allowed.insert(LockedPackage {
                        source: Some(framework_source.clone()),
                        ..identity.clone()
                    });
                }
            }
            allowed.insert(identity);
        }
        allowed
    }

    /// Whether `identity` resolves a scaffold package at the source its
    /// declared requirement sanctions — the declared `git + rev`, or the
    /// registry at the pinned `=version`. An extracted crate never enters
    /// `Water.lock`; the declared pin is the certification of what it must
    /// resolve to.
    fn sanctioned_source(&self, identity: &LockedPackage) -> bool {
        let Some(detail) = self.packages.get(identity.name.as_str()) else {
            return false;
        };
        let Some(source) = &identity.source else {
            return false;
        };
        if let (Some(git), Some(rev)) = (&detail.git, &detail.rev) {
            let Ok(source) = source.parse::<cargo_lock::SourceId>() else {
                return false;
            };
            let declared = cargo_lock::package::GitReference::Rev(rev.clone());
            return source.is_git()
                && source.git_reference() == Some(&declared)
                && canonical_git_url(source.url().as_str()) == canonical_git_url(git);
        }
        source.as_str() == "registry+https://github.com/rust-lang/crates.io-index"
            && detail.version.as_ref().is_some_and(|requirement| {
                identity
                    .version
                    .parse::<cargo_toml::SemVer>()
                    .is_ok_and(|version| requirement.matches(&version))
            })
    }

    pub(crate) fn dependency(&self, name: &str) -> DependencyDetail {
        match &self.source {
            Source::Stable { .. } => {
                let requirement = self.scaffold_value(&format!("{name}-version"));
                // The registry substitutes for a declared git pin only once
                // the workspace names the crate by version alone.
                let git = self.scaffold.get(&format!("{name}-git"));
                DependencyDetail {
                    version: Some(
                        git.map_or_else(|| format!("={requirement}"), |_| requirement.to_owned())
                            .parse()
                            .expect("resolved package version is valid"),
                    ),
                    git: git.cloned(),
                    rev: git.map(|_| self.scaffold_value(&format!("{name}-rev")).to_owned()),
                    ..Default::default()
                }
            }
            Source::Dev { .. } | Source::Nightly { .. } => self.packages[name].clone(),
            Source::Local { .. } => {
                unreachable!("a local checkout resolves framework crates by path")
            }
        }
    }

    /// Resolve a channel's exact framework selection.
    ///
    /// `dev` resolves the integration branch head once it has passed its
    /// compilation gate; `nightly` and `stable` resolve the newest eligible
    /// GitHub release carrying a `framework.json` — a published `nightly-*`
    /// prerelease, a published `v<semver>` release — and pin what it
    /// certifies.
    ///
    /// # Errors
    /// Returns an error when the channel has no eligible release, the manifest
    /// fails verification, or the certified revision cannot be fetched.
    pub(crate) async fn resolve(channel: FrameworkChannel) -> Result<(Self, Option<Vec<u8>>)> {
        let repository = framework_repository();
        let slug = repository_slug(repository)?;
        match channel {
            FrameworkChannel::Stable | FrameworkChannel::Nightly => {
                let certification = latest_certification(repository, channel).await?;
                let revision = certification.revision.clone();
                Self::construct(repository, slug, &revision, Some(certification)).await
            }
            FrameworkChannel::Dev => {
                let revision = resolve_dev(repository, slug).await?;
                Self::construct(repository, slug, &revision, None).await
            }
        }
    }

    /// Build the resolved selection for the framework tree at `revision`, plus
    /// the certification a certified channel carries.
    ///
    /// Every channel shares this path: the fetched root manifest supplies the
    /// scaffold requirements and framework metadata, the fetched lock the
    /// workspace versions, and the submodule pins — the certification's record
    /// for a certified channel, the repository's gitlinks for `dev` — the
    /// backend revisions. The certification is then held to the tree it names:
    /// its scaffold table must agree with the manifest's, its lock hash with
    /// the fetched lock.
    async fn construct(
        repository: &str,
        slug: &str,
        revision: &str,
        certification: Option<Certification>,
    ) -> Result<(Self, Option<Vec<u8>>)> {
        validate_revision(revision)?;
        let base = format!("https://raw.githubusercontent.com/{slug}/{revision}");
        let manifest_bytes = fetch(&format!("{base}/Cargo.toml")).await?;
        let root: toml::Value = toml::from_str(std::str::from_utf8(&manifest_bytes)?)?;
        let metadata = framework_metadata(&root)?;
        let minimum_cli_version = minimum_cli_version(&metadata)?;
        let mut scaffold = framework_scaffold(&root)?;
        let lock_bytes = fetch(&format!("{base}/Cargo.lock")).await?;
        let lock_sha256 = hex::encode(Sha256::digest(&lock_bytes));
        let lock: Lockfile = std::str::from_utf8(&lock_bytes)?.parse()?;

        let channel = certification
            .as_ref()
            .map_or(FrameworkChannel::Dev, |certification| certification.channel);
        if let Some(minimum) = &minimum_cli_version {
            let update = match channel {
                FrameworkChannel::Stable => registry_cli_update(minimum),
                FrameworkChannel::Dev | FrameworkChannel::Nightly => checkout_cli_update(),
            };
            validate_installed_cli(minimum, &update)?;
        }

        // `.gitmodules` names each submodule path's repository at the
        // revision; the pin's commit half comes from the tree's gitlinks
        // (`dev`) or the certification (a certified channel). `stable`
        // resolves from the registry and carries neither.
        let submodule_repositories = match channel {
            FrameworkChannel::Stable => BTreeMap::new(),
            FrameworkChannel::Dev | FrameworkChannel::Nightly => {
                match fetch_optional(&format!("{base}/.gitmodules")).await? {
                    Some(bytes) => parse_gitmodules(std::str::from_utf8(&bytes)?),
                    // Every submodule was extracted; the revision records none.
                    None => BTreeMap::new(),
                }
            }
        };

        let (source, submodules) = if let Some(certification) = &certification {
            (
                certified_source(
                    certification,
                    repository,
                    revision,
                    &metadata,
                    &scaffold,
                    &lock_sha256,
                )?,
                certification.submodules.clone(),
            )
        } else {
            (
                Source::Dev {
                    repository: repository.to_owned(),
                    revision: revision.to_owned(),
                    lock_sha256,
                },
                dev_submodules(slug, revision, &scaffold, &submodule_repositories).await?,
            )
        };
        complete_scaffold(&mut scaffold, &submodules, &lock)?;
        channel_apple_backend_pin(channel, &mut scaffold, certification.as_ref()).await?;

        let (packages, patches, lockfile) = match channel {
            // A stable project resolves its graph from the registry; nothing is
            // pinned to the framework repository, so there is no package detail
            // or canonical lock to persist.
            FrameworkChannel::Stable => (BTreeMap::new(), PatchSet::default(), None),
            FrameworkChannel::Dev | FrameworkChannel::Nightly => {
                let patches: PatchSet = root
                    .get("patch")
                    .cloned()
                    .map(toml::Value::try_into)
                    .transpose()?
                    .unwrap_or_default();
                // A path under a submodule belongs to the submodule's
                // repository at the pinned commit, not the superproject's —
                // whose tree holds a gitlink there, not the crate.
                let pins = submodule_pins(submodule_repositories, &submodules);
                let patches = rebase_patches_onto_source(patches, repository, revision, &pins);
                let packages = resolve_packages(&scaffold, &lock, repository, revision)?;
                (packages, patches, Some(lock_bytes))
            }
        };
        Ok((
            Self {
                source,
                minimum_cli_version,
                metadata,
                scaffold,
                packages,
                patches,
            }
            .validated()?,
            lockfile,
        ))
    }
}

/// The Apple backend follows the framework's channel. `dev` resolves the
/// backend's own `dev` HEAD — the compilation-gated revision the channel
/// promises — because the `backends/apple` gitlink that used to record the
/// pairing is gone and `apple-backend-version` is a stable pin. A
/// certification may likewise name the backend revision its suite ran.
/// Either lands as `apple-backend-revision`, the pin a non-stable channel's
/// requirement prefers; a framework from before the backend's extraction
/// instead keeps the gitlink pin `complete_scaffold` recorded.
async fn channel_apple_backend_pin(
    channel: FrameworkChannel,
    scaffold: &mut BTreeMap<String, String>,
    certification: Option<&Certification>,
) -> Result<()> {
    match channel {
        FrameworkChannel::Dev if scaffold.contains_key("apple-backend-version") => {
            let url = scaffold.get("apple-backend-url").ok_or_else(|| {
                eyre!("framework manifest declares apple-backend-version without apple-backend-url")
            })?;
            let revision = backend_dev_revision(url).await?;
            scaffold.insert("apple-backend-revision".to_owned(), revision);
        }
        FrameworkChannel::Nightly => {
            if let Some(revision) = certification
                .and_then(|certification| certification.scaffold.get("apple-backend-revision"))
            {
                validate_revision(revision)
                    .wrap_err("nightly certification scaffold `apple-backend-revision`")?;
                scaffold.insert("apple-backend-revision".to_owned(), revision.clone());
            }
        }
        FrameworkChannel::Stable | FrameworkChannel::Dev => {}
    }
    Ok(())
}

/// `dev` has no certification; the repository tree's own gitlinks record which
/// submodule revisions the revision was built against — every submodule
/// `.gitmodules` names plus `backends/apple`, whose manifest declaration
/// predates its extraction from the tree.
async fn dev_submodules(
    slug: &str,
    revision: &str,
    scaffold: &BTreeMap<String, String>,
    submodule_repositories: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut submodules = BTreeMap::new();
    for path in BACKEND_SUBMODULES {
        if declares_backend_revision(scaffold, path) {
            continue;
        }
        submodules.insert(
            (*path).to_owned(),
            submodule_revision(slug, revision, path).await?,
        );
    }
    // Revisions from before the Apple backend left the tree still carry its
    // `backends/apple` gitlink. A declared version or revision supplies the pin.
    if !declares_apple_backend_pin(scaffold) {
        submodules.insert(
            "backends/apple".to_owned(),
            submodule_revision(slug, revision, "backends/apple").await?,
        );
    }
    // The remaining `.gitmodules` entries (`kit`, `utils/nami`, …) pin no
    // scaffold fact, but a `[patch]` path under one rebases onto the
    // submodule's repository at the gitlink's commit — the same record the
    // certification supplies for `nightly`.
    for path in submodule_repositories.keys() {
        if !submodules.contains_key(path)
            && let Some(commit) = submodule_pin(slug, revision, path).await?
        {
            submodules.insert(path.clone(), commit);
        }
    }
    Ok(submodules)
}

/// Marry each `.gitmodules` path's repository URL to its recorded commit, the
/// pin a `[patch]` path under it rebases onto.
fn submodule_pins(
    repositories: BTreeMap<String, String>,
    submodules: &BTreeMap<String, String>,
) -> BTreeMap<String, SubmodulePin> {
    repositories
        .into_iter()
        .filter_map(|(path, url)| {
            submodules.get(&path).map(|commit| {
                (
                    path,
                    SubmodulePin {
                        repository: canonical_git_url(&url).to_owned(),
                        commit: commit.clone(),
                    },
                )
            })
        })
        .collect()
}

/// The persisted source a certification proves, checked against the tree it
/// names: the certified scaffold table, metadata, CLI floor and lock hash must
/// all agree with the fetched framework manifest before the release pin is
/// trusted.
fn certified_source(
    certification: &Certification,
    repository: &str,
    revision: &str,
    metadata: &toml::Table,
    scaffold: &BTreeMap<String, String>,
    lock_sha256: &str,
) -> Result<Source> {
    let channel = certification.channel;
    if certification.metadata != *metadata {
        bail!("{channel} framework metadata does not match its certification");
    }
    for (key, value) in scaffold {
        if certification.scaffold.get(key) != Some(value) {
            bail!("{channel} certification scaffold `{key}` does not match the framework manifest");
        }
    }
    let expected = certification
        .lockfiles
        .get("Cargo.lock")
        .ok_or_else(|| eyre!("{channel} certification has no dependency lock"))?;
    if lock_sha256 != *expected {
        bail!("{channel} dependency lock does not match its certification");
    }
    let release = FrameworkRelease {
        repository: repository.to_owned(),
        revision: revision.to_owned(),
        tag: certification.tag.clone(),
    };
    Ok(match certification.channel {
        FrameworkChannel::Stable => Source::Stable {
            release: Some(release),
        },
        FrameworkChannel::Nightly => Source::Nightly {
            repository: repository.to_owned(),
            revision: revision.to_owned(),
            tag: certification.tag.clone(),
            lock_sha256: lock_sha256.to_owned(),
        },
        FrameworkChannel::Dev => unreachable!("verify_certification rejects a dev manifest"),
    })
}

/// The submodule each native backend repository used to be pinned through;
/// the directory's basename keys the scaffold's `{name}-backend-revision`
/// entry. A framework that declares `{name}-backend-revision` in
/// `[package.metadata.waterui]` (Android, since water-rs/waterui#940) or
/// `{name}-backend-version` (Apple, since #839) carries no gitlink, and the
/// gitlink is read only for a revision from before that declaration.
const BACKEND_SUBMODULES: &[&str] = &["backends/android"];

fn declares_apple_backend_pin(scaffold: &BTreeMap<String, String>) -> bool {
    scaffold.contains_key("apple-backend-version")
        || declares_backend_revision(scaffold, "backends/apple")
}

/// Whether the scaffold already names `submodule_path`'s backend pin — a
/// declared `{name}-backend-revision` — so no gitlink has to be read for it.
fn declares_backend_revision(scaffold: &BTreeMap<String, String>, submodule_path: &str) -> bool {
    scaffold.contains_key(&format!(
        "{}-backend-revision",
        backend_name(submodule_path)
    ))
}

/// The workspace crates a scaffolded project pins; each `{name}-version`
/// scaffold entry comes from the framework's own lockfile at the selected
/// revision.
const FRAMEWORK_PACKAGES: &[&str] = &[
    "waterui",
    "waterui-core",
    "waterui-testing",
    "waterui-ffi",
    "waterui-locale",
    "waterui-browser-cef",
    "waterui-preview",
    "waterui-preview-protocol",
    "waterui-mcp",
];

fn backend_name(submodule_path: &str) -> &str {
    submodule_path
        .rsplit('/')
        .next()
        .expect("a submodule path has a basename")
}

/// The repository the CLI's pinned `waterui-*` dependencies resolve from —
/// where certified manifests, releases, and `dev` revisions live. `build.rs`
/// bakes it in from the git source in `Cargo.toml` so the pin is declared
/// exactly once.
fn framework_repository() -> &'static str {
    env!("WATERUI_FRAMEWORK_REPOSITORY").trim_end_matches(".git")
}

/// A repository's `owner/name` slug, from its GitHub URL.
fn repository_slug(repository: &str) -> Result<&str> {
    repository
        .strip_prefix("https://github.com/")
        .ok_or_else(|| eyre!("{repository} must identify its GitHub source"))
}

/// The framework's own metadata table — `[package.metadata.waterui]` of the
/// manifest at the selected revision — carried verbatim into every published
/// `framework.json` and every persisted selection.
fn framework_metadata(manifest: &toml::Value) -> Result<toml::Table> {
    manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("waterui"))
        .map_or_else(
            || Ok(toml::Table::new()),
            |metadata| {
                metadata
                    .clone()
                    .try_into()
                    .wrap_err("invalid package.metadata.waterui metadata")
            },
        )
}

/// The scaffold facts the framework manifest itself declares: each
/// `scaffold-packages` entry's requirement from `[workspace.dependencies]` —
/// `{name}-version`, plus `{name}-git` and `{name}-rev` when the requirement
/// pins a repository — and every backend coordinate — `{name}-backend-url`,
/// plus the `{name}-backend-version` of a backend pinned by release or the
/// `{name}-backend-revision` of one pinned by commit, rather than by
/// gitlink — from `[package.metadata.waterui]`.
///
/// `framework_manifest.py` emits exactly this table into every `framework.json`
/// it publishes; both must produce the same table for the same tree.
fn framework_scaffold(manifest: &toml::Value) -> Result<BTreeMap<String, String>> {
    let metadata = framework_metadata(manifest)?;
    let workspace = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| eyre!("framework manifest has no [workspace.dependencies]"))?;
    let packages = metadata
        .get("scaffold-packages")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| {
            eyre!("framework manifest has no package.metadata.waterui.scaffold-packages")
        })?;
    let mut scaffold = BTreeMap::new();
    for package in packages {
        let name = package.as_str().ok_or_else(|| {
            eyre!("package.metadata.waterui.scaffold-packages entries must be crate names")
        })?;
        let dependency = workspace.get(name).ok_or_else(|| {
            eyre!("scaffold package {name} has no [workspace.dependencies] requirement")
        })?;
        let requirement = dependency
            .as_str()
            .or_else(|| dependency.get("version").and_then(toml::Value::as_str))
            .ok_or_else(|| eyre!("workspace.dependencies.{name} declares no version"))?;
        scaffold.insert(format!("{name}-version"), requirement.to_owned());
        // A scaffold package pinned from git keeps that source: a bare
        // `{name}-version` cannot express the commit the framework builds
        // against, and the registry may not carry it at all.
        if let Some(git) = dependency.get("git").and_then(toml::Value::as_str) {
            let revision = dependency
                .get("rev")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    eyre!("workspace.dependencies.{name} must pin an immutable Git revision")
                })?;
            validate_revision(revision)
                .wrap_err_with(|| format!("workspace.dependencies.{name}.rev"))?;
            scaffold.insert(format!("{name}-git"), git.to_owned());
            scaffold.insert(format!("{name}-rev"), revision.to_owned());
        }
    }
    for (key, value) in &metadata {
        if !(key.ends_with("-backend-url")
            || key.ends_with("-backend-version")
            || key.ends_with("-backend-revision"))
        {
            continue;
        }
        let value = value
            .as_str()
            .ok_or_else(|| eyre!("package.metadata.waterui.{key} must be a string"))?;
        if key.ends_with("-backend-revision") {
            validate_revision(value).wrap_err_with(|| format!("package.metadata.waterui.{key}"))?;
        }
        scaffold.insert(key.clone(), value.to_owned());
    }
    Ok(scaffold)
}

/// The CLI floor a `package.metadata.waterui` metadata table declares —
/// read the same way from a checked-out root manifest and from a
/// certification's `metadata` table.
fn minimum_cli_version(metadata: &toml::Table) -> Result<Option<cargo_toml::SemVer>> {
    metadata
        .get("minimum-cli-version")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .wrap_err("invalid package.metadata.waterui.minimum-cli-version")
}

/// The CLI update hint for a framework that is not a registry release — a
/// local checkout or a git-pinned `dev`/`nightly` source pairs with the
/// development line of this repository.
fn checkout_cli_update() -> String {
    format!(
        "cargo install {} --git {} --locked",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

fn registry_cli_update(minimum: &cargo_toml::SemVer) -> String {
    format!(
        "cargo install {} --version '>={minimum}' --locked",
        env!("CARGO_PKG_NAME")
    )
}

fn validate_installed_cli(minimum: &cargo_toml::SemVer, update: &str) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION")
        .parse()
        .expect("CLI package version is valid");
    validate_cli_version(minimum, &current, update)
}

fn validate_cli_version(
    minimum: &cargo_toml::SemVer,
    current: &cargo_toml::SemVer,
    update: &str,
) -> Result<()> {
    if current.cmp_precedence(minimum).is_lt() {
        bail!(
            "This WaterUI framework requires waterui-cli >= {minimum}, but the running CLI is {current}.\nUpdate the CLI: {update}\nThen verify the installed version with `water --version`."
        );
    }
    Ok(())
}

pub(crate) async fn validate_local_cli(root: &Path) -> Result<()> {
    let contents = smol::fs::read_to_string(root.join("Cargo.toml")).await?;
    let manifest = toml::from_str(&contents)?;
    if let Some(minimum) = minimum_cli_version(&framework_metadata(&manifest)?)? {
        validate_installed_cli(&minimum, &checkout_cli_update())?;
    }
    Ok(())
}

pub(crate) fn validate_resolved_cli(metadata: &cargo_metadata::Metadata) -> Result<()> {
    for package in metadata
        .packages
        .iter()
        .filter(|package| package.name.as_str() == "waterui")
    {
        let Some(value) = package
            .metadata
            .get("waterui")
            .and_then(|metadata| metadata.get("minimum-cli-version"))
        else {
            continue;
        };
        let minimum: cargo_toml::SemVer = serde_json::from_value(value.clone())
            .wrap_err("invalid package.metadata.waterui.minimum-cli-version")?;
        let source = package
            .source
            .as_ref()
            .map(|source| source.repr.parse::<cargo_lock::SourceId>())
            .transpose()?;
        let update = match source {
            Some(source) if !source.is_git() => registry_cli_update(&minimum),
            Some(_) | None => checkout_cli_update(),
        };
        validate_installed_cli(&minimum, &update)?;
    }
    Ok(())
}

fn resolve_packages(
    scaffold: &BTreeMap<String, String>,
    lock: &Lockfile,
    repository: &str,
    revision: &str,
) -> Result<BTreeMap<String, DependencyDetail>> {
    let mut packages = BTreeMap::new();
    for (key, version) in scaffold {
        let Some(name) = key.strip_suffix("-version") else {
            continue;
        };
        // A `{name}-backend-version` entry pins a backend repository's release
        // tag, not a crate — there is no package to resolve for it.
        if name.ends_with("-backend") {
            continue;
        }
        let candidates: Vec<_> = lock
            .packages
            .iter()
            .filter(|package| {
                package.name.as_str() == name && package.version.to_string() == *version
            })
            .collect();
        // A scaffold package the workspace pins from git resolves from that
        // pin on every channel: an extracted crate never enters the framework
        // lock, and for one built in-tree the lock only witnesses that the
        // framework resolves the same commit.
        if let Some(git) = scaffold.get(&format!("{name}-git")) {
            let pinned = scaffold.get(&format!("{name}-rev")).ok_or_else(|| {
                eyre!("framework scaffold declares {name}-git without {name}-rev")
            })?;
            match candidates.as_slice() {
                [] => {}
                [package] => assert_declared_git_source(name, package, git, pinned)?,
                _ => bail!("framework lock has multiple sources for {name} {version}"),
            }
            packages.insert(
                name.to_owned(),
                DependencyDetail {
                    version: Some(version.parse()?),
                    git: Some(git.clone()),
                    rev: Some(pinned.clone()),
                    ..Default::default()
                },
            );
            continue;
        }
        let package = match candidates.as_slice() {
            [package] => *package,
            // An extracted crate the framework no longer builds never enters
            // its lock — `waterui-dew` releases from water-rs/dew (#614) — so
            // the scaffold's declared requirement is the resolution, the same
            // `=<version>` the registry-source arm below produces for a crate
            // the framework still carries.
            [] => {
                packages.insert(
                    name.to_owned(),
                    DependencyDetail {
                        version: Some(format!("={version}").parse()?),
                        ..Default::default()
                    },
                );
                continue;
            }
            _ => bail!("framework lock has multiple sources for {name} {version}"),
        };
        let mut dependency = DependencyDetail::default();
        match &package.source {
            None => {
                dependency.git = Some(repository.to_owned());
                dependency.rev = Some(revision.to_owned());
            }
            Some(source) if source.is_default_registry() => {
                dependency.version = Some(format!("={version}").parse()?);
            }
            Some(source) if source.is_git() => {
                let Some(cargo_lock::package::GitReference::Rev(revision)) = source.git_reference()
                else {
                    bail!(
                        "framework package {name} must use an immutable Git revision in the framework manifest"
                    );
                };
                validate_revision(revision)?;
                if source.precise() != Some(revision.as_str()) {
                    bail!("framework package {name} does not resolve its declared revision");
                }
                dependency.git = Some(source.url().to_string());
                dependency.rev = Some(revision.clone());
            }
            Some(source) => bail!("unsupported framework package source for {name}: {source}"),
        }
        packages.insert(name.to_owned(), dependency);
    }
    Ok(packages)
}

/// Assert `package`'s lock source is the git repository a declared
/// `{name}-git`/`{name}-rev` scaffold pair names — the witness that the
/// framework builds the same commit a scaffolded project receives.
fn assert_declared_git_source(
    name: &str,
    package: &cargo_lock::Package,
    git: &str,
    revision: &str,
) -> Result<()> {
    let Some(source) = &package.source else {
        bail!("framework package {name} is a workspace member, not the declared {git}");
    };
    let declared = cargo_lock::package::GitReference::Rev(revision.to_owned());
    if !(source.is_git()
        && source.git_reference() == Some(&declared)
        && source.precise() == Some(revision)
        && canonical_git_url(source.url().as_str()) == canonical_git_url(git))
    {
        bail!("framework package {name} resolves {source}, not the declared {git}@{revision}");
    }
    Ok(())
}

#[derive(Deserialize)]
struct SubmoduleEntry {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

/// Fill in what the framework manifest cannot carry itself: each backend's
/// pinned revision — `submodules` maps submodule path to the commit the
/// certification or the checkout records — and every framework package's
/// version from the framework's own lock.
fn complete_scaffold(
    scaffold: &mut BTreeMap<String, String>,
    submodules: &BTreeMap<String, String>,
    lock: &Lockfile,
) -> Result<()> {
    // A declared `{name}-backend-revision` is already in the scaffold
    // (`framework_scaffold` copied and validated it); the gitlink is the pin
    // record only for a framework from before the declaration.
    for &submodule in BACKEND_SUBMODULES {
        if declares_backend_revision(scaffold, submodule) {
            continue;
        }
        let commit = submodules
            .get(submodule)
            .ok_or_else(|| eyre!("framework records no {submodule} submodule pin"))?;
        validate_revision(commit)?;
        scaffold.insert(
            format!("{}-backend-revision", backend_name(submodule)),
            commit.clone(),
        );
    }
    // `framework_scaffold` already copied declared Apple versions or revisions.
    // Only a framework without either declaration needs its historical gitlink
    // promoted to the revision requirement consumed by the package template.
    if !declares_apple_backend_pin(scaffold) {
        let commit = submodules
            .get("backends/apple")
            .ok_or_else(|| eyre!("framework records no Apple backend pin"))?;
        validate_revision(commit)?;
        scaffold.insert("apple-backend-revision".to_owned(), commit.clone());
    }
    for &name in FRAMEWORK_PACKAGES {
        let candidates: Vec<_> = lock
            .packages
            .iter()
            .filter(|package| package.name.as_str() == name)
            .collect();
        let version = match candidates.as_slice() {
            [package] => package.version.to_string(),
            [] => bail!("framework lock has no package named {name}"),
            _ => bail!("framework lock has multiple packages named {name}"),
        };
        scaffold.insert(format!("{name}-version"), version);
    }
    Ok(())
}

/// The commit a submodule of a local checkout records at `HEAD` — the same
/// fact `submodule_revision` reads from the repository tree for a remote
/// revision.
async fn local_submodule_revision(root: &Path, path: &str) -> Result<String> {
    let treeish = format!("HEAD:{path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", treeish.as_str()])
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "the WaterUI checkout at {} records no {path} submodule pin: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let revision = std::str::from_utf8(&output.stdout)?.trim().to_owned();
    validate_revision(&revision)?;
    Ok(revision)
}

/// The commit a submodule of the framework repository records at `revision`,
/// read from the repository tree — the only record that pairs the revision
/// with the backends it was built and tested against.
async fn submodule_revision(slug: &str, revision: &str, path: &str) -> Result<String> {
    let bytes = fetch(&format!(
        "https://api.github.com/repos/{slug}/contents/{path}?ref={revision}"
    ))
    .await?;
    let entry: SubmoduleEntry = serde_json::from_slice(&bytes)?;
    if entry.kind != "submodule" {
        bail!(
            "{path} at {slug}@{revision} is a {}, not a submodule",
            entry.kind
        );
    }
    Ok(entry.sha)
}

/// The commit `path`'s gitlink records at `revision`, or `None` when `path`
/// is not a submodule there — a `.gitmodules` entry can outlive the gitlink
/// it once named, and the patch paths under it then belong in the tree.
async fn submodule_pin(slug: &str, revision: &str, path: &str) -> Result<Option<String>> {
    let Some(bytes) = fetch_optional(&format!(
        "https://api.github.com/repos/{slug}/contents/{path}?ref={revision}"
    ))
    .await?
    else {
        return Ok(None);
    };
    // A present-but-ordinary path lists as a directory array or carries a
    // non-submodule type; neither is a pin.
    let Ok(entry) = serde_json::from_slice::<SubmoduleEntry>(&bytes) else {
        return Ok(None);
    };
    Ok((entry.kind == "submodule").then_some(entry.sha))
}

/// The `path → url` pairs `.gitmodules` records — git-config syntax rather
/// than TOML (values go unquoted), so a line scan keyed on `[submodule]`
/// sections.
fn parse_gitmodules(contents: &str) -> BTreeMap<String, String> {
    let mut submodules = BTreeMap::new();
    let mut submodule = false;
    let mut path = None::<String>;
    let mut url = None::<String>;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            if submodule && let (Some(path), Some(url)) = (path.take(), url.take()) {
                submodules.insert(path, url);
            }
            submodule = line.starts_with("[submodule");
            continue;
        }
        if !submodule {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "path" => path = Some(value.trim().trim_matches('"').to_owned()),
                "url" => url = Some(value.trim().trim_matches('"').to_owned()),
                _ => {}
            }
        }
    }
    if submodule && let (Some(path), Some(url)) = (path, url) {
        submodules.insert(path, url);
    }
    submodules
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("framework revision must be a full Git commit hash");
    }
    Ok(())
}

async fn fetch(url: &str) -> Result<Vec<u8>> {
    fetch_optional(url)
        .await?
        .ok_or_else(|| eyre!("framework resolution returned HTTP 404 from {url}"))
}

/// [`fetch`] that answers `None` when the resource does not exist —
/// `.gitmodules` is absent on a revision whose submodules were all
/// extracted. zenwave surfaces a non-success status as `Err`, so the 404
/// arrives as an [`Error::Http`], never as a response to inspect.
async fn fetch_optional(url: &str) -> Result<Option<Vec<u8>>> {
    let mut client = zenwave::client();
    let response = match client
        .method(Method::GET, url)?
        .header("User-Agent", env!("CARGO_PKG_NAME"))?
        .await
    {
        Ok(response) => response,
        Err(zenwave::Error::Http { status, .. }) if status == StatusCode::NOT_FOUND => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Some(response.into_body().into_bytes().await?.to_vec()))
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use std::process::Command as StdCommand;

    use super::*;

    /// A stable-channel resolution carrying every scaffold fact the templates
    /// may read — the shape `resolve` produces, built in place because the
    /// real resolution lives on the network.
    pub fn stable_framework() -> ResolvedFramework {
        let revision = |seed: char| seed.to_string().repeat(40);
        let scaffold = FRAMEWORK_PACKAGES
            .iter()
            .map(|name| (format!("{name}-version"), "0.4.1".to_owned()))
            .chain([
                ("hydrolysis-version".to_owned(), "0.2.1".to_owned()),
                ("hydrolysis-m3-version".to_owned(), "0.2.0".to_owned()),
                ("waterui-dew-version".to_owned(), "0.2.1".to_owned()),
                ("waterui-gtk-version".to_owned(), "0.1.2".to_owned()),
                (
                    "apple-backend-url".to_owned(),
                    "https://github.com/water-rs/apple-backend.git".to_owned(),
                ),
                ("apple-backend-version".to_owned(), "0.3.0-dev.2".to_owned()),
                (
                    "android-backend-url".to_owned(),
                    "https://github.com/water-rs/android-backend.git".to_owned(),
                ),
                ("android-backend-revision".to_owned(), revision('c')),
            ])
            .collect();
        ResolvedFramework {
            source: Source::Stable {
                release: Some(FrameworkRelease {
                    repository: framework_repository().to_owned(),
                    revision: revision('a'),
                    tag: "v0.4.1".to_owned(),
                }),
            },
            minimum_cli_version: None,
            metadata: toml::toml! {
                android-min-api-level = 26
            },
            scaffold,
            packages: BTreeMap::new(),
            patches: PatchSet::default(),
        }
    }

    /// A `dev`-channel resolution: the manifest's scaffold facts plus the
    /// `apple-backend-revision` `construct` resolves for the channel — the
    /// backend's `dev` HEAD at selection time — beside the declared
    /// `apple-backend-version` the channel must not follow.
    pub fn dev_framework() -> ResolvedFramework {
        let mut framework = stable_framework();
        framework.source = Source::Dev {
            repository: framework_repository().to_owned(),
            revision: 'a'.to_string().repeat(40),
            lock_sha256: 'f'.to_string().repeat(64),
        };
        framework.scaffold.insert(
            "apple-backend-revision".to_owned(),
            'd'.to_string().repeat(40),
        );
        framework
    }

    /// A `nightly`-channel resolution; `backend_revision` carries the
    /// `apple-backend-revision` a certification records when its suite names
    /// the backend it ran — absent, the declared `apple-backend-version` is
    /// what the certification certified.
    pub fn nightly_framework(backend_revision: bool) -> ResolvedFramework {
        let mut framework = dev_framework();
        if !backend_revision {
            framework.scaffold.remove("apple-backend-revision");
        }
        framework.source = Source::Nightly {
            repository: framework_repository().to_owned(),
            revision: 'a'.to_string().repeat(40),
            tag: "nightly-2026.09.15".to_owned(),
            lock_sha256: 'f'.to_string().repeat(64),
        };
        framework
    }

    /// A local framework checkout fixture: the repository's own root manifest
    /// and a lock naming the workspace crates, inside a git worktree. Like the
    /// repository today it carries no backend gitlink: both backend pins are
    /// literals in the manifest.
    pub fn write_local_checkout(root: &Path) {
        std::fs::create_dir_all(root).expect("checkout dir");
        std::fs::write(root.join("Cargo.toml"), local_checkout_manifest()).expect("manifest");
        let lock = test_lock();
        std::fs::write(root.join("Cargo.lock"), lock.to_string()).expect("lockfile");
        let git = |args: &[String]| {
            let status = StdCommand::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .expect("git must run");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init".to_owned(), "-q".to_owned()]);
        git(&[
            "add".to_owned(),
            "Cargo.toml".to_owned(),
            "Cargo.lock".to_owned(),
        ]);
        git(&[
            "-c".to_owned(),
            "user.name=waterui-test".to_owned(),
            "-c".to_owned(),
            "user.email=waterui-test@waterui.dev".to_owned(),
            "commit".to_owned(),
            "-qm".to_owned(),
            "init".to_owned(),
        ]);
    }

    pub fn write_apple_revision_checkout(root: &Path, revision: &str) {
        write_local_checkout(root);
        let manifest_path = root.join("Cargo.toml");
        let mut manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let metadata = manifest["package"]["metadata"]["waterui"]
            .as_table_mut()
            .unwrap();
        metadata.remove("apple-backend-version");
        metadata.insert(
            "apple-backend-revision".to_owned(),
            toml::Value::String(revision.to_owned()),
        );
        std::fs::write(manifest_path, toml::to_string(&manifest).unwrap()).unwrap();
    }

    /// The same fixture as it existed while both backends still rode
    /// gitlinks: no `apple-backend-version` and no `android-backend-revision`
    /// in the manifest, the submodule pins recorded in the index.
    pub fn write_pre_decoupling_checkout(root: &Path) {
        write_local_checkout(root);
        write_submodule_pin(root, "backends/apple", 'b');
        write_submodule_pin(root, "backends/android", 'c');
        let manifest = local_checkout_manifest()
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                !(line.starts_with("apple-backend-version")
                    || line.starts_with("android-backend-revision"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !manifest.contains("apple-backend-version")
                && !manifest.contains("android-backend-revision"),
            "the fixture manifest moved; the pre-decoupling rewrite must be revisited"
        );
        std::fs::write(root.join("Cargo.toml"), manifest).expect("manifest");
        let git = |args: &[&str]| {
            let status = StdCommand::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .expect("git must run");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["add", "Cargo.toml"]);
        git(&[
            "-c",
            "user.name=waterui-test",
            "-c",
            "user.email=waterui-test@waterui.dev",
            "commit",
            "-qm",
            "pre-decoupling manifest",
        ]);
    }

    /// Record a gitlink pin the way a checked-out submodule records it —
    /// `160000` is the mode `git submodule` writes into the index.
    fn write_submodule_pin(root: &Path, path: &str, seed: char) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},{}", seed.to_string().repeat(40), path),
            ])
            .status()
            .expect("git must run");
        assert!(status.success(), "git update-index failed");
    }

    /// The manifest a local checkout fixture carries: the framework's own
    /// metadata and the workspace requirements `scaffold-packages` names.
    fn local_checkout_manifest() -> &'static str {
        include_str!("../../tests/fixtures/framework_checkout_manifest.toml")
    }

    /// A framework lock naming every workspace crate a scaffolded project pins.
    pub fn test_lock() -> Lockfile {
        Lockfile {
            packages: FRAMEWORK_PACKAGES
                .iter()
                .map(|name| package(name, "0.4.1", None))
                .collect(),
            version: cargo_lock::ResolveVersion::V4,
            root: None,
            metadata: BTreeMap::default(),
            patch: cargo_lock::Patch::default(),
        }
    }

    pub fn package(name: &str, version: &str, source: Option<&str>) -> cargo_lock::Package {
        cargo_lock::Package {
            name: name.parse().unwrap(),
            version: version.parse().unwrap(),
            source: source.map(|source| source.parse().unwrap()),
            checksum: None,
            dependencies: Vec::new(),
            replace: None,
        }
    }
}

async fn resolve_dev(repository: &str, slug: &str) -> Result<String> {
    gated_dev_head(repository, slug, "dev.yml", "framework").await
}

/// The Apple backend's `dev` HEAD for a `dev` framework selection. The
/// backend moved out of the framework tree, so nothing records the backend
/// revision a `dev` framework was built against — `apple-backend-version`
/// is the stable pin and must not serve `dev`. The backend's `dev` is held
/// to the same promise the framework's makes: `ci.yml` gates the branch.
async fn backend_dev_revision(url: &str) -> Result<String> {
    gated_dev_head(
        url,
        repository_slug(url.trim_end_matches(".git"))?,
        "ci.yml",
        "Apple backend",
    )
    .await
}

/// The `dev` HEAD of `repository`, held to the channel's promise that the
/// resolved commit passed `gate` — the workflow file gating `dev` in that
/// repository: `dev.yml` for the framework, `ci.yml` for a backend.
async fn gated_dev_head(repository: &str, slug: &str, gate: &str, what: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["ls-remote", repository, "refs/heads/dev"])
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "could not resolve {what} dev: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let revision = std::str::from_utf8(&output.stdout)?
        .split_whitespace()
        .next()
        .ok_or_else(|| eyre!("{what} repository has no dev branch"))?
        .to_owned();
    validate_revision(&revision)?;
    let response = fetch(&format!("https://api.github.com/repos/{slug}/actions/workflows/{gate}/runs?branch=dev&head_sha={revision}&status=success&event=push&per_page=1")).await?;
    let runs: serde_json::Value = serde_json::from_slice(&response)?;
    let checked = runs["workflow_runs"].as_array().is_some_and(|runs| {
        runs.iter().any(|run| {
            run["head_sha"].as_str() == Some(&revision) && run["conclusion"] == "success"
        })
    });
    if !checked {
        bail!("{what} dev revision {revision} has not passed its compilation gate");
    }
    Ok(revision)
}

/// The newest GitHub release `channel` accepts, with its certification
/// manifest loaded and verified against the release it rode in on.
async fn latest_certification(
    repository: &str,
    channel: FrameworkChannel,
) -> Result<Certification> {
    let slug = repository_slug(repository)?;
    let mut releases = Vec::new();
    let mut page = 1;
    loop {
        let bytes = fetch(&format!(
            "https://api.github.com/repos/{slug}/releases?per_page=100&page={page}"
        ))
        .await?;
        let batch: Vec<Release> = serde_json::from_slice(&bytes)?;
        let complete = batch.len() < 100;
        releases.extend(
            batch
                .into_iter()
                .filter(|release| release_matches(release, channel)),
        );
        if complete {
            break;
        }
        page += 1;
    }
    let release = newest_release(releases, channel)?;
    let asset = certification_asset(&release, channel)?;
    let certification = parse_certification(&fetch(&asset.browser_download_url).await?)?;
    verify_certification(&certification, Some(&release), repository)?;
    certifies_channel(&certification, channel)?;
    Ok(certification)
}

/// A release selected for `channel` must carry that channel's manifest: the
/// tag alone does not bind the contents to the distribution it names.
fn certifies_channel(certification: &Certification, channel: FrameworkChannel) -> Result<()> {
    if certification.channel != channel {
        bail!(
            "{} certifies the {} channel, not {channel}",
            certification.tag,
            certification.channel
        );
    }
    Ok(())
}

/// The schema a manifest declares, read before the rest of it so an
/// unsupported schema is reported as such rather than as whichever field it
/// happens to lack.
#[derive(Deserialize)]
struct CertificationSchema {
    schema_version: u32,
}

const CERTIFICATION_SCHEMA_VERSION: u32 = 2;

fn parse_certification(bytes: &[u8]) -> Result<Certification> {
    let schema: CertificationSchema = serde_json::from_slice(bytes)?;
    if schema.schema_version != CERTIFICATION_SCHEMA_VERSION {
        bail!(
            "framework manifest schema version {} is not supported; this CLI requires schema version {CERTIFICATION_SCHEMA_VERSION}",
            schema.schema_version
        );
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Whether a GitHub release can carry `channel`'s manifest.
fn release_matches(release: &Release, channel: FrameworkChannel) -> bool {
    match channel {
        FrameworkChannel::Nightly => {
            release.prerelease && !release.draft && release.tag_name.starts_with("nightly-")
        }
        FrameworkChannel::Stable => {
            !release.prerelease && !release.draft && is_stable_tag(&release.tag_name)
        }
        FrameworkChannel::Dev => false,
    }
}

/// A stable framework release tag: `v` followed by a bare semver version —
/// prerelease and build-metadata tags are not stable distributions.
fn is_stable_tag(tag: &str) -> bool {
    tag.strip_prefix('v').is_some_and(|version| {
        version
            .parse::<cargo_toml::SemVer>()
            .is_ok_and(|version| version.pre.is_empty() && version.build.is_empty())
    })
}

/// The release a channel resolves to: the highest version for stable, whose
/// tags are ordered; the most recently published for nightly, whose tags are
/// dated. Publication order breaks ties.
fn newest_release(releases: Vec<Release>, channel: FrameworkChannel) -> Result<Release> {
    let version = |release: &Release| -> Option<cargo_toml::SemVer> {
        match channel {
            FrameworkChannel::Stable => release.tag_name.strip_prefix('v')?.parse().ok(),
            FrameworkChannel::Nightly | FrameworkChannel::Dev => None,
        }
    };
    releases
        .into_iter()
        .max_by(|left, right| {
            version(left)
                .cmp(&version(right))
                .then_with(|| left.published_at.cmp(&right.published_at))
        })
        .ok_or_else(|| match channel {
            FrameworkChannel::Nightly => {
                eyre!("no certified nightly exists; select dev or stable explicitly")
            }
            FrameworkChannel::Stable => eyre!(
                "no stable framework release carries a manifest yet; \
                 select dev or nightly explicitly"
            ),
            FrameworkChannel::Dev => unreachable!("dev releases are not certified"),
        })
}

/// The `framework.json` asset of the selected release.
fn certification_asset(release: &Release, channel: FrameworkChannel) -> Result<&ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == "framework.json")
        .ok_or_else(|| match channel {
            FrameworkChannel::Nightly => {
                eyre!("nightly {} has no certification manifest", release.tag_name)
            }
            FrameworkChannel::Stable => eyre!(
                "stable release {} carries no framework.json — it predates manifest publishing",
                release.tag_name
            ),
            FrameworkChannel::Dev => unreachable!("dev releases are not certified"),
        })
}

/// Read and verify a `framework.json` from disk: the same schema, channel,
/// repository, revision and CLI checks a downloaded manifest passes, with no
/// release for the tag to be checked against.
async fn load_manifest(path: &Path, repository: &str) -> Result<Certification> {
    let contents = smol::fs::read(path)
        .await
        .wrap_err_with(|| format!("failed to read framework manifest {}", path.display()))?;
    let certification = parse_certification(&contents)
        .wrap_err_with(|| format!("invalid framework manifest {}", path.display()))?;
    verify_certification(&certification, None, repository)?;
    Ok(certification)
}

/// The checks a `framework.json` must pass before it resolves anything —
/// identical whether the manifest was downloaded from `release` or read from
/// disk via `--framework-manifest`, where there is no release to check the
/// tag against.
fn verify_certification(
    certification: &Certification,
    release: Option<&Release>,
    repository: &str,
) -> Result<()> {
    let channel = certification.channel;
    if certification.schema_version != CERTIFICATION_SCHEMA_VERSION {
        bail!(
            "framework manifest schema version {} is not supported; this CLI requires schema version {CERTIFICATION_SCHEMA_VERSION}",
            certification.schema_version
        );
    }
    if channel == FrameworkChannel::Dev {
        bail!("framework manifest channel `dev` is not a certified distribution");
    }
    if certification.repository != repository_slug(repository)? {
        bail!(
            "{channel} manifest names a different repository ({})",
            certification.repository
        );
    }
    if let Some(release) = release
        && certification.tag != release.tag_name
    {
        bail!("{channel} certification does not match its release");
    }
    validate_revision(&certification.revision)?;
    if let Some(minimum) = &minimum_cli_version(&certification.metadata)? {
        let update = match channel {
            FrameworkChannel::Stable => registry_cli_update(minimum),
            FrameworkChannel::Dev | FrameworkChannel::Nightly => checkout_cli_update(),
        };
        validate_installed_cli(minimum, &update)?;
    }
    Ok(())
}

/// A submodule the resolved revision pins: the repository `.gitmodules`
/// names for the path and the commit the revision's gitlink — or a certified
/// channel's certification — records.
struct SubmodulePin {
    /// The submodule's repository, canonicalized like [`framework_repository`].
    repository: String,
    /// The pinned commit.
    commit: String,
}

/// Rebase a fetched root manifest's `[patch]` tables onto the channel's own
/// sources: a path entry under one of the revision's submodules becomes
/// `git + rev` on the submodule's repository at the recorded commit, and any
/// other path entry becomes `git + rev` on the framework repository at the
/// resolved revision.
///
/// A fetched table keyed on the framework repository itself is dropped in
/// any spelling — Cargo canonicalizes a source's query, fragment, `.git`
/// suffix and trailing slash away, so every one names the patched source
/// itself, and a patch may not point at the source it patches. No
/// repository-source mirror is synthesized for the path entries either:
/// mirroring them at the channel's revision was the same-source patch Cargo
/// rejects (#807), and the extracted crates that once named framework
/// crates by `git` (#758) are consumed from the registry, where
/// `[patch.crates-io]` already applies.
fn rebase_patches_onto_source(
    mut patches: PatchSet,
    repository: &str,
    revision: &str,
    submodules: &BTreeMap<String, SubmodulePin>,
) -> PatchSet {
    patches.retain(|source, _| !same_git_source(source, repository));
    for dependencies in patches.values_mut() {
        for dependency in dependencies.values_mut() {
            let Dependency::Detailed(detail) = dependency else {
                continue;
            };
            let Some(path) = detail.path.take() else {
                continue;
            };
            let path = path.trim_start_matches("./");
            let pin = submodules.iter().find_map(|(root, pin)| {
                (path == root.as_str() || path.starts_with(&format!("{root}/"))).then_some(pin)
            });
            let (git, rev) = pin.map_or((repository, revision), |pin| {
                (pin.repository.as_str(), pin.commit.as_str())
            });
            detail.git = Some(git.to_owned());
            detail.rev = Some(rev.to_owned());
        }
    }
    patches
}

/// A git URL in the spelling Cargo canonicalizes sources to: the query,
/// fragment, `.git` suffix and trailing slash carry no meaning.
fn canonical_git_url(url: &str) -> &str {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    url.trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/')
}

/// Whether two URLs name the same git source — `repo?branch=dev`, `repo.git`
/// and `repo` canonicalize to one source, so a `[patch]` table keyed on any
/// of them patches the framework repository itself.
fn same_git_source(source: &str, repository: &str) -> bool {
    canonical_git_url(source) == canonical_git_url(repository)
}

#[cfg(test)]
mod tests {
    use test_fixtures::{
        package, stable_framework, test_lock, write_apple_revision_checkout, write_local_checkout,
        write_pre_decoupling_checkout,
    };

    use super::*;

    fn snapshot(lock: &Lockfile) -> (ResolvedFramework, Vec<u8>) {
        let bytes = lock.to_string().into_bytes();
        let scaffold = lock
            .packages
            .iter()
            .map(|package| {
                (
                    format!("{}-version", package.name),
                    package.version.to_string(),
                )
            })
            .collect();
        let repository = framework_repository();
        let revision = "a".repeat(40);
        let framework = ResolvedFramework {
            source: Source::Nightly {
                repository: repository.to_owned(),
                revision: revision.clone(),
                tag: "nightly-test".into(),
                lock_sha256: hex::encode(Sha256::digest(&bytes)),
            },
            minimum_cli_version: None,
            metadata: toml::toml! {
                android-min-api-level = 26
            },
            packages: resolve_packages(&scaffold, lock, repository, &revision).unwrap(),
            scaffold,
            patches: PatchSet::default(),
        };
        (framework, bytes)
    }

    #[test]
    fn cli_requirement_uses_semver_precedence() {
        for (minimum, current, compatible) in [
            ("0.1.4", "0.1.3", false),
            ("0.1.4", "0.1.4", true),
            ("0.1.9", "0.1.10", true),
            ("0.1.4", "0.1.4-rc.1", false),
            ("0.1.4-rc.1", "0.1.4-rc.2", true),
            ("0.1.4+z", "0.1.4+a", true),
            ("0.1.4", "1.0.0", true),
        ] {
            let minimum = minimum.parse().unwrap();
            let current = current.parse().unwrap();
            let update = registry_cli_update(&minimum);
            let result = validate_cli_version(&minimum, &current, &update);
            assert_eq!(result.is_ok(), compatible, "{current} against {minimum}");
            if let Err(error) = result {
                let message = error.to_string();
                assert!(message.contains(&minimum.to_string()));
                assert!(message.contains(&current.to_string()));
                assert!(message.contains(&update));
                assert!(message.contains("water --version"));
            }
        }
    }

    #[test]
    fn cli_requirement_metadata_rejects_invalid_versions() {
        let mut metadata = toml::toml! {
            minimum-cli-version = "0.1.4"
        };
        assert_eq!(
            minimum_cli_version(&metadata).unwrap(),
            Some("0.1.4".parse().unwrap())
        );
        metadata["minimum-cli-version"] = toml::Value::String(">=0.1.4".into());
        assert!(minimum_cli_version(&metadata).is_err());
        assert!(minimum_cli_version(&toml::Table::new()).unwrap().is_none());
    }

    #[test]
    fn android_min_api_level_is_required_framework_metadata() {
        assert_eq!(stable_framework().android_min_api_level().unwrap(), 26);

        let mut missing = stable_framework();
        missing.metadata.remove("android-min-api-level");
        let error = missing.android_min_api_level().unwrap_err().to_string();
        assert!(error.contains("android-min-api-level"), "{error}");
        assert!(error.contains("v0.4.1"), "{error}");

        let mut invalid = stable_framework();
        invalid.metadata["android-min-api-level"] = toml::Value::String("26".to_owned());
        let error = invalid.android_min_api_level().unwrap_err().to_string();
        assert!(error.contains("android-min-api-level"), "{error}");
    }

    #[test]
    fn persisted_cli_requirement_blocks_an_older_cli_with_update_guidance() {
        let mut minimum: cargo_toml::SemVer = env!("CARGO_PKG_VERSION").parse().unwrap();
        minimum.major += 1;
        let mut framework = stable_framework();
        framework.minimum_cli_version = Some(minimum.clone());
        let contents = toml::to_string(&framework).unwrap();
        let framework: ResolvedFramework = toml::from_str(&contents).unwrap();
        let error = framework.validate_cli().unwrap_err().to_string();
        assert!(error.contains(&registry_cli_update(&minimum)));
        assert_eq!(framework.minimum_cli_version, Some(minimum));
    }

    #[test]
    fn snapshot_preserves_independent_package_sources() {
        let backend_revision = "b".repeat(40);
        let backend_source =
            format!("git+https://example.com/hydrolysis?rev={backend_revision}#{backend_revision}");
        let lock = Lockfile {
            packages: vec![
                package("waterui", "0.3.0", None),
                package("hydrolysis", "0.1.0", Some(&backend_source)),
                package(
                    "hydrolysis-m3",
                    "0.1.0",
                    Some("registry+https://github.com/rust-lang/crates.io-index"),
                ),
            ],
            version: cargo_lock::ResolveVersion::V4,
            root: None,
            metadata: BTreeMap::default(),
            patch: cargo_lock::Patch::default(),
        };
        let (framework, _) = snapshot(&lock);
        let persisted = toml::to_string(&framework).unwrap();
        let framework: ResolvedFramework = toml::from_str(&persisted).unwrap();
        assert_eq!(framework.channel(), Some(FrameworkChannel::Nightly));
        assert_eq!(framework.dependency("waterui").rev, Some("a".repeat(40)));
        let backend = framework.dependency("hydrolysis");
        assert_eq!(
            backend.git.as_deref(),
            Some("https://example.com/hydrolysis")
        );
        assert_eq!(backend.rev, Some(backend_revision));
        let theme = framework.dependency("hydrolysis-m3");
        assert!(theme.git.is_none());
        assert_eq!(theme.version.unwrap().to_string(), "=0.1.0");
    }

    #[test]
    fn extracted_crate_absent_from_the_lock_resolves_to_its_declared_requirement() {
        // A crate released from its own repository and not consumed by the
        // framework never enters the framework lock — the scaffold's declared
        // requirement is the requirement a dev/nightly resolution pins,
        // whether the workspace names a version or a git revision.
        let lock = Lockfile {
            packages: vec![package("waterui", "0.3.0", None)],
            version: cargo_lock::ResolveVersion::V4,
            root: None,
            metadata: BTreeMap::default(),
            patch: cargo_lock::Patch::default(),
        };
        let gtk_revision = "b".repeat(40);
        let scaffold = BTreeMap::from([
            ("waterui-version".to_string(), "0.3.0".to_string()),
            ("waterui-dew-version".to_string(), "0.2.1".to_string()),
            ("waterui-gtk-version".to_string(), "0.2.0".to_string()),
            (
                "waterui-gtk-git".to_string(),
                "https://github.com/water-rs/gtk-backend".to_string(),
            ),
            ("waterui-gtk-rev".to_string(), gtk_revision.clone()),
        ]);
        let packages =
            resolve_packages(&scaffold, &lock, framework_repository(), &"a".repeat(40)).unwrap();
        assert!(packages["waterui"].git.is_some());
        let dew = &packages["waterui-dew"];
        assert!(dew.git.is_none());
        assert_eq!(dew.version.as_ref().unwrap().to_string(), "=0.2.1");
        let gtk = &packages["waterui-gtk"];
        assert_eq!(
            gtk.git.as_deref(),
            Some("https://github.com/water-rs/gtk-backend")
        );
        assert_eq!(gtk.rev.as_deref(), Some(gtk_revision.as_str()));
        assert_eq!(gtk.version.as_ref().unwrap().to_string(), "^0.2.0");
    }

    #[test]
    fn declared_git_source_must_agree_with_the_lock() {
        // The declared pin is authoritative, but a framework that also builds
        // the crate in-tree must not lock a different commit than it declares.
        let locked_revision = "b".repeat(40);
        let drifted_revision = "c".repeat(40);
        for (lock_revision, expected) in [
            (locked_revision.as_str(), true),
            (drifted_revision.as_str(), false),
        ] {
            let source = format!(
                "git+https://github.com/water-rs/gtk-backend?rev={lock_revision}#{lock_revision}"
            );
            let lock = Lockfile {
                packages: vec![package("waterui-gtk", "0.2.0", Some(&source))],
                version: cargo_lock::ResolveVersion::V4,
                root: None,
                metadata: BTreeMap::default(),
                patch: cargo_lock::Patch::default(),
            };
            let scaffold = BTreeMap::from([
                ("waterui-gtk-version".to_string(), "0.2.0".to_string()),
                (
                    "waterui-gtk-git".to_string(),
                    "https://github.com/water-rs/gtk-backend".to_string(),
                ),
                ("waterui-gtk-rev".to_string(), locked_revision.clone()),
            ]);
            let result =
                resolve_packages(&scaffold, &lock, framework_repository(), &"a".repeat(40));
            assert_eq!(result.is_ok(), expected, "lock revision {lock_revision}");
            if expected {
                assert_eq!(
                    result.unwrap()["waterui-gtk"].rev.as_deref(),
                    Some(locked_revision.as_str())
                );
            }
        }
    }

    #[test]
    fn stable_dependency_honors_a_declared_git_source() {
        let mut framework = stable_framework();
        let revision = "b".repeat(40);
        framework.scaffold.insert(
            "waterui-gtk-git".to_owned(),
            "https://github.com/water-rs/gtk-backend".to_owned(),
        );
        framework
            .scaffold
            .insert("waterui-gtk-rev".to_owned(), revision.clone());
        let gtk = framework.dependency("waterui-gtk");
        assert_eq!(
            gtk.git.as_deref(),
            Some("https://github.com/water-rs/gtk-backend")
        );
        assert_eq!(gtk.rev.as_deref(), Some(revision.as_str()));
        assert_eq!(gtk.version.as_ref().unwrap().to_string(), "^0.1.2");
        // A scaffold package declared by version alone still resolves the
        // registry pin.
        let dew = framework.dependency("waterui-dew");
        assert!(dew.git.is_none());
        assert_eq!(dew.version.as_ref().unwrap().to_string(), "=0.2.1");
    }

    #[test]
    fn extracted_crate_validation_accepts_only_the_sanctioned_source() {
        // `Water.lock` cannot record an extracted crate, so
        // `validate_dependencies` holds it to its declared pin instead: the
        // exact commit for a git source, the exact version for the registry.
        let gtk_revision = "b".repeat(40);
        let mut framework = stable_framework();
        framework.packages.insert(
            "waterui-gtk".to_owned(),
            DependencyDetail {
                version: Some("0.2.0".parse().unwrap()),
                git: Some("https://github.com/water-rs/gtk-backend".to_owned()),
                rev: Some(gtk_revision.clone()),
                ..Default::default()
            },
        );
        framework.packages.insert(
            "waterui-dew".to_owned(),
            DependencyDetail {
                version: Some("=0.2.1".parse().unwrap()),
                ..Default::default()
            },
        );
        let identity = |name: &str, version: &str, source: String| LockedPackage {
            name: name.to_owned(),
            version: version.to_owned(),
            source: Some(source),
        };
        let gtk_source = |revision: &str| {
            format!("git+https://github.com/water-rs/gtk-backend?rev={revision}#{revision}")
        };
        assert!(framework.sanctioned_source(&identity(
            "waterui-gtk",
            "0.2.0",
            gtk_source(&gtk_revision)
        )));
        // The pinned commit carries whatever version its manifest declares.
        assert!(framework.sanctioned_source(&identity(
            "waterui-gtk",
            "0.2.1",
            gtk_source(&gtk_revision)
        )));
        // A different commit is a different pin.
        let drifted = "c".repeat(40);
        assert!(!framework.sanctioned_source(&identity(
            "waterui-gtk",
            "0.2.0",
            gtk_source(&drifted)
        )));
        // The registry pin holds only its exact version.
        let registry = || "registry+https://github.com/rust-lang/crates.io-index".to_owned();
        assert!(framework.sanctioned_source(&identity("waterui-dew", "0.2.1", registry())));
        assert!(!framework.sanctioned_source(&identity("waterui-dew", "0.2.2", registry())));
    }

    /// The exact requirement a stable channel writes for one scaffold entry.
    ///
    /// Spelling the number out here would put a third copy of it beside the two
    /// the manifest and the workspace already keep in step (#548), and it would
    /// have to be edited on every release.
    fn scaffolded(field: &str) -> String {
        let version = &stable_framework().scaffold[field];
        format!("={version}")
    }

    #[test]
    fn channel_update_preserves_aliases_features_and_unrelated_dependencies() {
        let manifest = toml::toml! {
            [dependencies.ui]
            package = "waterui"
            path = "../waterui"
            default-features = false
            features = ["gpu"]
            [dependencies.serde]
            version = "1"
            features = ["derive"]
            [target."cfg(unix)".build-dependencies]
            waterui-core = "0.2"
        };
        let mut document = toml_edit::ser::to_document(&manifest).unwrap();
        stable_framework()
            .update_manifest(&mut document, &PatchSet::default())
            .unwrap();
        assert_eq!(
            document["dependencies"]["ui"]["package"].as_str(),
            Some("waterui")
        );
        assert!(document["dependencies"]["ui"].get("path").is_none());
        assert_eq!(
            document["dependencies"]["ui"]["version"].as_str(),
            Some(scaffolded("waterui-version").as_str())
        );
        assert_eq!(
            document["dependencies"]["ui"]["default-features"].as_bool(),
            Some(false)
        );
        assert_eq!(
            document["dependencies"]["ui"]["features"][0].as_str(),
            Some("gpu")
        );
        let updated: toml::Value = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(
            updated["dependencies"]["serde"],
            manifest["dependencies"]["serde"]
        );
        assert_eq!(
            updated["target"]["cfg(unix)"]["build-dependencies"]["waterui-core"]["version"]
                .as_str(),
            Some(scaffolded("waterui-core-version").as_str())
        );
    }

    #[test]
    fn channel_update_writes_patches_as_tables_and_clears_stale_ones() {
        let mut document: toml_edit::DocumentMut =
            "[package]\nname = \"app\"\n\n[dependencies]\nwaterui = \"0.3.0\"\n"
                .parse()
                .unwrap();
        let (dev, _) = snapshot(&Lockfile {
            packages: vec![package("waterui", "0.3.0", None)],
            version: cargo_lock::ResolveVersion::V4,
            root: None,
            metadata: BTreeMap::default(),
            patch: cargo_lock::Patch::default(),
        });
        let mut dev = dev;
        let vello: Dependency = toml::from_str::<toml::Value>(
            r#"git = "https://github.com/lexoliu/vello"
rev = "d68d9e9825bcd1ffee762323881c13a2e7a3f639""#,
        )
        .unwrap()
        .try_into()
        .unwrap();
        dev.patches
            .entry("crates-io".into())
            .or_default()
            .insert("vello".into(), vello);
        dev.update_manifest(&mut document, &PatchSet::default())
            .unwrap();
        let rendered = document.to_string();
        assert!(rendered.starts_with("[package]"), "{rendered}");
        assert!(rendered.contains("[patch.crates-io]\n"), "{rendered}");
        assert!(!rendered.contains("\n[patch]\n"), "{rendered}");
        assert_eq!(
            document["patch"]["crates-io"]["vello"]["rev"].as_str(),
            Some("d68d9e9825bcd1ffee762323881c13a2e7a3f639")
        );
        assert_eq!(
            document["dependencies"]["waterui"]["rev"].as_str(),
            Some("a".repeat(40).as_str())
        );

        stable_framework()
            .update_manifest(&mut document, &dev.patches())
            .unwrap();
        let rendered = document.to_string();
        assert!(!rendered.contains("patch"), "{rendered}");
        assert_eq!(
            document["dependencies"]["waterui"]["version"].as_str(),
            Some(scaffolded("waterui-version").as_str())
        );
    }

    #[test]
    fn snapshot_lock_preserves_external_sources_and_rewrites_local_edges() {
        let core = package("waterui-core", "0.3.0", None);
        let mut facade = package("waterui", "0.3.0", None);
        facade.dependencies.push(LockedDependency::from(&core));
        let theme = package(
            "hydrolysis-m3",
            "0.1.0",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        );
        let lock = Lockfile {
            packages: vec![facade, core, theme.clone()],
            version: cargo_lock::ResolveVersion::V4,
            root: None,
            metadata: BTreeMap::default(),
            patch: cargo_lock::Patch::default(),
        };
        let (framework, bytes) = snapshot(&lock);
        let resolved = framework.cargo_lock(&bytes).unwrap();
        let facade = resolved
            .packages
            .iter()
            .find(|package| package.name.as_str() == "waterui")
            .unwrap();
        let core = resolved
            .packages
            .iter()
            .find(|package| package.name.as_str() == "waterui-core")
            .unwrap();
        assert_eq!(facade.dependencies, vec![LockedDependency::from(core)]);
        assert!(core.source.as_ref().unwrap().is_git());
        assert!(resolved.packages.contains(&theme));
        let mut changed = bytes;
        changed.push(b'\n');
        assert!(
            framework
                .cargo_lock(&changed)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
    }

    #[test]
    fn rebase_patches_onto_source_redirects_git_source_dependencies() {
        let mut patches = PatchSet::default();
        let mut crates_io = std::collections::BTreeMap::new();
        crates_io.insert(
            "waterui-core".to_string(),
            Dependency::Detailed(Box::new(DependencyDetail {
                path: Some("core".to_string()),
                ..DependencyDetail::default()
            })),
        );
        crates_io.insert(
            "waterkit-audio".to_string(),
            Dependency::Detailed(Box::new(DependencyDetail {
                path: Some("kit/multimedia/audio".to_string()),
                ..DependencyDetail::default()
            })),
        );
        crates_io.insert(
            "vello".to_string(),
            Dependency::Detailed(Box::new(DependencyDetail {
                git: Some("https://github.com/lexoliu/vello".to_string()),
                rev: Some("5e5f538556be16527f67379b105af82f408b747d".to_string()),
                ..DependencyDetail::default()
            })),
        );
        patches.insert("crates-io".to_string(), crates_io);
        // A fetched table keyed on the framework repository itself — in any
        // spelling Cargo canonicalizes to it — must not survive: a patch may
        // not point at the source it patches.
        patches.insert(
            "https://github.com/water-rs/waterui".to_string(),
            std::collections::BTreeMap::new(),
        );
        patches.insert(
            "https://github.com/water-rs/waterui.git?branch=dev".to_string(),
            std::collections::BTreeMap::new(),
        );

        let submodules = BTreeMap::from([(
            "kit".to_string(),
            SubmodulePin {
                repository: "https://github.com/water-rs/waterkit".to_string(),
                commit: "98c89ee702c5629094030023fb8d55464592d35d".to_string(),
            },
        )]);
        let rebased = rebase_patches_onto_source(
            patches,
            "https://github.com/water-rs/waterui",
            "475b4bb884a5f4e2b1156f1af74c40feaf71fdc1",
            &submodules,
        );

        // The crates-io path entry became a git pin at the channel revision.
        let Dependency::Detailed(core) = &rebased["crates-io"]["waterui-core"] else {
            panic!("a path patch stays a detailed dependency");
        };
        assert!(core.path.is_none());
        assert_eq!(
            core.git.as_deref(),
            Some("https://github.com/water-rs/waterui")
        );
        assert_eq!(
            core.rev.as_deref(),
            Some("475b4bb884a5f4e2b1156f1af74c40feaf71fdc1")
        );

        // A path under a submodule rebases onto the submodule's repository at
        // the pinned commit — the superproject holds a gitlink, not the crate.
        let Dependency::Detailed(audio) = &rebased["crates-io"]["waterkit-audio"] else {
            panic!("a submodule path patch stays a detailed dependency");
        };
        assert_eq!(
            audio.git.as_deref(),
            Some("https://github.com/water-rs/waterkit")
        );
        assert_eq!(
            audio.rev.as_deref(),
            Some("98c89ee702c5629094030023fb8d55464592d35d")
        );

        // Dependencies patched to another source stay untouched, and no
        // repository-source table is synthesized — it would patch a source
        // onto itself.
        let Dependency::Detailed(vello) = &rebased["crates-io"]["vello"] else {
            panic!("a git patch stays a detailed dependency");
        };
        assert_eq!(
            vello.git.as_deref(),
            Some("https://github.com/lexoliu/vello")
        );
        assert!(!rebased.contains_key("https://github.com/water-rs/waterui"));
        assert!(!rebased.contains_key("https://github.com/water-rs/waterui.git?branch=dev"));
    }

    #[test]
    fn coherence_admits_a_submodule_crate_at_either_source() {
        let revision = "a".repeat(40);
        let pin = "98c89ee702c5629094030023fb8d55464592d35d";
        let framework_source = format!("git+{}?rev={revision}#{revision}", framework_repository());
        let pin_source = format!("git+https://github.com/water-rs/waterkit?rev={pin}#{pin}");
        let (mut framework, _) = snapshot(&test_lock());
        framework
            .patches
            .entry("crates-io".into())
            .or_default()
            .insert(
                "waterkit-codec".into(),
                Dependency::Detailed(Box::new(DependencyDetail {
                    git: Some("https://github.com/water-rs/waterkit".into()),
                    rev: Some(pin.into()),
                    ..DependencyDetail::default()
                })),
            );
        framework
            .patches
            .entry("crates-io".into())
            .or_default()
            .insert(
                "waterkit-fs".into(),
                Dependency::Detailed(Box::new(DependencyDetail {
                    git: Some("https://github.com/water-rs/waterkit".into()),
                    rev: Some(pin.into()),
                    ..DependencyDetail::default()
                })),
            );
        let packages = vec![
            // Recorded at the framework's own source — a submodule path dep
            // cargo vendors in-source.
            package("waterkit-codec", "0.1.1", Some(&framework_source)),
            // Recorded at the submodule repository the patch pins it to.
            package("waterkit-fs", "0.1.1", Some(&pin_source)),
            package(
                "serde",
                "1.0.0",
                Some("registry+https://github.com/rust-lang/crates.io-index"),
            ),
        ];
        let allowed = framework.allowed_packages(&packages);
        let identity = |name: &str, source: &str| LockedPackage {
            name: name.to_owned(),
            version: "0.1.1".to_owned(),
            source: Some(source.to_owned()),
        };
        assert!(allowed.contains(&identity("waterkit-codec", &framework_source)));
        assert!(allowed.contains(&identity("waterkit-codec", &pin_source)));
        assert!(allowed.contains(&identity("waterkit-fs", &pin_source)));
        assert!(allowed.contains(&identity("waterkit-fs", &framework_source)));
        // A registry package gains no variants.
        assert_eq!(
            allowed
                .iter()
                .filter(|package| package.name == "serde")
                .count(),
            1
        );
    }

    #[test]
    fn parse_gitmodules_reads_submodule_paths_and_urls() {
        let submodules = parse_gitmodules(
            "[submodule \"backends/android\"]\n\
             \tpath = backends/android\n\
             \turl = https://github.com/water-rs/android-backend.git\n\
             \tbranch = dev\n\
             [submodule \"kit\"]\n\
             \tpath = kit\n\
             \turl = \"https://github.com/water-rs/waterkit.git\"\n",
        );
        assert_eq!(
            submodules,
            BTreeMap::from([
                (
                    "backends/android".to_string(),
                    "https://github.com/water-rs/android-backend.git".to_string(),
                ),
                (
                    "kit".to_string(),
                    "https://github.com/water-rs/waterkit.git".to_string(),
                ),
            ])
        );
    }

    fn release(tag: &str, draft: bool, prerelease: bool, published_at: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            draft,
            prerelease,
            published_at: Some(published_at.to_owned()),
            assets: vec![ReleaseAsset {
                name: "framework.json".to_owned(),
                browser_download_url: format!(
                    "https://github.com/water-rs/waterui/releases/download/{tag}/framework.json"
                ),
            }],
        }
    }

    #[test]
    fn stable_release_selection_takes_the_newest_stable_tag() {
        let releases = vec![
            release("v0.4.0", false, false, "2025-10-01T00:00:00Z"),
            // A prerelease tag and a nightly prerelease are not stable
            // distributions no matter how recent.
            release("v0.5.0-rc.1", true, false, "2025-12-01T00:00:00Z"),
            release("nightly-2025-12-01", false, true, "2025-12-02T00:00:00Z"),
            release("v0.4.1", false, false, "2025-11-01T00:00:00Z"),
            release("v0.9.9", true, false, "2025-12-03T00:00:00Z"),
            // Build metadata is not a stable distribution either.
            release("v0.6.0+build.5", false, false, "2025-12-04T00:00:00Z"),
            // A backport published after a newer version does not outrank it:
            // stable is ordered by version, not by publication date.
            release("v0.3.9", false, false, "2025-12-05T00:00:00Z"),
        ];
        let eligible: Vec<_> = releases
            .into_iter()
            .filter(|release| release_matches(release, FrameworkChannel::Stable))
            .collect();
        let release = newest_release(eligible, FrameworkChannel::Stable).unwrap();
        assert_eq!(release.tag_name, "v0.4.1");
        assert_eq!(
            certification_asset(&release, FrameworkChannel::Stable)
                .unwrap()
                .name,
            "framework.json"
        );
    }

    #[test]
    fn stable_release_without_a_manifest_reports_it_predates_publishing() {
        let mut latest = release("v0.4.1", false, false, "2025-11-01T00:00:00Z");
        latest.assets.clear();
        let releases = vec![
            release("v0.4.0", false, false, "2025-10-01T00:00:00Z"),
            latest,
        ];
        let eligible: Vec<_> = releases
            .into_iter()
            .filter(|release| release_matches(release, FrameworkChannel::Stable))
            .collect();
        let release = newest_release(eligible, FrameworkChannel::Stable).unwrap();
        let error = certification_asset(&release, FrameworkChannel::Stable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("v0.4.1"), "{error}");
        assert!(error.contains("predates manifest publishing"), "{error}");
    }

    fn certification(channel: FrameworkChannel, tag: &str) -> Certification {
        Certification {
            schema_version: 2,
            channel,
            repository: "water-rs/waterui".to_owned(),
            revision: "a".repeat(40),
            tag: tag.to_owned(),
            lockfiles: BTreeMap::from([("Cargo.lock".to_owned(), "f".repeat(64))]),
            submodules: BTreeMap::new(),
            scaffold: BTreeMap::new(),
            metadata: toml::toml! {
                android-min-api-level = 26
            },
        }
    }

    #[test]
    fn certification_verification_rejects_uncertified_or_mismatched_manifests() {
        let repository = framework_repository();
        let release = release("v0.4.1", false, false, "2025-11-01T00:00:00Z");

        let dev = certification(FrameworkChannel::Dev, "dev");
        assert!(
            verify_certification(&dev, None, repository)
                .unwrap_err()
                .to_string()
                .contains("dev")
        );

        let mut wrong_schema = certification(FrameworkChannel::Stable, "v0.4.1");
        wrong_schema.schema_version = 1;
        assert!(
            verify_certification(&wrong_schema, None, repository)
                .unwrap_err()
                .to_string()
                .contains("schema")
        );
        let nightly_on_a_stable_tag = certification(FrameworkChannel::Nightly, "v0.4.1");
        let error = certifies_channel(&nightly_on_a_stable_tag, FrameworkChannel::Stable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("certifies the nightly channel"), "{error}");
        certifies_channel(&nightly_on_a_stable_tag, FrameworkChannel::Nightly).unwrap();

        // A schema-1 manifest has no `metadata`; the schema is still what the
        // error names, not the field the newer schema happens to require.
        let error = parse_certification(
            br#"{"schema_version": 1, "channel": "nightly", "repository": "water-rs/waterui"}"#,
        )
        .err()
        .expect("a schema-1 manifest is rejected")
        .to_string();
        assert!(
            error.contains("schema version 1 is not supported"),
            "{error}"
        );

        let mut wrong_repository = certification(FrameworkChannel::Stable, "v0.4.1");
        wrong_repository.repository = "water-rs/android-backend".to_owned();
        assert!(
            verify_certification(&wrong_repository, None, repository)
                .unwrap_err()
                .to_string()
                .contains("water-rs/android-backend")
        );

        let wrong_tag = certification(FrameworkChannel::Stable, "v0.4.0");
        assert!(
            verify_certification(&wrong_tag, Some(&release), repository)
                .unwrap_err()
                .to_string()
                .contains("does not match its release")
        );

        // A manifest read from disk has no release; the tag check is skipped.
        verify_certification(&wrong_tag, None, repository).unwrap();

        let stable = certification(FrameworkChannel::Stable, "v0.4.1");
        verify_certification(&stable, Some(&release), repository).unwrap();
    }

    #[test]
    fn manifest_loading_verifies_a_certification_from_disk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("framework.json");
        let manifest = serde_json::json!({
            "schema_version": 2,
            "channel": "stable",
            "repository": "water-rs/waterui",
            "revision": "a".repeat(40),
            "tag": "v0.4.1",
            "lockfiles": {"Cargo.lock": "f".repeat(64)},
            "submodules": {
                "backends/apple": "b".repeat(40),
                "backends/android": "c".repeat(40),
            },
            "scaffold": {
                "hydrolysis-version": "0.2.1",
                "hydrolysis-m3-version": "0.2.0",
                "waterui-dew-version": "0.2.1",
                "waterui-gtk-version": "0.1.2",
                "apple-backend-url": "https://github.com/water-rs/apple-backend.git",
                "android-backend-url": "https://github.com/water-rs/android-backend.git",
            },
            "metadata": {
                "minimum-cli-version": "0.1.0",
                "android-min-api-level": 26,
            },
        });
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let repository = framework_repository();
        let certification = smol::block_on(load_manifest(&path, repository)).unwrap();
        assert_eq!(certification.channel, FrameworkChannel::Stable);
        assert_eq!(certification.tag, "v0.4.1");

        std::fs::write(&path, b"not json").unwrap();
        assert!(smol::block_on(load_manifest(&path, repository)).is_err());
    }

    /// The Rust scaffold derivation and `framework_manifest.py`'s must produce
    /// the same table for the same tree — this asserts the Rust side against
    /// the fixture manifest, which carries the framework root manifest's
    /// metadata table and the workspace requirements `scaffold-packages`
    /// names.
    #[test]
    fn framework_scaffold_derives_from_the_framework_manifest() {
        let root: toml::Value = toml::from_str(include_str!(
            "../../tests/fixtures/framework_checkout_manifest.toml"
        ))
        .unwrap();
        let scaffold = framework_scaffold(&root).unwrap();
        let workspace = |name: &str| {
            let dependency = &root["workspace"]["dependencies"][name];
            dependency
                .as_str()
                .or_else(|| dependency.get("version").and_then(toml::Value::as_str))
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            scaffold,
            BTreeMap::from([
                ("hydrolysis-version".to_owned(), workspace("hydrolysis")),
                (
                    "hydrolysis-m3-version".to_owned(),
                    workspace("hydrolysis-m3")
                ),
                ("waterui-dew-version".to_owned(), workspace("waterui-dew")),
                ("waterui-gtk-version".to_owned(), workspace("waterui-gtk")),
                (
                    "waterui-gtk-git".to_owned(),
                    "https://github.com/water-rs/gtk-backend".to_owned()
                ),
                (
                    "waterui-gtk-rev".to_owned(),
                    "3162043e618e759bea6d6e52ec75c6ee1273c080".to_owned()
                ),
                (
                    "apple-backend-url".to_owned(),
                    "https://github.com/water-rs/apple-backend.git".to_owned()
                ),
                ("apple-backend-version".to_owned(), "0.3.0-dev.2".to_owned()),
                (
                    "android-backend-url".to_owned(),
                    "https://github.com/water-rs/android-backend.git".to_owned()
                ),
                ("android-backend-revision".to_owned(), "c".repeat(40)),
            ])
        );
    }

    #[test]
    fn framework_scaffold_rejects_a_git_package_without_a_revision() {
        let mut root: toml::Value = toml::from_str(include_str!(
            "../../tests/fixtures/framework_checkout_manifest.toml"
        ))
        .unwrap();
        let gtk = &mut root["workspace"]["dependencies"]["waterui-gtk"];
        gtk.as_table_mut()
            .unwrap()
            .insert("branch".to_owned(), toml::Value::String("dev".to_owned()));
        gtk.as_table_mut().unwrap().remove("rev");
        let error = framework_scaffold(&root).unwrap_err();
        assert!(error.to_string().contains("waterui-gtk"), "{error:?}");
    }

    #[test]
    fn framework_scaffold_rejects_a_backend_revision_that_is_not_a_commit() {
        let mut root: toml::Value = toml::from_str(include_str!(
            "../../tests/fixtures/framework_checkout_manifest.toml"
        ))
        .unwrap();
        root["package"]["metadata"]["waterui"]["android-backend-revision"] =
            toml::Value::String("dev".to_owned());
        let error = framework_scaffold(&root).unwrap_err();
        assert!(
            error.to_string().contains("android-backend-revision"),
            "{error:?}"
        );
    }

    #[test]
    fn declared_apple_revision_resolves_without_a_gitlink() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("waterui");
        let revision = "d".repeat(40);
        write_apple_revision_checkout(&root, &revision);

        let framework = smol::block_on(ResolvedFramework::for_local_checkout(&root)).unwrap();
        assert_eq!(framework.apple_backend_revision(), Some(revision.as_str()));
        assert!(framework.apple_backend_version().is_none());
        assert!(framework.git_source().is_none());
    }

    #[test]
    fn local_checkout_resolves_from_its_own_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("waterui");
        write_local_checkout(&root);
        let framework = smol::block_on(ResolvedFramework::for_local_checkout(&root)).unwrap();
        assert_eq!(framework.channel(), None);
        assert_eq!(framework.scaffold_value("hydrolysis-version"), "0.2.1");
        assert_eq!(
            framework.scaffold_value("apple-backend-version"),
            "0.3.0-dev.2"
        );
        assert_eq!(
            framework.scaffold_value("android-backend-revision"),
            "c".repeat(40)
        );
        assert_eq!(framework.scaffold_value("waterui-version"), "0.4.1");
        assert!(framework.git_source().is_none());
    }

    /// A checkout from before the backends left the tree: its manifest
    /// declares neither `apple-backend-version` nor
    /// `android-backend-revision`, so the gitlinks supply the pins.
    #[test]
    fn local_checkout_predating_the_gitlink_removals_uses_its_pins() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("waterui");
        write_pre_decoupling_checkout(&root);

        let framework = smol::block_on(ResolvedFramework::for_local_checkout(&root)).unwrap();
        assert_eq!(
            framework.scaffold_value("apple-backend-revision"),
            "b".repeat(40)
        );
        assert_eq!(
            framework.scaffold_value("android-backend-revision"),
            "c".repeat(40)
        );
    }
}
