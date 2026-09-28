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
// Warn: You cannot use both revision and local_path at the same time.
/// Configuration for the Apple backend in a `WaterUI` project.
///
/// `[backends.apple]` in `Water.toml`
pub struct AppleBackend {
    #[serde(
        default = "default_apple_project_path",
        skip_serializing_if = "is_default_apple_project_path"
    )]
    /// Path to the Apple project within the `WaterUI` project.
    pub project_path: PathBuf,
    /// The scheme to use for building the Apple project.
    pub scheme: String,
    /// The branch of the Apple backend to use.
    ///
    /// You cannot use both branch and revision at the same time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// The revision (commit hash or tag) of the Apple backend to use.
    ///
    /// You cannot use both revision and branch at the same time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Local path to the Apple backend for local dev.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_path: Option<String>,
}

/// What this project's built application bundle is called.
///
/// Deliberately not the scheme. The scheme is a fixed handle the CLI drives the
/// Xcode project with — every playground shares one, which is what lets one set
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
            branch: None,
            revision: None,
            backend_path: None,
        }
    }

    /// Set a custom project path (defaults to "apple").
    #[must_use]
    pub fn with_project_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.project_path = path.into();
        self
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

    /// Whether this entry configures backend-project scaffolding — anything
    /// beyond `backend_path`, which only selects the runtime's source.
    #[must_use]
    pub fn configures_project(&self) -> bool {
        self.project_path != default_apple_project_path()
            || !self.scheme.is_empty()
            || self.branch.is_some()
            || self.revision.is_some()
    }
}

fn default_apple_project_path() -> PathBuf {
    PathBuf::from("apple")
}

fn is_default_apple_project_path(s: &Path) -> bool {
    s == Path::new("apple")
}

impl AppleBackend {
    /// The `(scheme, app name, crate name)` the scaffold renders with: fixed
    /// `WaterUIApp` for playgrounds, derived from the crate name for apps —
    /// the scheme must match the Xcode target name.
    fn scaffold_names(project: &Project) -> (String, String, CrateName) {
        if project.manifest().package.package_type == crate::project::PackageType::Playground {
            (
                "WaterUIApp".to_string(),
                "WaterUIApp".to_string(),
                CrateName::try_from("WaterUIApp").expect("playground crate name must be valid"),
            )
        } else {
            let crate_name = project.crate_name().clone();
            // App name for Swift code must be a valid Swift identifier (no hyphens)
            // Convert "video-player-example" to "VideoPlayerExample"
            let app_name = templates::apple_app_name(&crate_name);
            (crate_name.to_string(), app_name, crate_name)
        }
    }

