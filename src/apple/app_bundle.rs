//! `.app` bundle assembly with no Xcode project.
//!
//! The CLI lays out the bundle itself — executable, resources,
//! `actool`-compiled asset catalog, `Info.plist`, `codesign`.

use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use smol::fs;
#[cfg(target_os = "macos")]
use tracing::info;

use crate::{
    apple::backend::AppleBackend,
    platform::{DeviceSigning, PackageOptions, TargetPlatform},
    project::Project,
    project_model::templates::TemplateContext,
    utils::{copy_file, run_command_os},
};

#[cfg(target_os = "macos")]
use crate::toolchain::Host;

/// The on-disk layout of an assembled application bundle.
///
/// macOS uses the `Contents/` layout (`Contents/MacOS`, `Contents/Resources`,
/// `Contents/Frameworks`); every other Apple platform uses the flat layout —
/// executable, resources and `Frameworks/` all at the bundle root.
#[derive(Debug)]
pub struct AppleAppLayout {
    /// The `<name>.app` directory itself.
    pub app_path: PathBuf,
    /// Directory the executable is copied into.
    pub executable_dir: PathBuf,
    /// Directory resources are copied into.
    pub resources_dir: PathBuf,
    /// Directory dynamic libraries are staged into.
    pub frameworks_dir: PathBuf,
    /// `Info.plist` location.
    pub info_plist_path: PathBuf,
}

impl AppleAppLayout {
    /// Compute the layout for `app_path` under the given SDK name.
    #[must_use]
    pub fn for_app(app_path: &Path, sdk_name: &str) -> Self {
        if sdk_name == "macosx" {
            let contents = app_path.join("Contents");
            Self {
                app_path: app_path.to_path_buf(),
                executable_dir: contents.join("MacOS"),
                resources_dir: contents.join("Resources"),
                frameworks_dir: contents.join("Frameworks"),
                info_plist_path: contents.join("Info.plist"),
            }
        } else {
            Self {
                app_path: app_path.to_path_buf(),
                executable_dir: app_path.to_path_buf(),
                resources_dir: app_path.to_path_buf(),
                frameworks_dir: app_path.join("Frameworks"),
                info_plist_path: app_path.join("Info.plist"),
            }
        }
    }

    /// The shipped executable path for the given product name.
    #[must_use]
    pub fn executable_file(&self, product_name: &str) -> PathBuf {
        self.executable_dir.join(product_name)
    }
}

/// The plist entries every Apple platform carries.
fn common_info_plist_entries(
    ctx: &TemplateContext,
    deployment_target: &str,
    product_name: &str,
    bundle_id: &str,
) -> plist::Dictionary {
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "CFBundleDisplayName".to_string(),
        plist::Value::String(ctx.app_display_name.clone()),
    );
    dict.insert(
        "CFBundleExecutable".to_string(),
        plist::Value::String(product_name.to_string()),
    );
    dict.insert(
        "CFBundleIdentifier".to_string(),
        plist::Value::String(bundle_id.to_string()),
    );
    dict.insert(
        "CFBundleInfoDictionaryVersion".to_string(),
        plist::Value::String("6.0".to_string()),
    );
    dict.insert(
        "CFBundleName".to_string(),
        plist::Value::String(product_name.to_string()),
    );
    dict.insert(
        "CFBundlePackageType".to_string(),
        plist::Value::String("APPL".to_string()),
    );
    dict.insert(
        "CFBundleShortVersionString".to_string(),
        plist::Value::String("1.0".to_string()),
    );
    dict.insert(
        "CFBundleVersion".to_string(),
        plist::Value::String("1".to_string()),
    );
    dict.insert(
        "LSMinimumSystemVersion".to_string(),
        plist::Value::String(deployment_target.to_string()),
    );
    dict
}

/// Build the `Info.plist` dictionary the Xcode build settings used to produce
/// (`GENERATE_INFOPLIST_FILE` plus the `INFOPLIST_KEY_*` entries the generated
/// project set), for one platform.
#[must_use]
pub fn apple_info_plist(
    ctx: &TemplateContext,
    project: &Project,
    platform: TargetPlatform,
    deployment_target: &str,
    product_name: &str,
    bundle_id: &str,
) -> plist::Dictionary {
    let mut dict = common_info_plist_entries(ctx, deployment_target, product_name, bundle_id);
    dict.insert(
        "CFBundleDevelopmentRegion".to_string(),
        plist::Value::String("en".to_string()),
    );
    if platform == TargetPlatform::MacOS {
        apply_macos_plist_entries(&mut dict, ctx, project);
    } else {
        apply_mobile_plist_entries(&mut dict, ctx, project, deployment_target);
    }
    dict
}

