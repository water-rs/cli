//! Toolchain support for `sccache` - shared compilation cache.

use std::path::{Path, PathBuf};

use smol::process::Command;

use crate::{
    brew::Brew,
    toolchain::linux::{
        LinuxPackageManagerError, has_supported_package_manager, install_named_packages,
    },
    toolchain::winget::{WingetInstallError, ensure_package_installed},
    toolchain::{Host, Installation, Toolchain, ToolchainError},
    utils::{CommandError, sccache_install_hint},
};

/// Route a Cargo invocation's compiles through `sccache`.
///
/// Caching only bites because generated-crate builds also disable incremental
/// compilation — Cargo does not pass `-C incremental` to registry dependencies but does
/// pass it to every *path* dependency, which for a `WaterUI` build is the entire
/// framework, and `sccache` refuses to cache an incremental compile. That setting lives
/// in [`crate::build::configure_generated_crate_compilation`] rather than here, because
/// it must not depend on whether a machine happens to have `sccache` installed: it
/// changes the compiled ABI, and two builds in one flow have to agree on it.
///
/// The server address is namespaced to the invoking user. sccache discovers
/// its server on a host-wide address — TCP `127.0.0.1:4226` unless told
/// otherwise — and every compile job runs inside the server process under
/// the *server owner's* identity. Left at the default, a build running as one
/// user borrows a server another user left alive and its artifacts land in
/// this user's target dir owned by the other uid, ending the build on
/// `Permission denied`. A unix socket under the user's own Water home gives
/// each account its own server with no port to collide over; where unix
/// sockets do not exist, a deterministic per-user port does the same.
pub fn configure_compilation_cache(command: &mut Command, sccache_path: &Path) {
    command.env("RUSTC_WRAPPER", sccache_path);
    #[cfg(unix)]
    if let Some(socket) = server_socket_path() {
        command.env("SCCACHE_SERVER_UDS", socket);
        return;
    }
    command.env("SCCACHE_SERVER_PORT", per_user_server_port().to_string());
}

/// The unix socket a per-user sccache server listens on, under the invoking
/// user's own `~/.water` so no other account can reach — or be reached by —
/// it. `None` when the home directory cannot be resolved or created.
#[cfg(unix)]
fn server_socket_path() -> Option<PathBuf> {
    let water_home = crate::project_model::water_dir::water_home_dir().ok()?;
    std::fs::create_dir_all(&water_home).ok()?;
    Some(water_home.join("sccache-server.sock"))
}

/// A deterministic per-user TCP port for the sccache server, chosen inside
/// the 40000–49150 block that sits below every platform's ephemeral range.
/// FNV-1a over the user name spreads two accounts on one host apart without
/// any state to coordinate.
fn per_user_server_port() -> u16 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in whoami::username().unwrap_or_default().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
    }
    40_000 + (hash % 9_151) as u16
}

/// Toolchain for `sccache` - a shared compilation cache for Rust.
///
/// sccache is optional but significantly improves build times by caching
/// compiled artifacts across builds and projects.
#[derive(Debug, Clone, Default)]
pub struct Sccache;

impl Sccache {
    /// Get the path to the `sccache` executable if available.
    ///
    /// # Errors
    /// Returns an error if `sccache` is not found in the system PATH.
    pub async fn path(&self, host: &Host) -> Result<PathBuf, which::Error> {
        host.which("sccache").await
    }

    /// Check if sccache is available on `host` without returning an error.
    pub async fn is_available(&self, host: &Host) -> bool {
        self.path(host).await.is_ok()
    }
}

impl Toolchain for Sccache {
    type Installation = SccacheInstallation;

    async fn check(&self, host: &Host) -> Result<(), ToolchainError<Self::Installation>> {
        if host.which("sccache").await.is_ok() {
            Ok(())
        } else if cfg!(target_os = "windows") {
            if host.which("winget").await.is_ok() {
                Err(ToolchainError::fixable(SccacheInstallation))
            } else {
                Err(ToolchainError::unfixable(
                    "sccache not found and winget is unavailable",
                    format!(
                        "Install Microsoft App Installer to provide winget, or install manually with {}.",
                        sccache_install_hint()
                    ),
                ))
            }
        } else if cfg!(target_os = "macos") {
            if host.which("brew").await.is_ok() {
                Err(ToolchainError::fixable(SccacheInstallation))
            } else {
                Err(ToolchainError::unfixable(
                    "sccache not found and Homebrew is unavailable",
                    format!(
                        "Install Homebrew to enable automatic fixes, or install manually with {}.",
                        sccache_install_hint()
                    ),
                ))
            }
        } else if cfg!(target_os = "linux") {
            if has_supported_package_manager(host).await {
                Err(ToolchainError::fixable(SccacheInstallation))
            } else {
                Err(ToolchainError::unfixable(
                    "sccache is missing and no supported package manager was found",
                    format!("Install manually with {}", sccache_install_hint()),
                ))
            }
        } else {
            Err(ToolchainError::unfixable(
                "sccache not found",
                format!(
                    "Install sccache manually ({}) and ensure `sccache` is available in PATH.",
                    sccache_install_hint()
                ),
            ))
        }
    }
}

