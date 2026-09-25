//! End-to-end check that a packaged binary finds every shared library its
//! dynamic section records.
//!
//! The fixture is the dependency arrangement `water build` produces for a
//! real project: a `dylib` crate pulled in over **git** — so Cargo hashes its
//! `-C metadata` into every `deps/` file name — linked into a binary with
//! `-Cprefer-dynamic`. rustc writes `deps/libwaterui_dylib-<metadata>.so`,
//! Cargo uplifts it to the unhashed `<profile>/libwaterui_dylib.so` the
//! artifact report names, and the binary's `DT_NEEDED` records the hashed
//! name. Staging the unhashed name ships a library the loader never looks
//! for (water-rs/cli#184); packaging must stage each needed library under
//! the name the binary itself records.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use target_lexicon::Triple;
use tempfile::{TempDir, tempdir};

use waterui_cli::build::{BuiltTarget, RustDynamicLibraries, needed_shared_libraries};
use waterui_cli::project::{ManagedBackends, Project};

/// Write `contents` to `path`, creating parent directories.
fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent dir");
    std::fs::write(path, contents).expect("write file");
}

/// Run a fixture command to success or fail the test with its output.
fn run(command: &mut Command, what: &str) -> Output {
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn {what}: {error}"));
    assert!(
        output.status.success(),
        "{what} failed with {}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The executable Cargo reported building for `app`'s `--bin` unit.
fn built_executable(app_dir: &Path) -> PathBuf {
    let mut child = Command::new("cargo")
        .args(["build", "--message-format=json"])
        .current_dir(app_dir)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn cargo build");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut stdout)
        .expect("read cargo messages");
    let status = child.wait().expect("wait on cargo build");
    assert!(status.success(), "fixture `cargo build` failed: {status}");

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let kinds = message["target"]["kind"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !kinds.iter().any(|kind| kind == "bin") {
            continue;
        }
        if let Some(executable) = message["executable"].as_str() {
            return PathBuf::from(executable);
        }
    }
    panic!("cargo reported no binary artifact for the fixture")
}

/// Whether `name` — a recorded dynamic dependency — is supplied by the
/// platform's own runtime rather than the packaged dist directory.
fn is_system_library(name: &str) -> bool {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_lowercase();
    // ELF system libraries the platform loader resolves.
    for prefix in [
        "linux-vdso",
        "linux-gate",
        "ld-linux",
        "ld-musl",
        "ld64",
        "libc.so",
        "libc.musl",
        "libm.so",
        "libdl.so",
        "librt.so",
        "libutil.so",
        "libresolv.so",
        "libpthread.so",
        "libthread_db.so",
        "libgcc_s",
        "libgcc",
        "libatomic.so",
        "libstdc++",
        "libasan",
        "libubsan",
        "libnsl.so",
    ] {
        if base.starts_with(prefix) {
            return true;
        }
    }
    // PE imports Windows itself supplies.
    if cfg!(windows)
        && matches!(
            base.split('.').next().unwrap_or(""),
            "kernel32"
                | "ntdll"
                | "msvcrt"
                | "ucrtbase"
                | "vcruntime140"
                | "vcruntime140_1"
                | "msvcp140"
                | "concrt140"
                | "advapi32"
                | "user32"
                | "gdi32"
                | "shell32"
                | "ole32"
                | "oleaut32"
                | "ws2_32"
                | "wship6"
                | "iphlpapi"
                | "dnsapi"
                | "bcrypt"
                | "crypt32"
                | "secur32"
                | "sspicli"
                | "rpcrt4"
                | "netapi32"
                | "winmm"
                | "imm32"
                | "version"
                | "psapi"
                | "dbghelp"
                | "shlwapi"
                | "comctl32"
                | "comdlg32"
                | "setupapi"
                | "cfgmgr32"
                | "powrprof"
                | "userenv"
                | "kernel.appcore"
                | "normaliz"
                | "winspool"
                | "gdiplus"
                | "dwmapi"
                | "uxtheme"
                | "msimg32"
                | "authz"
                | "fwpuclnt"
                | "oleacc"
                | "winhttp"
                | "wininet"
        )
    {
        return true;
    }
    if base.starts_with("api-ms-") {
        return true;
    }
    // Mach-O install names the OS supplies.
    name.starts_with("/usr/lib/")
        || name.starts_with("/System/Library/")
        || name.starts_with("@rpath/libswift")
}

/// The file name the fixture's `waterui_dylib` crate uplifts to for this
/// target — the name `BuiltTarget::shared_runtime` carries.
fn shared_runtime_name(triple: &Triple) -> String {
    if triple.operating_system == target_lexicon::OperatingSystem::Windows {
        "waterui_dylib.dll".to_owned()
    } else if triple.operating_system == target_lexicon::OperatingSystem::Darwin(None) {
        "libwaterui_dylib.dylib".to_owned()
    } else {
        "libwaterui_dylib.so".to_owned()
    }
}

/// Scaffold the fixture app under `root` and return its directory: a `dylib`
/// crate vendored in a local git repository — so Cargo hashes the source
/// into the dylib's `deps/` names — and a binary linking it dynamically.
fn scaffold_fixture(root: &Path) -> PathBuf {
    let dylib_dir = root.join("waterui-dylib");
    write(
        &dylib_dir.join("Cargo.toml"),
        "[package]\nname = \"waterui-dylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"waterui_dylib\"\ncrate-type = [\"dylib\", \"rlib\"]\n",
    );
    write(
        &dylib_dir.join("src/lib.rs"),
        "/// Marker the fixture binary calls so the linker keeps the dependency.\npub extern \"C\" fn fixture_marker() -> u8 {\n    42\n}\n",
    );
    run(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dylib_dir),
        "git init",
    );
    run(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dylib_dir),
        "git add",
    );
    run(
        Command::new("git")
            .args([
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "user.name=fixture",
            ])
            .args(["commit", "-qm", "fixture"])
            .current_dir(&dylib_dir),
        "git commit",
    );

    let dylib_url = url::Url::from_directory_path(&dylib_dir)
        .expect("dylib directory URL")
        .to_string();
    let app_dir = root.join("app");
    write(
        &app_dir.join("Cargo.toml"),
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nwaterui-dylib = {{ git = \"{dylib_url}\" }}\n"
        ),
    );
    write(
        &app_dir.join("src/main.rs"),
        "fn main() {\n    // A referenced symbol keeps the `DT_NEEDED` the link would\n    // otherwise drop under `--as-needed`.\n    assert_eq!(waterui_dylib::fixture_marker(), 42);\n}\n",
    );
    write(
        &app_dir.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"-Cprefer-dynamic\"]\n",
    );
    // The CLI opens a Water project; the resolve consults it for the
    // project's toolchain (which the fallback libstd lookup needs).
    write(
        &app_dir.join("Water.toml"),
        "[package]\ntype = \"app\"\nname = \"app\"\nbundle_identifier = \"dev.waterui.fixture\"\n",
    );
    app_dir
}

/// Assert every non-system dynamic dependency `executable` records exists in
/// `dist` under exactly the recorded file name.
fn assert_dist_satisfies_needed(executable: &Path, dist: &Path) {
    let needed = needed_shared_libraries(executable).expect("read dynamic dependencies");
    assert!(
        needed.iter().any(|name| name.contains("waterui_dylib")),
        "fixture binary records no waterui_dylib dependency: {needed:?}"
    );
    for name in &needed {
        if is_system_library(name) {
            continue;
        }
        let file_name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        assert!(
            dist.join(file_name).is_file(),
            "dist is missing {file_name} which {} records as needed\nstaged: {:?}",
            executable.display(),
            std::fs::read_dir(dist)
                .map(|entries| entries
                    .flatten()
                    .map(|entry| entry.file_name())
                    .collect::<Vec<_>>())
                .unwrap_or_default(),
        );
    }
}

/// Run the staged `executable` with `dist` on the library search path and
/// assert it exits successfully.
fn assert_staged_binary_runs(executable: &Path, dist: &Path) {
    let staged_exe = dist.join(executable.file_name().expect("exe name"));
    std::fs::copy(executable, &staged_exe).expect("stage executable");
    let mut run = Command::new(&staged_exe);
    if cfg!(windows) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dist.to_path_buf()];
        paths.extend(std::env::split_paths(&path));
        run.env("PATH", std::env::join_paths(paths).expect("join PATH"));
    } else if cfg!(target_os = "macos") {
        run.env("DYLD_FALLBACK_LIBRARY_PATH", dist);
    } else {
        run.env("LD_LIBRARY_PATH", dist);
    }
    let output = run
        .stdin(Stdio::null())
        .output()
        .expect("launch staged binary");
    assert!(
        output.status.success(),
        "staged binary failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build a binary that links a git-sourced `dylib` crate dynamically, run
/// `RustDynamicLibraries::resolve` + `stage` the way platform packaging
/// does, and assert the dist directory contains every non-system library the
/// binary's own dynamic records name — then run the staged binary.
#[test]
fn packaged_binary_finds_every_shared_library_it_records() {
    smol::block_on(async {
        let temporary: TempDir = tempdir().expect("tempdir");
        let root = temporary.path();
        let app_dir = scaffold_fixture(root);

        let executable = built_executable(&app_dir);
        let profile_dir = app_dir.join("target/debug");
        let triple = Triple::host();
        let shared_runtime = profile_dir.join(shared_runtime_name(&triple));
        assert!(
            shared_runtime.is_file(),
            "cargo did not produce the shared runtime {}",
            shared_runtime.display()
        );

        let built = BuiltTarget {
            profile_dir: profile_dir.clone(),
            artifact: executable.clone(),
            shared_runtime: Some(shared_runtime),
        };
        let project = Project::open(&app_dir, ManagedBackends::NONE)
            .await
            .expect("open fixture project");

        let libraries = RustDynamicLibraries::resolve(&built, &triple, &project)
            .await
            .expect("resolve shared libraries");
        let dist = root.join("dist");
        libraries
            .stage(&dist)
            .await
            .expect("stage shared libraries");

        assert_dist_satisfies_needed(&executable, &dist);
        assert_staged_binary_runs(&executable, &dist);
    });
}