/// The plist entries an enabled manifest permission produces for `platform`.
fn permission_plist_entries(
    project: &Project,
    macos: bool,
) -> impl Iterator<Item = (String, String)> + '_ {
    project
        .manifest()
        .permissions
        .iter()
        .filter(|(_, entry)| entry.is_enabled())
        .flat_map(move |(key, entry)| {
            let description = entry.description().to_string();
            let keys: Vec<String> = if macos {
                key.macos_usage_description_keys()
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            } else {
                key.ios_plist_key()
                    .and_then(|plist_key| plist_key.strip_prefix("INFOPLIST_KEY_"))
                    .map_or_else(Vec::new, |key| vec![key.to_string()])
            };
            keys.into_iter()
                .map(move |plist_key| (plist_key, description.clone()))
        })
}

/// The macOS-only entries: principal class, menu-bar-agent flag, usage
/// descriptions.
fn apply_macos_plist_entries(
    dict: &mut plist::Dictionary,
    ctx: &TemplateContext,
    project: &Project,
) {
    dict.insert(
        "NSPrincipalClass".to_string(),
        plist::Value::String("NSApplication".to_string()),
    );
    dict.insert(
        "LSUIElement".to_string(),
        plist::Value::Boolean(ctx.accessory),
    );
    for (key, description) in permission_plist_entries(project, true) {
        dict.insert(key, plist::Value::String(description));
    }
}

/// The entries every non-macOS Apple bundle declares.
fn apply_mobile_plist_entries(
    dict: &mut plist::Dictionary,
    ctx: &TemplateContext,
    project: &Project,
    deployment_target: &str,
) {
    let mut insert = |key: &str, value: plist::Value| {
        dict.insert(key.to_string(), value);
    };
    // `MinimumOSVersion` is the floor `installd` enforces; it must not exceed
    // the simulator runtime the bundle installs onto.
    insert(
        "MinimumOSVersion",
        plist::Value::String(deployment_target.to_string()),
    );
    insert(
        "UIDeviceFamily",
        plist::Value::Array(vec![
            plist::Value::Integer(1u64.into()),
            plist::Value::Integer(2u64.into()),
            plist::Value::Integer(4u64.into()),
            plist::Value::Integer(7u64.into()),
        ]),
    );
    insert(
        "UIApplicationSupportsIndirectInputEvents",
        plist::Value::Boolean(true),
    );
    insert(
        "UIBackgroundModes",
        plist::Value::Array(vec![plist::Value::String("audio".to_string())]),
    );
    insert(
        "UIStatusBarStyle",
        plist::Value::String("UIStatusBarStyleDefault".to_string()),
    );
    insert(
        "UISupportedInterfaceOrientations",
        plist::Value::Array(vec![
            plist::Value::String("UIInterfaceOrientationPortrait".to_string()),
            plist::Value::String("UIInterfaceOrientationLandscapeLeft".to_string()),
            plist::Value::String("UIInterfaceOrientationLandscapeRight".to_string()),
        ]),
    );
    insert(
        "UISupportedInterfaceOrientations~ipad",
        plist::Value::Array(vec![
            plist::Value::String("UIInterfaceOrientationPortrait".to_string()),
            plist::Value::String("UIInterfaceOrientationPortraitUpsideDown".to_string()),
            plist::Value::String("UIInterfaceOrientationLandscapeLeft".to_string()),
            plist::Value::String("UIInterfaceOrientationLandscapeRight".to_string()),
        ]),
    );

    // UIKit refuses to launch a scene-configured application without the
    // manifest; the scene delegate class lives in the backend itself
    // (`cocoa_ui`'s `SceneDelegate`), exactly as the generated Xcode project
    // declared it.
    let mut scene_configuration = plist::Dictionary::new();
    scene_configuration.insert(
        "UISceneConfigurationName".to_string(),
        plist::Value::String("Default".to_string()),
    );
    scene_configuration.insert(
        "UISceneDelegateClassName".to_string(),
        plist::Value::String("SceneDelegate".to_string()),
    );
    let mut scene_configurations = plist::Dictionary::new();
    scene_configurations.insert(
        "UIWindowSceneSessionRoleApplication".to_string(),
        plist::Value::Array(vec![plist::Value::Dictionary(scene_configuration)]),
    );
    let mut scene_manifest = plist::Dictionary::new();
    scene_manifest.insert(
        "UIApplicationSupportsMultipleScenes".to_string(),
        plist::Value::Boolean(true),
    );
    scene_manifest.insert(
        "UISceneConfigurations".to_string(),
        plist::Value::Dictionary(scene_configurations),
    );
    insert(
        "UIApplicationSceneManifest",
        plist::Value::Dictionary(scene_manifest),
    );

    let mut launch_screen = plist::Dictionary::new();
    if ctx.launch.has_background {
        launch_screen.insert(
            "UIColorName".to_string(),
            plist::Value::String("LaunchBackground".to_string()),
        );
    }
    if ctx.launch.has_image {
        launch_screen.insert(
            "UIImageName".to_string(),
            plist::Value::String("LaunchImage".to_string()),
        );
        launch_screen.insert(
            "UIImageRespectsSafeAreaInsets".to_string(),
            plist::Value::Boolean(true),
        );
    }
    insert("UILaunchScreen", plist::Value::Dictionary(launch_screen));

    for (key, description) in permission_plist_entries(project, false) {
        dict.insert(key, plist::Value::String(description));
    }
}

