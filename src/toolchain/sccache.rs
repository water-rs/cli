//! Toolchain support for `sccache` - shared compilation cache.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use smol::process::Command;

use crate::{
    brew::Brew,
    toolchain::linux::{
        LinuxPackageManagerError, has_supported_package_manager, install_named_packages,
    },
    toolchain::winget::{WingetInstallError, ensure_package_installed},
    toolchain::{Host, Installation, Toolchain, ToolchainError},
    utils::{CommandError, sccache_install_hint, sccache_upgrade_hint},
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
/// each account its own server with no port to collide over, and sccache
/// ≥ 0.9.0 prefers it when both are set; the port is still set unconditionally
/// because older builds ignore the socket variable entirely and would fall
/// back to the shared default address.
/// # Errors
/// Returns an error when the socket directory under the user's Water home
/// cannot be created or exists with permissions wider than `0700`.
pub fn configure_compilation_cache(command: &mut Command, sccache_path: &Path) -> eyre::Result<()> {
    let water_home = crate::project_model::water_dir::water_home_dir().ok();
    #[cfg(unix)]
    let env = compilation_cache_env_in(sccache_path, water_home.as_deref())?;
    #[cfg(not(unix))]
    let env = compilation_cache_env_in(sccache_path, water_home.as_deref());
    for (key, value) in env {
        command.env(key, value);
    }
    Ok(())
}

/// The environment a compile command needs for per-user sccache routing, as
/// `(key, value)` pairs so the whole contract is observable without spawning
/// a process. The Water home is a parameter so tests can inject a scratch
/// directory instead of touching the real `~/.water` or depending on the
/// machine's home-path length. Only unix is fallible: it is the one host
/// that adds a socket under that home.
#[cfg(unix)]
fn compilation_cache_env_in(
    sccache_path: &Path,
    water_home: Option<&Path>,
) -> eyre::Result<Vec<(&'static str, OsString)>> {
    let mut env = base_compilation_cache_env(sccache_path);
    if let Some(socket) = water_home.map(server_socket_path_in).transpose()?.flatten() {
        env.push(("SCCACHE_SERVER_UDS", socket.into_os_string()));
    }
    Ok(env)
}

/// `compilation_cache_env_in` for hosts with no per-user socket: the
/// contract is the fixed pair list, so nothing here can fail.
#[cfg(not(unix))]
fn compilation_cache_env_in(
    sccache_path: &Path,
    _water_home: Option<&Path>,
) -> Vec<(&'static str, OsString)> {
    base_compilation_cache_env(sccache_path)
}

/// The pairs every host sets: `RUSTC_WRAPPER` routes each compile through
/// sccache and `SCCACHE_SERVER_PORT` namespaces its server to the user.
fn base_compilation_cache_env(sccache_path: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("RUSTC_WRAPPER", sccache_path.as_os_str().to_os_string()),
        (
            "SCCACHE_SERVER_PORT",
            per_user_server_port().to_string().into(),
        ),
    ]
}

/// `sun_path` is 108 bytes on Linux and 104 on macOS/BSD, including the
/// terminator — 103 keeps a socket path bindable on every unix host.
#[cfg(unix)]
const MAX_SUN_PATH_BYTES: usize = 103;

/// The unix socket a per-user sccache server listens on, under a dedicated
/// `0700` directory in the invoking user's Water home so no other account can
/// reach — or be reached by — it. `Ok(None)` when the path would not fit
/// `sun_path`: a socket that cannot bind is no fallback at all, so only the
/// per-user port is offered then.
///
/// # Errors
/// Returns an error when the socket directory cannot be created, or exists
/// with permissions wider than `0700` — sccache's server runs compile jobs
/// under its owner's identity with no authentication, so a socket another
/// account could traverse to is not an isolation mechanism and the build must
/// not silently fall back to the shared-address exposure.
#[cfg(unix)]
fn server_socket_path_in(water_home: &Path) -> eyre::Result<Option<PathBuf>> {
    let socket_dir = water_home.join("sccache");
    ensure_private_socket_dir(&socket_dir)?;
    let socket = socket_dir.join("server.sock");
    Ok((socket.as_os_str().len() <= MAX_SUN_PATH_BYTES).then_some(socket))
}