    /// The template context [`init`] scaffolds with, rebuilt from the current
    /// manifest — what [`requires_regeneration`] diffs the generated project
    /// against.
    ///
    /// # Errors
    ///
    /// Returns an error when the application dependency graph or the
    /// framework cannot be resolved.
    async fn template_context(project: &Project) -> eyre::Result<TemplateContext> {
        let manifest = project.manifest();
        let (_, app_name, crate_name_for_template) = Self::scaffold_names(project);
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
    /// `backend_path`, `branch` or `revision` change rewrites the Swift
    /// package reference the Xcode project pins.
    ///
    /// `project.pbxproj` is compared with its `OTHER_LDFLAGS` values stripped
    /// — the build merges resolved link inputs into them — and
    /// `WaterUIFonts.swift` is skipped: the build re-renders it from the
    /// resolved fonts, replacing the scaffold output before it can go stale.
    ///
    /// # Errors
    ///
    /// Returns an error when the template context or outputs cannot resolve.
    pub async fn requires_regeneration(project: &Project) -> eyre::Result<bool> {
        let backend_dir = project.backend_path::<Self>();
        let ctx = Self::template_context(project).await?;
        for (relative, expected) in templates::apple::rendered_outputs(&ctx)? {
            let file_name = relative.file_name().and_then(|name| name.to_str());
            if file_name == Some("WaterUIFonts.swift") {
                continue;
            }
            let path = backend_dir.join(&relative);
            match std::fs::read(&path) {
                Ok(existing) if file_name == Some("project.pbxproj") => {
                    if crate::apple::platform::strip_other_ldflags(&String::from_utf8_lossy(
                        &existing,
                    )) != crate::apple::platform::strip_other_ldflags(&String::from_utf8_lossy(
                        &expected,
                    )) {
                        return Ok(true);
                    }
                }
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
        let (scheme, _, _) = Self::scaffold_names(project);
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
            branch: existing.and_then(|backend| backend.branch.clone()),
            revision: existing.and_then(|backend| backend.revision.clone()),
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
        project::{CreateOptions, ManagedBackends, PackageType, Project},
        project_types::BundleIdentifier,
    };

    /// A `[backends.apple]` `backend_path` declared after `water create` must
    /// switch the Xcode project from the remote Swift package to the local
    /// checkout on the next build: the staleness check sees the manifest
    /// change, and the shared re-render rewrites the package reference to an
    /// `XCLocalSwiftPackageReference`.
    #[test]
    fn backend_path_added_after_create_re_renders_the_local_package_reference() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("water-example");
        let mut project = smol::block_on(Project::create(
            &root,
            CreateOptions {
                name: "Water Example".to_string(),
                bundle_identifier: BundleIdentifier::try_from("dev.waterui.waterexample")
                    .expect("bundle identifier"),
                package_type: PackageType::App,
                waterui_path: None,
                channel: None,
                framework_manifest: None,
                framework: Some(crate::framework::test_fixtures::stable_framework()),
                framework_lock: None,
                author: "Lexo Liu".to_string(),
                backends: vec![TargetBackend::Apple],
                web: None,
            },
        ))
        .expect("project creation must succeed");
        smol::block_on(project.init_apple_backend()).expect("apple backend scaffold");

        let backend_dir = project.backend_path::<AppleBackend>();
        let xcodeproj = fs::read_dir(&backend_dir)
            .expect("scaffolded backend directory")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.is_dir()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "xcodeproj")
            })
            .expect("the scaffold produces an .xcodeproj directory");
        let pbxproj = xcodeproj.join("project.pbxproj");
        let rendered = fs::read_to_string(&pbxproj).expect("scaffolded project.pbxproj");
        assert!(
            rendered.contains("XCRemoteSwiftPackageReference"),
            "without backend_path the scaffold pins the remote package"
        );
        assert!(
            !smol::block_on(AppleBackend::requires_regeneration(&project))
                .expect("staleness check"),
            "a freshly scaffolded backend is not stale"
        );

        // The Xcode build cache must survive the re-render like it does for
        // the other generated backends.
        let derived_data = backend_dir.join("DerivedData/stale.txt");
        fs::create_dir_all(derived_data.parent().expect("parent")).expect("DerivedData");
        fs::write(&derived_data, "cache").expect("cache file");

        // The local checkout the manifest points at, declared after create.
        let checkout = dir.path().join("apple-backend");
        fs::create_dir_all(&checkout).expect("checkout");
        fs::write(
            checkout.join("Package.swift"),
            "// swift-tools-version: 6.0\n",
        )
        .expect("Package.swift");
        let manifest_path = root.join("Water.toml");
        let mut manifest: toml_edit::DocumentMut = fs::read_to_string(&manifest_path)
            .expect("Water.toml")
            .parse()
            .expect("Water.toml parses");
        manifest["backends"]["apple"]["backend_path"] =
            toml_edit::value(checkout.to_str().expect("temp dir path is UTF-8"));
        fs::write(&manifest_path, manifest.to_string()).expect("edited Water.toml");

        let project = smol::block_on(Project::open(&root, ManagedBackends::NONE))
            .expect("project must reopen");
        assert!(
            smol::block_on(AppleBackend::requires_regeneration(&project)).expect("staleness check"),
            "a manifest backend_path the render predates is stale"
        );
        smol::block_on(reinit_backend::<AppleBackend>(&project)).expect("reinit");

        let rendered = fs::read_to_string(&pbxproj).expect("re-rendered project.pbxproj");
        assert!(
            rendered.contains("XCLocalSwiftPackageReference"),
            "the re-render pins the local checkout: {rendered}"
        );
        assert!(
            !rendered.contains("XCRemoteSwiftPackageReference"),
            "the remote package reference is gone: {rendered}"
        );
        assert!(derived_data.exists(), "reinit preserves DerivedData");
        assert!(
            !smol::block_on(AppleBackend::requires_regeneration(&project))
                .expect("staleness check"),
            "the re-rendered backend is fresh again"
        );
    }
}