/// Assemble the `.app` directory: executable, copied resources, the
/// `actool`-compiled asset catalog, `Info.plist`, `PkgInfo`.
///
/// `staging_dir` is what `copy_assets_and_fonts` populated (`waterui_assets/`,
/// `WaterUIAssets.xcassets/`, `fonts/`).
///
/// # Errors
/// Returns an error when the executable is missing, a copy fails, or `actool`
/// fails to compile the asset catalog.
pub async fn assemble_app_bundle(
    layout: &AppleAppLayout,
    executable: &Path,
    product_name: &str,
    staging_dir: &Path,
    info_plist: &plist::Dictionary,
    sdk_name: &str,
    deployment_target: &str,
) -> eyre::Result<()> {
    if !executable.is_file() {
        bail!(
            "Application executable not found at {}. Build must succeed before packaging.",
            executable.display()
        );
    }
    if layout.app_path.exists() {
        fs::remove_dir_all(&layout.app_path).await?;
    }
    fs::create_dir_all(&layout.executable_dir).await?;
    fs::create_dir_all(&layout.resources_dir).await?;

    let executable_dest = layout.executable_file(product_name);
    copy_file(executable, &executable_dest).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&executable_dest).await?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&executable_dest, perms).await?;
    }

    // `waterui_assets` and `fonts` are ordinary bundle resources.
    for name in ["waterui_assets", "fonts"] {
        let source = staging_dir.join(name);
        if source.is_dir() {
            copy_dir_contents(&source, &layout.resources_dir.join(name)).await?;
        }
    }

    let mut plist = info_plist.clone();
    compile_asset_catalog(
        layout,
        &staging_dir.join("WaterUIAssets.xcassets"),
        sdk_name,
        deployment_target,
        &mut plist,
    )
    .await?;

    let mut plist_file = Vec::new();
    plist::Value::Dictionary(plist)
        .to_writer_xml(&mut plist_file)
        .wrap_err("failed to serialize Info.plist")?;
    fs::write(&layout.info_plist_path, plist_file).await?;

    if sdk_name == "macosx" {
        fs::write(
            layout.app_path.join("Contents").join("PkgInfo"),
            b"APPL????",
        )
        .await?;
    }

    Ok(())
}