/// Installation plan for `sccache`.
#[derive(Debug, Clone)]
pub struct SccacheInstallation;

/// Errors that can occur during `sccache` installation.
#[derive(Debug, thiserror::Error)]
pub enum FailToInstallSccache {
    /// Homebrew not found error.
    #[error("Homebrew not found. Please install Homebrew to proceed.")]
    BrewNotFound,

    /// An installation command failed.
    #[error("Failed to install sccache: {0}")]
    Command(#[from] CommandError),

    /// winget is required for Windows automatic installation.
    #[error(
        "winget is required for automatic sccache installation on Windows. Install App Installer and retry."
    )]
    WingetNotFound,

    /// Windows installation via winget failed.
    #[error("Failed to install sccache via winget: {0}")]
    WingetInstallFailed(String),

    /// Linux package manager is required for automatic installation.
    #[error(
        "No supported Linux package manager found (apt-get, dnf, pacman, zypper, apk). Install sccache manually."
    )]
    UnsupportedPackageManager,

    /// Unsupported platform error.
    #[error(
        "Automatic installation of sccache is not supported on this platform. \
         Install manually with: cargo install sccache"
    )]
    UnsupportedPlatform,
}

impl Installation for SccacheInstallation {
    type Error = FailToInstallSccache;

    async fn install(&self, host: &Host) -> Result<(), Self::Error> {
        if cfg!(target_os = "macos") {
            let brew = Brew::default();

            brew.check(host)
                .await
                .map_err(|_| FailToInstallSccache::BrewNotFound)?;
            brew.install(host, "sccache").await?;

            Ok(())
        } else if cfg!(target_os = "windows") {
            ensure_package_installed(host, "Mozilla.sccache")
                .await
                .map_err(map_winget_error_for_sccache)
        } else if cfg!(target_os = "linux") {
            install_named_packages(host, &["sccache"])
                .await
                .map_err(map_linux_error_for_sccache)
        } else {
            Err(FailToInstallSccache::UnsupportedPlatform)
        }
    }
}

fn map_linux_error_for_sccache(error: LinuxPackageManagerError) -> FailToInstallSccache {
    match error {
        LinuxPackageManagerError::UnsupportedPackageManager => {
            FailToInstallSccache::UnsupportedPackageManager
        }
        LinuxPackageManagerError::Command(source) => FailToInstallSccache::Command(source),
    }
}

fn map_winget_error_for_sccache(error: WingetInstallError) -> FailToInstallSccache {
    match error {
        WingetInstallError::WingetNotFound => FailToInstallSccache::WingetNotFound,
        WingetInstallError::CommandFailed(err) => {
            FailToInstallSccache::WingetInstallFailed(err.to_string())
        }
        WingetInstallError::NotInstalled { package_id } => {
            FailToInstallSccache::WingetInstallFailed(format!(
                "Package `{package_id}` is still missing after winget install; verify winget sources and retry."
            ))
        }
    }
}

#[cfg(test)]
mod host_tests {
    use super::{Sccache, SccacheInstallation};
    use crate::toolchain::testing::TestMachine;
    use crate::toolchain::{Toolchain, ToolchainError};

    fn check(machine: &TestMachine) -> Result<(), ToolchainError<SccacheInstallation>> {
        let host = machine.host(Vec::<(String, String)>::new());
        smol::block_on(Sccache.check(&host))
    }

    #[test]
    fn ok_when_sccache_on_path() {
        let machine = TestMachine::new();
        machine.install("sccache");
        check(&machine).expect("sccache on PATH must be ok");
    }

    #[test]
    fn missing_without_installer_is_unfixable() {
        let machine = TestMachine::new();
        let result = check(&machine);
        assert!(
            matches!(result, Err(ToolchainError::Unfixable(_))),
            "missing sccache without a package manager must be unfixable: {result:?}"
        );
    }

    #[test]
    fn missing_with_installer_is_fixable() {
        let machine = TestMachine::new();
        #[cfg(target_os = "macos")]
        machine.install("brew");
        #[cfg(target_os = "linux")]
        machine.install("apt-get");
        #[cfg(target_os = "windows")]
        machine.install("winget");
        let result = check(&machine);
        assert!(
            matches!(result, Err(ToolchainError::Fixable(_))),
            "missing sccache with a package manager must be fixable: {result:?}"
        );
    }
}
