use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    android::platform::{AndroidAbi, AndroidPlatform, clean_android, is_android_platform},
    backend::Backend,
    build::BuildOptions,
    device::Artifact,
    platform::{PackageOptions, TargetBackend, TargetPlatform},
    project::Project,
    templates::{self, TemplateContext},
};

/// Configuration for the Android backend in a `WaterUI` project.
///
/// `[backends.android]` in `Water.toml`
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AndroidBackend {
    #[serde(
        default = "default_android_project_path",
        skip_serializing_if = "is_default_android_project_path"
    )]
    project_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// Path to a local `android-backend` checkout used as the runtime source.
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_path: Option<String>,
}

impl AndroidBackend {
    /// Create a new Android backend configuration with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            project_path: default_android_project_path(),
            version: None,
            backend_path: None,
        }
    }

    /// Set a custom project path (defaults to "android").
    #[must_use]
    pub fn with_project_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.project_path = path.into();
        self
    }

    /// Set the local `android-backend` checkout used as the runtime source.
    #[must_use]
    pub fn with_backend_path(mut self, path: impl Into<String>) -> Self {
        self.backend_path = Some(path.into());
        self
    }

    /// Get the path to the Android project within the `WaterUI` project.
    #[must_use]
    pub const fn project_path(&self) -> &PathBuf {
        &self.project_path
    }

    /// Get the local `android-backend` checkout used as the runtime source.
    #[must_use]
    pub fn backend_path(&self) -> Option<&str> {
        self.backend_path.as_deref()
    }

    /// Whether this entry configures backend-project scaffolding — anything
    /// beyond `backend_path`, which only selects the runtime's source.
    #[must_use]
    pub fn configures_project(&self) -> bool {
        self.project_path != default_android_project_path() || self.version.is_some()
    }

    /// Get the path to the Gradle wrapper script within the Android project.
    #[must_use]
    pub fn gradlew_path(&self) -> PathBuf {
        let base = &self.project_path;
        if cfg!(windows) {
            base.join("gradlew.bat")
        } else {
            base.join("gradlew")
        }
    }
}

impl Default for AndroidBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for AndroidBackend {
    const DEFAULT_PATH: &'static str = "android";

    // Preserve Gradle build caches during re-scaffolding
    const CACHE_PATHS: &'static [&'static str] = &[".gradle", "build", "app"];

    fn path(&self) -> &Path {
        &self.project_path
    }

    async fn init(project: &Project) -> Result<Self, crate::backend::FailToInitBackend> {
        let manifest = project.manifest();

        // Derive app name from the display name (remove spaces for filesystem)
        let app_name = manifest
            .package
            .name
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>();

        // Android is where a missing declaration actually breaks things, so
        // surface anything a dependency needs that the app has not enabled.
        // The audit resolves the FFI companion's graph — the crate the Android
        // build compiles. On a first init the companion is not scaffolded yet,
        // so the scan fails and the audit is skipped until the next reinit.
        match crate::assets::scan_required_permissions(&project.ffi_crate_path().join("Cargo.toml"))
            .await
        {
            Ok(required) => crate::assets::warn_missing_permissions(project, &required, |key| {
                key.android_permission_name().is_some()
            }),
            Err(error) => tracing::debug!("skipped permission audit: {error}"),
        }

        // Extract enabled permissions from the manifest
        let android_permissions = manifest
            .permissions
            .iter()
            .filter(|(_, entry)| entry.is_enabled())
            .filter_map(|(key, _)| {
                key.android_permission_name()
                    .map(|name| templates::AndroidPermissionTemplateEntry { name })
            })
            .collect();

        let ctx = TemplateContext::for_project_manifest(
            manifest,
            project.crate_name().clone(),
            app_name,
            &project
                .resolved_framework()
                .await
                .map_err(crate::backend::FailToInitBackend::Config)?,
        )
        .with_backend_project_path(project.backend_path::<Self>())
        .with_project_root_path(project.root().to_path_buf())
        .with_android_permissions(android_permissions);

        templates::android::scaffold(&project.backend_path::<Self>(), &ctx)
            .await
            .map_err(crate::backend::FailToInitBackend::Io)?;

        let existing = manifest.backends.android();
        Ok(Self {
            project_path: existing.map_or_else(default_android_project_path, |backend| {
                backend.project_path.clone()
            }),
            version: existing.and_then(|backend| backend.version.clone()),
            backend_path: existing.and_then(|backend| backend.backend_path.clone()),
        })
    }

    fn supports(&self, platform: TargetPlatform) -> bool {
        is_android_platform(platform)
    }

    async fn build(
        &self,
        project: &Project,
        platform: TargetPlatform,
        options: BuildOptions,
    ) -> eyre::Result<crate::build::BuiltTarget> {
        debug_assert_eq!(platform, TargetPlatform::Android);
        project
            .browser_runtime_plan(platform, TargetBackend::Android)
            .await?;
        AndroidPlatform::arm64().build(project, options).await
    }

    async fn package(
        &self,
        project: &Project,
        platform: TargetPlatform,
        options: PackageOptions,
        _built: &crate::build::BuiltTarget,
    ) -> eyre::Result<Artifact> {
        debug_assert_eq!(platform, TargetPlatform::Android);
        AndroidPlatform::package_with_abis(project, options, &[AndroidAbi::Arm64V8a]).await
    }

    async fn clean(&self, project: &Project, _platform: TargetPlatform) -> eyre::Result<()> {
        clean_android(project).await
    }
}

fn default_android_project_path() -> PathBuf {
    PathBuf::from("android")
}

fn is_default_android_project_path(s: &Path) -> bool {
    s == Path::new("android")
}