/// Compile the staged asset catalog into the bundle's resources directory
/// with `actool`, then merge the `--output-partial-info-plist` keys
/// (`CFBundleIconFile`, `CFBundleIconName`, …) into `info_plist`.
#[cfg(target_os = "macos")]
async fn compile_asset_catalog(
    layout: &AppleAppLayout,
    xcassets: &Path,
    sdk_name: &str,
    deployment_target: &str,
    info_plist: &mut plist::Dictionary,
) -> eyre::Result<()> {
    if !xcassets.is_dir() {
        return Ok(());
    }
    let partial_plist = layout.resources_dir.join(".waterui-actool-partial.plist");
    run_command_os(
        "xcrun",
        [
            "actool".into(),
            "--compile".into(),
            layout.resources_dir.as_os_str().to_os_string(),
            "--platform".into(),
            sdk_name.into(),
            "--minimum-deployment-target".into(),
            deployment_target.into(),
            "--app-icon".into(),
            "AppIcon".into(),
            "--accent-color".into(),
            "AccentColor".into(),
            "--output-partial-info-plist".into(),
            partial_plist.as_os_str().to_os_string(),
            xcassets.as_os_str().to_os_string(),
        ],
    )
    .await
    .wrap_err("actool failed to compile the asset catalog")?;

    if partial_plist.is_file() {
        let partial = plist::Value::from_file(&partial_plist)
            .wrap_err("failed to read actool's partial Info.plist")?;
        if let plist::Value::Dictionary(entries) = partial {
            for (key, value) in entries {
                info_plist.insert(key, value);
            }
        }
        fs::remove_file(&partial_plist).await?;
    }
    Ok(())
}

/// Non-macOS hosts cannot run `actool`; Apple packaging only ever ran on
/// macOS (it drove `xcodebuild` before), so the check is a plain error.
#[cfg(not(target_os = "macos"))]
async fn compile_asset_catalog(
    _layout: &AppleAppLayout,
    _xcassets: &Path,
    _sdk_name: &str,
    _deployment_target: &str,
    _info_plist: &mut plist::Dictionary,
) -> eyre::Result<()> {
    bail!("Apple packaging requires macOS (actool is part of the Xcode toolchain)")
}

async fn copy_dir_contents(from: &Path, to: &Path) -> eyre::Result<()> {
    let source = from.to_path_buf();
    let destination = to.to_path_buf();
    smol::unblock(move || {
        let mut options = fs_extra::dir::CopyOptions::new();
        options.copy_inside = true;
        options.overwrite = true;
        fs_extra::dir::copy(&source, &destination, &options)
            .map(|_| ())
            .map_err(|error| {
                eyre::eyre!(
                    "Failed to copy resources from {} to {}: {error}",
                    source.display(),
                    destination.display()
                )
            })
    })
    .await
}

