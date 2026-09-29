use std::path::{Path, PathBuf};

use eyre::WrapErr as _;
use serde::{Deserialize, Serialize};
use waterui_assets_planner::ColorScheme;

use crate::{
    apple::platform::{build_rust_lib, clean_apple, is_apple_platform, package_apple},
    backend::Backend,
    build::BuildOptions,
    device::Artifact,
    platform::{PackageOptions, TargetBackend, TargetPlatform},
    project::Project,
    project_types::CrateName,
    templates::{self, TemplateContext},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
/// Configuration for the Apple backend in a `WaterUI` project.
///
/// `[backends.apple]` in `Water.toml` persists only `backend_path`; the
/// project path and scheme describe the Xcode project the CLI generates in
/// the managed build cache.
pub struct AppleBackend {
    /// Path to the generated Apple project below the managed backends root.
    #[serde(skip, default = "default_apple_project_path")]
    pub project_path: PathBuf,
    /// The scheme to use for building the Apple project.
    #[serde(skip)]
    pub scheme: String,
    /// Local path to the Apple backend for local dev.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_path: Option<String>,
}

/// What this project's built application bundle is called.
///
/// Deliberately not the scheme. The scheme is a fixed handle the CLI drives the
/// Xcode project with — every project shares one, which is what lets one set
/// of commands build any of them — while the product name is the one a person
/// reads. macOS takes `CFBundleName`, and with it the menu bar, the Dock and
/// Force Quit, from `PRODUCT_NAME`, so a project that leaves the two equal
/// announces itself as the scaffold's target rather than as itself.
///
/// The scaffold writes this same name into `PRODUCT_NAME`, so this is also
/// where the built bundle is found afterwards; the two must agree.
///
/// # Errors
///
/// Returns an error when the name cannot be a bundle's: empty, or containing a
/// path separator that would place the bundle somewhere else entirely.
pub fn apple_product_name(project: &Project) -> Result<&str, eyre::Report> {
    let name = project.manifest().package.name.as_str();
    if name.is_empty() {
        eyre::bail!("This project has no name; `package.name` in Water.toml names the application");
    }
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        eyre::bail!(
            "The project name {name:?} contains a path separator, so it cannot name an application bundle"
        );
    }
    Ok(name)
}

impl AppleBackend {
    /// Create a new Apple backend configuration with the given scheme.
    #[must_use]
    pub fn new(scheme: impl Into<String>) -> Self {
        Self {
            project_path: default_apple_project_path(),
            scheme: scheme.into(),
            backend_path: None,
        }
    }

    /// Set the local backend path for development.
    #[must_use]
    pub fn with_backend_path(mut self, path: impl Into<String>) -> Self {
        self.backend_path = Some(path.into());
        self
    }

    /// Get the path to the Apple project within the `WaterUI` project.
    #[must_use]
    pub fn project_path(&self) -> &Path {
        &self.project_path
    }
}

fn default_apple_project_path() -> PathBuf {
    PathBuf::from("apple")
}

impl AppleBackend {
    /// The `(scheme, app name, crate name)` the scaffold renders with: every
    /// generated Apple project is the shared `WaterUIApp` host, and the scheme
    /// must match its Xcode target name.
    fn scaffold_names() -> (String, String, CrateName) {
        (
            "WaterUIApp".to_string(),
            "WaterUIApp".to_string(),
            CrateName::try_from("WaterUIApp").expect("the Apple host crate name must be valid"),
        )
    }

    /// The template context [`init`] scaffolds with, rebuilt from the current
    /// manifest — what [`requires_regeneration`] diffs the generated project
    /// against.
    ///
    /// # Errors
    ///
    /// Returns an error when the application dependency graph or the
    /// framework cannot be resolved.
    pub(crate) async fn template_context(project: &Project) -> eyre::Result<TemplateContext> {
        let manifest = project.manifest();
        let (_, app_name, crate_name_for_template) = Self::scaffold_names();
        let ios_permissions = manifest
            .permissions
            .iter()
            .filter(|(_, entry)| entry.is_enabled())
            .filter_map(|(key, entry)| {
                key.ios_plist_key()
                    .map(|plist_key| templates::IosPermissionTemplateEntry {
                        plist_key,
                        description: entry.description().to_string(),
                    })
            })
            .collect();
        let webview_enabled = project.uses_standard_webview().await?;
        let chromium_enabled = project.links_runtime_package("waterui-chromium").await?;
        let browser_engine = project.linked_browser_engine().await?;
        // The generated project names the launch assets the catalog will
        // hold, so the two are decided from the same resolution.
        let launch = crate::assets::project_launch_assets(project)?;
        let launch_entry = templates::LaunchTemplateEntry {
            has_background: launch.plan().background(ColorScheme::Light).is_some(),
            has_image: launch.has_artwork(),
        };
        Ok(TemplateContext::for_project_manifest(
            manifest,
            crate_name_for_template,
            app_name,
            &project.resolved_framework().await?,
        )
        .with_backend_project_path(project.backend_path::<Self>())
        .with_project_root_path(project.root().to_path_buf())
        .with_ios_permissions(ios_permissions)
        .with_webview_enabled(webview_enabled)
        .with_chromium_enabled(chromium_enabled)
        .with_browser_engine(browser_engine)
        .with_launch(launch_entry))
    }