/// Create `dir` mode `0700`, or verify an existing one is that private. A
/// wider directory fails loudly: the socket inside is how one account would
/// submit compile jobs to another user's server, so narrowing the check to a
/// warning would leave the door it exists to close.
#[cfg(unix)]
fn ensure_private_socket_dir(dir: &Path) -> eyre::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    use eyre::WrapErr as _;

    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(dir)
        .wrap_err_with(|| format!("Failed to create sccache socket dir {}", dir.display()))?;
    let mode = std::fs::metadata(dir)
        .wrap_err_with(|| format!("Failed to stat sccache socket dir {}", dir.display()))?
        .mode()
        & 0o777;
    eyre::ensure!(
        mode.trailing_zeros() >= 6,
        "sccache socket dir {} has mode {mode:o}, wider than 0700 — other local \
         accounts could submit compile jobs to this user's sccache server. \
         Tighten it with `chmod 700 {}`.",
        dir.display(),
        dir.display()
    );
    Ok(())
}

/// A deterministic per-user TCP port for the sccache server, in the
/// 22000–31150 block below every supported host's ephemeral floor (Linux
/// 32768, Windows and macOS 49152) so a transient connection never occupies
/// it. A collision with an unrelated registered service is still possible;
/// that fails the server bind loudly instead of quietly joining another
/// user's server.
fn per_user_server_port() -> u16 {
    port_for_identity(&user_identity())
}

/// Spread a machine-unique user identity over the port block. FNV-1a needs
/// no state and no coordination between accounts.
fn port_for_identity(identity: &str) -> u16 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in identity.as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
    }
    22_000 + (hash % 9_151) as u16
}

/// The machine-unique identity of the invoking user. Hashing the user *name*
/// instead would let two accounts share a port — `ayy` and `cad` both
/// produced 46119 — and a name that cannot be read would pin every such
/// machine to one port. The uid is always present and distinct per account;
/// the FNV-1a reduction into 9151 slots can still map two uids to one port —
/// rare, and it re-shares a server rather than failing, so it is worth
/// keeping the identity as distinct as the OS makes possible.
#[cfg(unix)]
fn user_identity() -> String {
    nix::unistd::getuid().to_string()
}