/// Sign the assembled bundle per platform: ad-hoc for macOS and simulators,
/// the resolved development identity for devices, nothing for an unsigned
/// device package.
///
/// # Errors
/// Returns an error when signing fails, or when a device build's identity or
/// provisioning profile cannot be resolved.
pub async fn sign_apple_app(
    layout: &AppleAppLayout,
    platform: TargetPlatform,
    options: &PackageOptions,
    backend: &AppleBackend,
    backend_root: &Path,
    project: &Project,
) -> eyre::Result<()> {
    if platform == TargetPlatform::MacOS {
        #[cfg(target_os = "macos")]
        {
            let requires_stable_identity =
                project.manifest().permissions.iter().any(|(key, entry)| {
                    entry.is_enabled() && !key.macos_usage_description_keys().is_empty()
                });
            crate::macos_bundle::sign_macos_app(
                &layout.app_path,
                project.bundle_identifier(),
                requires_stable_identity,
            )
            .await?;
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        {
            bail!("Apple packaging requires macOS (codesign is part of the Xcode toolchain)");
        }
    }

    if platform.is_simulator() {
        codesign_bundle(&layout.app_path, &layout.frameworks_dir, "-", None, None).await?;
        return Ok(());
    }

    // A physical Apple OS refuses unsigned code; only an explicitly unsigned
    // package leaves the bundle unsigned (it is signed before install).
    if options.device_signing() == DeviceSigning::Unsigned {
        return Ok(());
    }

    let entitlements = backend_root
        .join(&backend.scheme)
        .join(format!("{}.entitlements", backend.scheme));
    sign_device_app(layout, project, &entitlements).await
}

/// Run `codesign` over every member of `frameworks_dir`, then the bundle
/// itself, inside-out the way `xcodebuild` signs.
async fn codesign_bundle(
    app_path: &Path,
    frameworks_dir: &Path,
    identity: &str,
    entitlements: Option<&Path>,
    identifier: Option<&str>,
) -> eyre::Result<()> {
    use smol::stream::StreamExt as _;

    if frameworks_dir.is_dir() {
        let mut members = Vec::new();
        let mut entries = fs::read_dir(frameworks_dir).await?;
        while let Some(entry) = entries.next().await {
            let path = entry?.path();
            if path.is_file()
                || matches!(
                    path.extension().and_then(std::ffi::OsStr::to_str),
                    Some("app" | "framework")
                )
            {
                members.push(path);
            }
        }
        members.sort();
        for member in members {
            codesign_path(&member, identity, None, None).await?;
        }
    }
    codesign_path(app_path, identity, entitlements, identifier).await
}

async fn codesign_path(
    path: &Path,
    identity: &str,
    entitlements: Option<&Path>,
    identifier: Option<&str>,
) -> eyre::Result<()> {
    let mut arguments = vec![
        std::ffi::OsString::from("--force"),
        std::ffi::OsString::from("--sign"),
        std::ffi::OsString::from(identity),
        std::ffi::OsString::from("--timestamp=none"),
    ];
    if let Some(entitlements) = entitlements {
        arguments.push(std::ffi::OsString::from("--entitlements"));
        arguments.push(entitlements.as_os_str().to_owned());
    }
    if let Some(identifier) = identifier {
        arguments.push(std::ffi::OsString::from("--identifier"));
        arguments.push(std::ffi::OsString::from(identifier));
    }
    arguments.push(path.as_os_str().to_owned());
    run_command_os("codesign", arguments).await?;
    Ok(())
}

/// Sign a device build with the resolved development identity, embedding the
/// development provisioning profile that covers the bundle id — the
/// responsibilities `xcodebuild` automatic signing used to take.
///
/// The provisioning profile supplies the entitlements (application
/// identifier, team identifier, `get-task-allow`) the signature must claim;
/// the generated project's `.entitlements` file is merged over them.
#[cfg(target_os = "macos")]
async fn sign_device_app(
    layout: &AppleAppLayout,
    project: &Project,
    entitlements_path: &Path,
) -> eyre::Result<()> {
    let host = Host::current();
    let bundle_id = project.bundle_identifier();
    let team = crate::apple::toolchain::development_team_id(&host).await?;
    let profile = find_development_profile(&host, &team, bundle_id).await?;
    let profile_data = decode_profile(&host, &profile).await?;
    let identity = development_identity(&host, &team, &profile, &profile_data).await?;

    copy_file(&profile, layout.app_path.join("embedded.mobileprovision")).await?;

    let mut entitlements = profile_entitlements(&profile, &profile_data)?;
    if entitlements_path.is_file()
        && let plist::Value::Dictionary(project_entitlements) =
            plist::Value::from_file(entitlements_path).wrap_err_with(|| {
                format!(
                    "Failed to read entitlements {}",
                    entitlements_path.display()
                )
            })?
    {
        for (key, value) in project_entitlements {
            entitlements.insert(key, value);
        }
    }
    let app_name = layout
        .app_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let merged = layout.app_path.with_file_name(format!("{app_name}.xcent"));
    let mut serialized = Vec::new();
    plist::Value::Dictionary(entitlements)
        .to_writer_xml(&mut serialized)
        .wrap_err("failed to serialize signing entitlements")?;
    fs::write(&merged, serialized).await?;

    codesign_bundle(
        &layout.app_path,
        &layout.frameworks_dir,
        &identity,
        Some(&merged),
        Some(bundle_id),
    )
    .await?;
    info!(
        "Signed {} with {team}/{identity}",
        layout.app_path.display()
    );
    Ok(())
}

/// A physical device cannot be provisioned from a non-macOS host.
#[cfg(not(target_os = "macos"))]
async fn sign_device_app(
    _layout: &AppleAppLayout,
    _project: &Project,
    _entitlements_path: &Path,
) -> eyre::Result<()> {
    bail!("Apple device signing requires macOS (codesign and the provisioning profiles live there)")
}

/// The keychain development identity `profile` was issued for, from
/// `security find-identity -v -p codesigning`.
///
/// Xcode pairs a provisioning profile with the `Apple Development`
/// certificate listed in its `DeveloperCertificates`; `find-identity`
/// reports the same certificate SHA-1 for each keychain identity, so the
/// identity whose hash the profile names is the one `codesign` should
/// use.
#[cfg(target_os = "macos")]
async fn development_identity(
    host: &Host,
    team: &str,
    profile: &Path,
    profile_data: &plist::Dictionary,
) -> eyre::Result<String> {
    let output = host
        .output("security", ["find-identity", "-v", "-p", "codesigning"])
        .await
        .wrap_err("failed to run `security find-identity`")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    pick_profile_identity(&stdout, profile_data).ok_or_else(|| {
        eyre::eyre!(
            "The keychain holds no `Apple Development` certificate the \
             provisioning profile {} was issued for (team {team}). Open \
             Xcode → Settings → Accounts → Manage Certificates and add the \
             development certificate this profile lists, then re-run \
             `water package`.",
            profile.display()
        )
    })
}

/// SHA-1 of a DER certificate as uppercase hex — the hash `find-identity`
/// prints for each identity.
#[cfg(target_os = "macos")]
fn certificate_sha1_hex(der: &[u8]) -> String {
    use sha1::Digest as _;
    hex::encode_upper(sha1::Sha1::digest(der))
}

/// SHA-1 hashes of every certificate in a decoded profile's
/// `DeveloperCertificates` array.
#[cfg(target_os = "macos")]
fn profile_certificate_hashes(data: &plist::Dictionary) -> std::collections::HashSet<String> {
    data.get("DeveloperCertificates")
        .and_then(plist::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_data().map(certificate_sha1_hex))
        .collect()
}

/// `(sha-1, display name)` pairs `security find-identity -v -p
/// codesigning` lists, for development identities only — each entry looks
/// like `  1) 40HEXDIGITS "Apple Development: name (id)"`.
#[cfg(target_os = "macos")]
fn development_identities(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let entry = line.split_once(')')?.1.trim();
            let (hash, name) = entry.split_once(' ')?;
            let name = name.trim().trim_matches('"');
            crate::apple::toolchain::is_development_certificate_name(name)
                .then(|| (hash.to_string(), name.to_string()))
        })
        .collect()
}