    /// Whether the generated Apple project differs from what the current
    /// templates and manifest would render — a `[backends.apple]`
    /// `backend_path`, `branch` or `revision` change rewrites the backend
    /// dependency the ffi crate's manifest pins.
    ///
    /// # Errors
    ///
    /// Returns an error when the template context or outputs cannot resolve.
    pub async fn requires_regeneration(project: &Project) -> eyre::Result<bool> {
        let backend_dir = project.backend_path::<Self>();
        let ctx = Self::template_context(project).await?;
        for (relative, expected) in templates::apple::rendered_outputs(&ctx)? {
            let path = backend_dir.join(&relative);
            match std::fs::read(&path) {
                Ok(existing) if existing == expected => {}
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(true);
                }
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("Failed to read {}", path.display()));
                }
            }
        }
        Ok(false)
    }
}

impl Backend for AppleBackend {
    const DEFAULT_PATH: &'static str = "apple";

    // Preserve Xcode build caches during re-scaffolding.
    const CACHE_PATHS: &'static [&'static str] = &["DerivedData"];

    fn path(&self) -> &Path {
        &self.project_path
    }

    async fn init(project: &Project) -> Result<Self, crate::backend::FailToInitBackend> {
        // A `[backends.apple]` source override the manifest already carries is
        // a user choice; init re-scaffolds the project without rewriting it.
        let existing = project.manifest().backends.apple();
        let (scheme, _, _) = Self::scaffold_names();
        let project_path = default_apple_project_path();

        let ctx = Self::template_context(project)
            .await
            .map_err(crate::backend::FailToInitBackend::Config)?;

        templates::apple::scaffold(&project.backend_path::<Self>(), &ctx)
            .await
            .map_err(crate::backend::FailToInitBackend::Io)?;

        Ok(Self {
            project_path,
            scheme,
            backend_path: existing.and_then(|backend| backend.backend_path.clone()),
        })
    }

    fn supports(&self, platform: TargetPlatform) -> bool {
        is_apple_platform(platform)
    }

    async fn build(
        &self,
        project: &Project,
        platform: TargetPlatform,
        options: BuildOptions,
    ) -> eyre::Result<crate::build::BuiltTarget> {
        project
            .browser_runtime_plan(platform, TargetBackend::Apple)
            .await?;
        build_rust_lib(project, platform, options).await
    }

    async fn package(
        &self,
        project: &Project,
        platform: TargetPlatform,
        options: PackageOptions,
        built: &crate::build::BuiltTarget,
    ) -> eyre::Result<Artifact> {
        package_apple(project, platform, options, built).await
    }

    async fn clean(&self, project: &Project, _platform: TargetPlatform) -> eyre::Result<()> {
        clean_apple(project).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::AppleBackend;
    use crate::{
        backend::reinit_backend,
        platform::TargetBackend,
        project::{CreateOptions, ManagedBackends, Project},
        project_types::BundleIdentifier,
    };

    /// The scaffold emits only the entitlements file — no Xcode project
    /// exists to regenerate — yet the staleness check still detects an edit
    /// and re-renders without losing the packaging cache.
    #[test]
    fn scaffold_without_xcode_project_still_detects_staleness() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("water-example");
        smol::block_on(Project::create(
            &root,
            CreateOptions {
                name: "Water Example".to_string(),
                bundle_identifier: BundleIdentifier::try_from("dev.waterui.waterexample")
                    .expect("bundle identifier"),
                waterui_path: None,
                channel: None,
                framework_manifest: None,
                framework: Some(crate::framework::test_fixtures::stable_framework()),
                framework_lock: None,
                author: "Lexo Liu".to_string(),
                web: None,
            },
        ))
        .expect("project creation must succeed");
        let project = smol::block_on(Project::open(
            &root,
            ManagedBackends::for_backend(TargetBackend::Apple),
        ))
        .expect("opening the project scaffolds the Apple backend");

        let backend_dir = project.backend_path::<AppleBackend>();
        assert!(
            !backend_dir
                .join("WaterUIApp.xcodeproj/project.pbxproj")
                .exists(),
            "the entry-owning scaffold produces no Xcode project"
        );
        let entitlements = backend_dir
            .join("WaterUIApp")
            .join("WaterUIApp.entitlements");
        assert!(entitlements.exists(), "the entitlements scaffolded");
        assert!(
            !smol::block_on(AppleBackend::requires_regeneration(&project))
                .expect("staleness check"),
            "a freshly scaffolded backend is not stale"
        );

        // The packaging cache must survive the re-render like it does for
        // the other generated backends.
        let derived_data = backend_dir.join("DerivedData/stale.txt");
        fs::create_dir_all(derived_data.parent().expect("parent")).expect("DerivedData");
        fs::write(&derived_data, "cache").expect("cache file");

        fs::write(&entitlements, "<plist/>").expect("edit entitlements");
        assert!(
            smol::block_on(AppleBackend::requires_regeneration(&project)).expect("staleness check"),
            "an edited scaffold file is stale"
        );
        smol::block_on(reinit_backend::<AppleBackend>(&project)).expect("reinit");
        assert!(
            !smol::block_on(AppleBackend::requires_regeneration(&project))
                .expect("staleness check"),
            "the re-rendered backend is fresh again"
        );
        assert!(derived_data.exists(), "reinit preserves DerivedData");
    }
}