/// The machine-unique identity of the invoking user: the account's SID string
/// (`S-1-5-21-…`), which is unique per machine and always present for a
/// running process.
#[cfg(windows)]
fn user_identity() -> String {
    use std::io;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    // SAFETY: every call queries the current process's own token; the token
    // buffer is sized by the API before the second `GetTokenInformation`
    // writes it, the handle is closed on every path past `OpenProcessToken`,
    // and the string the SID conversion allocates is freed with `LocalFree`.
    unsafe {
        let mut token = std::mem::zeroed();
        assert!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) != 0,
            "OpenProcessToken failed: {}",
            io::Error::last_os_error()
        );
        let mut size = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &raw mut size);
        // The buffer is read back as a TOKEN_USER, so it needs that struct's
        // alignment — u64 elements guarantee it on every Windows target.
        let mut buffer = vec![0u64; (size as usize).div_ceil(std::mem::size_of::<u64>())];
        let queried = size > 0
            && GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &raw mut size,
            ) != 0;
        CloseHandle(token);
        assert!(
            queried,
            "GetTokenInformation(TokenUser) failed: {}",
            io::Error::last_os_error()
        );
        let sid = (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid;
        let mut text = std::ptr::null_mut::<u16>();
        assert!(
            ConvertSidToStringSidW(sid, &raw mut text) != 0,
            "ConvertSidToStringSidW failed: {}",
            io::Error::last_os_error()
        );
        let mut length = 0usize;
        while *text.add(length) != 0 {
            length += 1;
        }
        let identity = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        identity
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!(
    "per-user sccache ports need a user-identity source; supported hosts are unix and Windows"
);

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

/// The sccache release that understands `SCCACHE_SERVER_UDS` — the mechanism
/// `configure_compilation_cache` uses to keep each user's compile server
/// private on unix hosts.
const MINIMUM_SCCACHE_VERSION: &str = "0.9.0";

/// `sccache` is on PATH; it still has to be new enough to honor the per-user
/// server address the compile path hands it, which only 0.9.0 does. An older
/// build gets the port fallback and keeps working, but a check that cannot
/// name the installed version — or finds one below the floor — reports it
/// instead of letting a quietly-shared host-wide server resurface.
async fn check_sccache_version(host: &Host) -> Result<(), ToolchainError<SccacheInstallation>> {
    let Ok(output) = host.output("sccache", ["--version"]).await else {
        return Err(ToolchainError::unfixable(
            "sccache is installed but `sccache --version` could not run",
            format!(
                "Reinstall sccache ({}) so it executes correctly, then re-run `water doctor`.",
                sccache_install_hint()
            ),
        ));
    };
    if !output.status.success() {
        return Err(ToolchainError::unfixable(
            "`sccache --version` exited with a failure",
            format!(
                "Reinstall sccache ({}) so `sccache --version` succeeds, then re-run `water doctor`.",
                sccache_install_hint()
            ),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let installed = text
        .split_whitespace()
        .nth(1)
        .and_then(|token| semver::Version::parse(token).ok());
    let Some(installed) = installed else {
        return Err(ToolchainError::unfixable(
            format!(
                "`sccache --version` printed an unreadable version: {}",
                text.trim()
            ),
            format!(
                "Install a released sccache build ({}), then re-run `water doctor`.",
                sccache_install_hint()
            ),
        ));
    };
    let minimum =
        semver::Version::parse(MINIMUM_SCCACHE_VERSION).expect("the version floor is valid semver");
    if installed.cmp_precedence(&minimum).is_lt() {
        return Err(ToolchainError::unfixable(
            format!(
                "sccache {installed} is too old: per-user build-cache isolation needs sccache {MINIMUM_SCCACHE_VERSION} or newer"
            ),
            format!(
                "Upgrade sccache — {} — then re-run `water doctor`.",
                sccache_upgrade_hint()
            ),
        ));
    }
    Ok(())
}

impl Toolchain for Sccache {
    type Installation = SccacheInstallation;

    async fn check(&self, host: &Host) -> Result<(), ToolchainError<Self::Installation>> {
        if host.which("sccache").await.is_ok() {
            check_sccache_version(host).await
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
    use std::ffi::OsString;
    use std::path::Path;

    use super::{
        Sccache, SccacheInstallation, compilation_cache_env_in, per_user_server_port,
        port_for_identity,
    };
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
    fn sccache_below_the_uds_floor_is_rejected() {
        let machine = TestMachine::new();
        machine.install("sccache");
        let host = machine.host([("WATERUI_FAKE_SCCACHE_VERSION", "0.8.2")]);
        let result = smol::block_on(Sccache.check(&host));
        let Err(ToolchainError::Unfixable(error)) = result else {
            panic!("an sccache below the UDS floor must be unfixable: {result:?}");
        };
        assert!(
            error.message().contains("0.8.2"),
            "the error names the installed version: {}",
            error.message()
        );
        assert!(
            error.message().contains("0.9.0"),
            "the error names the required version: {}",
            error.message()
        );
    }

    #[test]
    fn sccache_with_unreadable_version_is_rejected() {
        let machine = TestMachine::new();
        machine.install("sccache");
        let host = machine.host([("WATERUI_FAKE_SCCACHE_VERSION", "unknown")]);
        let result = smol::block_on(Sccache.check(&host));
        assert!(
            matches!(result, Err(ToolchainError::Unfixable(_))),
            "an sccache whose version cannot be read must be unfixable: {result:?}"
        );
    }

    #[test]
    fn port_is_deterministic_and_inside_the_reserved_block() {
        let port = per_user_server_port();
        assert_eq!(port, per_user_server_port());
        assert!(
            (22_000..=31_150).contains(&port),
            "the port stays below every host's ephemeral floor: {port}"
        );
    }

    #[test]
    fn distinct_identities_land_on_distinct_ports() {
        // 0 and 1 are the two uids that exist on every unix host; the names
        // that used to feed this hash (`ayy`/`cad`) collided.
        assert_ne!(port_for_identity("0"), port_for_identity("1"));
    }

    /// The environment contract: `RUSTC_WRAPPER` routes compiles through
    /// sccache, the port is always set — sccache < 0.9.0 knows nothing else —
    /// and unix additionally gets the socket that newer builds prefer. The
    /// Water home is injected so the test never touches the real `~/.water`
    /// or depends on this machine's home-path length.
    #[test]
    fn compilation_cache_env_sets_wrapper_port_and_unix_socket() {
        let water_home = tempfile::tempdir().expect("water home");
        #[cfg(unix)]
        let env =
            compilation_cache_env_in(Path::new("/toolchain/bin/sccache"), Some(water_home.path()))
                .expect("a scratch Water home yields the env");
        #[cfg(not(unix))]
        let env =
            compilation_cache_env_in(Path::new("/toolchain/bin/sccache"), Some(water_home.path()));

        assert!(
            env.contains(&("RUSTC_WRAPPER", OsString::from("/toolchain/bin/sccache"))),
            "RUSTC_WRAPPER routes rustc through sccache: {env:?}"
        );
        let port = env
            .iter()
            .find(|(key, _)| *key == "SCCACHE_SERVER_PORT")
            .map(|(_, value)| {
                value
                    .to_str()
                    .expect("port is text")
                    .parse::<u16>()
                    .expect("port parses")
            })
            .expect("SCCACHE_SERVER_PORT is always set");
        assert!((22_000..=31_150).contains(&port));

        #[cfg(unix)]
        {
            let socket = env
                .iter()
                .find(|(key, _)| *key == "SCCACHE_SERVER_UDS")
                .map(|(_, value)| value.to_string_lossy().into_owned())
                .expect("unix builds get the per-user socket");
            assert!(
                socket.ends_with("sccache/server.sock"),
                "the socket lives in a private dir under the Water home: {socket}"
            );
            assert!(
                socket.starts_with(&water_home.path().display().to_string()),
                "the socket lives under the injected Water home: {socket}"
            );
        }
        #[cfg(not(unix))]
        assert!(
            !env.iter().any(|(key, _)| *key == "SCCACHE_SERVER_UDS"),
            "non-unix builds only get the port"
        );
    }

    /// A socket path that cannot fit `sun_path` must not produce a socket
    /// that fails to bind — the port then carries the whole contract.
    #[cfg(unix)]
    #[test]
    fn oversized_home_path_falls_back_to_port_only() {
        let long_home = tempfile::tempdir()
            .expect("water home")
            .path()
            .join("a".repeat(200));
        assert!(
            super::server_socket_path_in(&long_home)
                .expect("creatable but overlong home")
                .is_none()
        );

        let home = tempfile::tempdir().expect("water home");
        let socket = super::server_socket_path_in(&home.path().join(".water"))
            .expect("a normal Water home gets a socket")
            .expect("a normal Water home gets a socket");
        assert!(socket.ends_with("sccache/server.sock"));
        assert!(
            socket
                .parent()
                .and_then(Path::parent)
                .is_some_and(|dir| dir.ends_with(".water")),
            "the socket's parent dir sits directly under the Water home: {}",
            socket.display()
        );
    }

    /// A socket dir another account can traverse is the exact exposure the
    /// mechanism exists to close — an existing `sccache/` wider than `0700`
    /// must fail rather than quietly offer the socket.
    #[cfg(unix)]
    #[test]
    fn a_socket_dir_wider_than_private_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().expect("water home");
        let socket_dir = home.path().join("sccache");
        std::fs::create_dir(&socket_dir).expect("socket dir");
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod socket dir");

        let error = super::server_socket_path_in(home.path())
            .expect_err("a world-traversable socket dir must be rejected");
        assert!(
            error.to_string().contains("0755") || error.to_string().contains("755"),
            "the error names the offending mode: {error}"
        );

        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
            .expect("tighten socket dir");
        super::server_socket_path_in(home.path())
            .expect("a 0700 socket dir is accepted")
            .expect("a 0700 socket dir yields a socket");
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