/// The identity hash the profile's `DeveloperCertificates` names, if the
/// keychain holds it.
#[cfg(target_os = "macos")]
fn pick_profile_identity(find_identity: &str, data: &plist::Dictionary) -> Option<String> {
    let accepted = profile_certificate_hashes(data);
    development_identities(find_identity)
        .into_iter()
        .map(|(hash, _)| hash)
        .find(|hash| accepted.contains(hash))
}

/// Decode a `.mobileprovision` file with `security cms` into its plist
/// dictionary — `security cms -D -i` writes the embedded plist to stdout.
#[cfg(target_os = "macos")]
async fn decode_profile(host: &Host, profile: &Path) -> eyre::Result<plist::Dictionary> {
    let output = host
        .output(
            "security",
            [
                "cms".into(),
                "-D".into(),
                "-i".into(),
                profile.as_os_str().to_owned(),
            ],
        )
        .await
        .wrap_err_with(|| format!("failed to decode {}", profile.display()))?;
    if !output.status.success() {
        bail!(
            "`security cms` rejected {}: {}",
            profile.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(&output.stdout))
        .wrap_err_with(|| format!("{} is not a provisioning profile", profile.display()))?;
    let plist::Value::Dictionary(root) = value else {
        bail!(
            "{} does not decode to a plist dictionary",
            profile.display()
        );
    };
    Ok(root)
}

/// The `Entitlements` dictionary of a decoded provisioning profile.
#[cfg(target_os = "macos")]
fn profile_entitlements(
    profile: &Path,
    data: &plist::Dictionary,
) -> eyre::Result<plist::Dictionary> {
    match data.get("Entitlements") {
        Some(plist::Value::Dictionary(entitlements)) => Ok(entitlements.clone()),
        _ => bail!("{} carries no Entitlements dictionary", profile.display()),
    }
}

/// The installed development provisioning profile whose application
/// identifier is `<team>.<bundle_id>` (or a wildcard), searched in the two
/// profile directories Xcode maintains.
#[cfg(target_os = "macos")]
async fn find_development_profile(
    host: &Host,
    team: &str,
    bundle_id: &str,
) -> eyre::Result<PathBuf> {
    use smol::stream::StreamExt as _;

    let expected = format!("{team}.{bundle_id}");
    let mut candidates = Vec::new();
    let Some(home) = host.home_dir() else {
        bail!("Cannot locate the provisioning profile directory: no home directory");
    };
    for directory in [
        home.join("Library/MobileDevice/Provisioning Profiles"),
        home.join("Library/Developer/Xcode/UserData/Provisioning Profiles"),
    ] {
        if !directory.is_dir() {
            continue;
        }
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next().await {
            let path = entry?.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) == Some("mobileprovision") {
                candidates.push(path);
            }
        }
    }
    candidates.sort();

    for profile in candidates {
        let Ok(data) = decode_profile(host, &profile).await else {
            continue;
        };
        let Some(identifier) = data
            .get("Entitlements")
            .and_then(plist::Value::as_dictionary)
            .and_then(|entitlements| entitlements.get("application-identifier"))
            .and_then(plist::Value::as_string)
        else {
            continue;
        };
        if identifier == expected || identifier == format!("{team}.*") {
            return Ok(profile);
        }
    }

    bail!(
        "No development provisioning profile for {expected} found in \
         ~/Library/MobileDevice/Provisioning Profiles. `xcodebuild` used to \
         mint one; open the project in Xcode and build for a device once, or \
         install a profile from the Apple Developer portal."
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{
        certificate_sha1_hex, development_identities, pick_profile_identity,
        profile_certificate_hashes,
    };

    /// The self-signed `Apple Development`-shaped fixture certificate from
    /// `toolchain/testdata`; `sha1sum` of its DER is
    /// `5d03dd01b4f95d47874c9bfd9367a978d838a228`.
    const DEV_CERT_PEM: &str = include_str!("../toolchain/testdata/apple_development.pem");
    const DEV_CERT_SHA1: &str = "5D03DD01B4F95D47874C9BFD9367A978D838A228";
    const OTHER_CERT_SHA1: &str = "AAAAAAAABBBBBBBBCCCCCCCCDDDDDDDDEEEEEEEE";

    fn dev_cert_der() -> Vec<u8> {
        x509_parser::pem::Pem::iter_from_buffer(DEV_CERT_PEM.as_bytes())
            .next()
            .expect("the fixture holds one PEM block")
            .expect("the fixture PEM decodes")
            .contents
    }

    fn find_identity_output() -> String {
        format!(
            "     1) {DEV_CERT_SHA1} \"Apple Development: devin.test@example.com (TESTCERT42)\"\n     2) {OTHER_CERT_SHA1} \"Apple Development: devin.other@example.com (OTHERID9X)\"\n     2 valid identities found\n"
        )
    }

    #[test]
    fn certificate_sha1_matches_openssl() {
        assert_eq!(certificate_sha1_hex(&dev_cert_der()), DEV_CERT_SHA1);
    }

    #[test]
    fn identity_pairs_with_a_profile_certificate() {
        let profile = plist::Dictionary::from_iter([(
            "DeveloperCertificates".to_string(),
            plist::Value::Array(vec![plist::Value::Data(dev_cert_der())]),
        )]);
        assert_eq!(
            pick_profile_identity(&find_identity_output(), &profile).as_deref(),
            Some(DEV_CERT_SHA1)
        );
    }

    #[test]
    fn identity_absent_when_profile_lists_another_certificate() {
        let profile = plist::Dictionary::from_iter([(
            "DeveloperCertificates".to_string(),
            plist::Value::Array(vec![plist::Value::Data(vec![0xDE, 0xAD])]),
        )]);
        assert_eq!(
            pick_profile_identity(&find_identity_output(), &profile),
            None
        );
    }

    #[test]
    fn non_development_identities_are_skipped() {
        let find_identity = format!(
            "     1) {DEV_CERT_SHA1} \"Devin Signing Test\"\n     1 valid identities found\n"
        );
        let profile = plist::Dictionary::from_iter([(
            "DeveloperCertificates".to_string(),
            plist::Value::Array(vec![plist::Value::Data(dev_cert_der())]),
        )]);
        assert_eq!(pick_profile_identity(&find_identity, &profile), None);
    }

    #[test]
    fn a_profile_without_certificates_matches_nothing() {
        assert_eq!(
            pick_profile_identity(&find_identity_output(), &plist::Dictionary::new()),
            None
        );
    }

    #[test]
    fn find_identity_parsing_keeps_development_names_only() {
        let output = find_identity_output()
            + "     3) DEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF \"Apple Distribution: x\"\n";
        let identities = development_identities(&output);
        assert_eq!(identities.len(), 2);
        assert_eq!(identities[0].0, DEV_CERT_SHA1);
    }

    #[test]
    fn profile_hash_set_covers_every_certificate() {
        let profile = plist::Dictionary::from_iter([(
            "DeveloperCertificates".to_string(),
            plist::Value::Array(vec![
                plist::Value::Data(dev_cert_der()),
                plist::Value::Data(vec![0x01, 0x02]),
            ]),
        )]);
        let hashes = profile_certificate_hashes(&profile);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(DEV_CERT_SHA1));
    }
}
