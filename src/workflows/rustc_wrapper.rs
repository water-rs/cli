//! `RUSTC_WRAPPER` shim that recovers the `dylib` half of a `-Zbuild-std`
//! standard library.
//!
//! Cargo deliberately strips the `dylib` crate type from `std` when the
//! standard library is built from source (`-Zbuild-std`), so the only shared
//! `libstd` a `-Cprefer-dynamic` link can pick is rustup's prebuilt one — and
//! rustup ships the Android `libstd` with 4 KB-aligned `LOAD` segments, which
//! a device with 16 KB pages refuses to map.
//!
//! This wrapper supplies the missing half at the exact point Cargo cannot
//! express it. For the `std` unit of the configured target it appends
//! `--crate-type dylib` to the same rustc invocation that emits the rlib, so
//! both artifacts carry one strict version hash and every dependent crate
//! accepts the dylib as the same `std`. And for every other unit that
//! receives the `std` rlib through `--extern`, it appends the produced `.so`
//! as a second `std` extern: a crate named on the command line is never
//! re-resolved against the library search path, so the sibling dylib has to
//! be passed explicitly for `prefer-dynamic` to find it.
//!
//! The `water` binary itself is the wrapper (`RUSTC_WRAPPER` points at the
//! running executable) so the shim ships inside the same artifact a
//! `cargo install` produces. Cargo invokes the wrapper as
//! `water <rustc> <args…>`; the process enters here only when the cargo
//! invocation that spawned it also exported [`WRAPPER_MODE_ENV`], so a normal
//! `water` run never wanders into this path.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Marks the `water` process as a Cargo rustc wrapper rather than a CLI.
pub const WRAPPER_MODE_ENV: &str = "WATERUI_INTERNAL_RUSTC_WRAPPER";
/// Optional second wrapper (e.g. `sccache`) invoked between this shim and
/// rustc; unset means the rewritten arguments go to rustc directly.
pub const WRAPPER_CHAIN_ENV: &str = "WATERUI_RUSTC_WRAPPER_CHAIN";
/// The only `--target` triple this shim rewrites.
///
/// Host units (build scripts, proc macros) never carry the flag and are
/// passed through untouched, which keeps their host standard library on the
/// default rlib path.
pub const BUILD_STD_TARGET_ENV: &str = "WATERUI_BUILD_STD_TARGET";
/// Directory the produced `libstd-*.so` is published into — the Cargo profile
/// `deps/` directory, where the packaging step stages it from.
pub const BUILD_STD_DYLIB_DIR_ENV: &str = "WATERUI_BUILD_STD_DYLIB_DIR";

/// Run as a Cargo rustc wrapper when the invoking cargo configured us as one.
///
/// Returns `Some(exit_code)` in wrapper mode; the caller should exit the
/// process with it. `None` means this is an ordinary CLI invocation.
#[must_use]
pub fn wrapper_main() -> Option<i32> {
    std::env::var_os(WRAPPER_MODE_ENV)?;
    Some(run_wrapper())
}

fn run_wrapper() -> i32 {
    let mut invocation = std::env::args_os();
    let _self = invocation.next();
    let Some(rustc) = invocation.next() else {
        eprintln!("water: rustc wrapper invoked without a rustc path");
        return 1;
    };
    let args: Vec<OsString> = invocation.collect();
    let target = std::env::var_os(BUILD_STD_TARGET_ENV).unwrap_or_default();
    let rewritten = rewrite_args(&args, &target);

    let status = match std::env::var_os(WRAPPER_CHAIN_ENV) {
        Some(chain) => std::process::Command::new(chain)
            .arg(&rustc)
            .args(&rewritten.args)
            .status(),
        None => std::process::Command::new(&rustc)
            .args(&rewritten.args)
            .status(),
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            eprintln!("water: failed to invoke rustc wrapper target: {error}");
            return 1;
        }
    };

    if status.success()
        && rewritten.emits_std_dylib
        && emits_linked_output(&args)
        && let Some(publish_dir) = std::env::var_os(BUILD_STD_DYLIB_DIR_ENV)
    {
        let out_dir = arg_value(&args, "--out-dir");
        if let Err(error) = publish_std_dylib(Path::new(out_dir), Path::new(&publish_dir)) {
            eprintln!("water: failed to stage the build-std libstd dylib: {error}");
            return 1;
        }
    }

    status.code().unwrap_or(1)
}

/// The result of rewriting one rustc invocation.
struct Rewrite {
    args: Vec<OsString>,
    /// This invocation is the configured target's `std` unit with `dylib`
    /// added — on success a `libstd-*.so` exists under its `--out-dir`.
    emits_std_dylib: bool,
}

fn rewrite_args(args: &[OsString], target: &OsStr) -> Rewrite {
    if target.is_empty() || arg_value(args, "--target") != target {
        return Rewrite {
            args: args.to_vec(),
            emits_std_dylib: false,
        };
    }

    // Cargo spells the rlib crate type either `rlib` or `lib`; both need the
    // dylib companion.
    let std_crate_type = arg_value(args, "--crate-type");
    let is_std_rlib = arg_value(args, "--crate-name") == OsStr::new("std")
        && (std_crate_type == "rlib" || std_crate_type == "lib");
    // A metadata-only pass emits no dylib; rewriting it would only clobber
    // the rlib's `libstd-*.rmeta` with a second SVH.
    if is_std_rlib && emits_linked_output(args) {
        return Rewrite {
            args: rewrite_std_unit(args),
            emits_std_dylib: true,
        };
    }

    Rewrite {
        args: add_std_dylib_extern(args),
        emits_std_dylib: false,
    }
}

/// Compile the `std` unit as `rlib` + `dylib` in one invocation.
///
/// The dylib emits the dep `rlib`s' machine code, so each rmeta-only extern
/// Cargo hands the unit gains its rlib sibling as a second location for the
/// same crate — one candidate per artifact kind, one hash.
fn rewrite_std_unit(args: &[OsString]) -> Vec<OsString> {
    let is_rlib_type = |value: &OsStr| value == "rlib" || value == "lib";
    let mut rewritten = Vec::with_capacity(args.len() + 8);
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let split =
            arg == "--crate-type" && args.get(index + 1).is_some_and(|value| is_rlib_type(value));
        let joined = arg == "--crate-type=rlib" || arg == "--crate-type=lib";
        if !split && !joined {
            rewritten.push(arg.clone());
            index += 1;
            continue;
        }
        rewritten.push(arg.clone());
        if split {
            rewritten.push(args[index + 1].clone());
            index += 2;
        } else {
            index += 1;
        }
        rewritten.extend([OsString::from("--crate-type"), OsString::from("dylib")]);
    }
    for spec in extern_values(args) {
        let spec = spec.to_string_lossy();
        if !spec.ends_with(".rmeta") {
            continue;
        }
        let rlib = format!("{}.rlib", spec.trim_end_matches(".rmeta"));
        let Some((_, path)) = rlib.rsplit_once('=') else {
            continue;
        };
        // Cargo pipelines the `std` unit behind its dependencies' metadata
        // alone: the `dylib` crate type added above codegens against the dep
        // rlibs, which the still-running dep units have not written yet. The
        // rlib appears when that unit finishes; waiting for the file — never
        // for a fixed delay — is the only ordering the shim can impose.
        if !wait_for_file(Path::new(path)) {
            eprintln!(
                "water: build-std dependency rlib never appeared: {path}; \
                 compiling std without it will fail"
            );
            continue;
        }
        rewritten.push(OsString::from("--extern"));
        rewritten.push(OsString::from(rlib));
    }
    rewritten
}

/// Poll until `path` exists, up to a bound far beyond any dependency's
/// codegen time. Returns `false` on timeout so a genuinely missing artifact
/// surfaces as rustc's own error rather than a silent drop.
fn wait_for_file(path: &Path) -> bool {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !path.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    path.is_file()
}

/// Hand every link-emitting unit that depends on `std` the freshly built
/// dylib as a second `std` extern, so `-Cprefer-dynamic` resolves to it.
///
/// Cargo may express the dependency as either the `libstd-*.rlib` or the
/// `libstd-*.rmeta` produced in the `std` unit's own out dir; the dylib sits
/// next to both. A unit that only reads metadata never links `std`, so only
/// `--emit` sets containing `link` are rewritten — and only those wait for
/// the dylib, since Cargo starts them no earlier than the unit producing it.
fn add_std_dylib_extern(args: &[OsString]) -> Vec<OsString> {
    if !emits_linked_output(args) {
        return args.to_vec();
    }
    let mut rewritten = args.to_vec();
    for spec in extern_values(args) {
        let spec = spec.to_string_lossy();
        let Some((name, path)) = spec.rsplit_once('=') else {
            continue;
        };
        let is_std = name.rsplit(':').next() == Some("std")
            && Path::new(path)
                .file_name()
                .is_some_and(|file| file.to_string_lossy().starts_with("libstd-"));
        if !is_std {
            continue;
        }
        let dylib = Path::new(path).with_extension("so");
        // The `std` unit emits the dylib at the end of the same invocation
        // that wrote the rmeta/rlib this extern points at; if pipelining let
        // this unit start early, the dylib is on its way — wait for the file,
        // and let rustc's own error surface if it never arrives.
        if wait_for_file(&dylib) {
            rewritten.push(OsString::from("--extern"));
            rewritten.push(OsString::from(format!("{}={}", name, dylib.display())));
        } else {
            eprintln!(
                "water: build-std libstd dylib never appeared at {}; \
                 this unit will link `std` statically",
                dylib.display()
            );
        }
    }
    rewritten
}

/// Every `--extern` spec in the invocation, covering both the split
/// (`--extern <spec>`) and joined (`--extern=<spec>`) forms.
fn extern_values(args: &[OsString]) -> Vec<OsString> {
    let mut specs = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--extern" {
            if let Some(spec) = iter.next() {
                specs.push(spec.clone());
            }
        } else if let Some(spec) = arg.to_str().and_then(|arg| arg.strip_prefix("--extern=")) {
            specs.push(OsString::from(spec));
        }
    }
    specs
}

/// The value paired with `flag` in split or joined (`--flag=value`) form.
fn arg_value<'a>(args: &'a [OsString], flag: &str) -> &'a OsStr {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            if let Some(value) = iter.next() {
                return value;
            }
        } else if let Some(value) = arg.to_str().and_then(|arg| {
            arg.strip_prefix(flag)
                .and_then(|rest| rest.strip_prefix('='))
        }) {
            return OsStr::new(value);
        }
    }
    OsStr::new("")
}

/// Whether this invocation links an artifact (as opposed to a metadata-only
/// check emit), which is when the added dylib actually lands on disk.
fn emits_linked_output(args: &[OsString]) -> bool {
    let emit = arg_value(args, "--emit");
    emit.is_empty() || emit.to_string_lossy().split(',').any(|kind| kind == "link")
}

/// Move the produced `libstd-*.so` from the unit's out dir into the profile's
/// `deps/` directory, replacing whatever an earlier toolchain left there.
///
/// Cargo compiles the `std` unit straight into `deps/`, so the usual case is
/// `out_dir == publish_dir`: the produced file must survive the stale sweep,
/// and a copy onto itself would truncate it.
fn publish_std_dylib(out_dir: &Path, publish_dir: &Path) -> std::io::Result<()> {
    let mut produced: Vec<PathBuf> = std::fs::read_dir(out_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| is_std_dylib_file_name(path.file_name()))
        .collect();
    produced.sort_unstable();
    let [source] = produced.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "expected exactly one libstd-*.so in {}, found {}",
                out_dir.display(),
                produced.len()
            ),
        ));
    };

    std::fs::create_dir_all(publish_dir)?;
    let destination = publish_dir.join(source.file_name().unwrap_or_default());
    if source == &destination {
        for entry in std::fs::read_dir(publish_dir)? {
            let path = entry?.path();
            if path != *source && is_std_dylib_file_name(path.file_name()) {
                std::fs::remove_file(&path)?;
            }
        }
    } else {
        for entry in std::fs::read_dir(publish_dir)? {
            let path = entry?.path();
            if is_std_dylib_file_name(path.file_name()) {
                std::fs::remove_file(&path)?;
            }
        }
        std::fs::copy(source, &destination)?;
    }
    Ok(())
}

/// Whether a directory entry is a `libstd-*.so` shared library.
fn is_std_dylib_file_name(file_name: Option<&OsStr>) -> bool {
    file_name.is_some_and(|name| {
        name.to_string_lossy().starts_with("libstd-")
            && Path::new(name).extension() == Some(OsStr::new("so"))
    })
}
#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use tempfile::tempdir;

    use super::{arg_value, publish_std_dylib, rewrite_args};

    fn os(strings: &[&str]) -> Vec<OsString> {
        strings.iter().map(OsString::from).collect()
    }

    #[test]
    fn leaves_units_for_other_targets_alone() {
        let args = os(&[
            "--crate-name",
            "std",
            "--crate-type",
            "rlib",
            "--target",
            "x86_64-linux-android",
        ]);
        let rewritten = rewrite_args(&args, OsString::from("aarch64-linux-android").as_os_str());
        assert_eq!(rewritten.args, args);
        assert!(!rewritten.emits_std_dylib);
    }

    #[test]
    fn leaves_host_units_alone() {
        let args = os(&["--crate-name", "std", "--crate-type", "rlib"]);
        let rewritten = rewrite_args(&args, OsString::from("aarch64-linux-android").as_os_str());
        assert_eq!(rewritten.args, args);
        assert!(!rewritten.emits_std_dylib);
    }

    #[test]
    fn std_unit_gains_dylib_and_rlib_externs() {
        let dir = tempdir().expect("deps dir");
        let rmeta = dir.path().join("libcore-abc.rmeta");
        let rlib = dir.path().join("libcore-abc.rlib");
        std::fs::write(&rmeta, []).expect("rmeta");
        std::fs::write(&rlib, []).expect("rlib");

        let args = os(&[
            "--crate-name",
            "std",
            "--crate-type",
            "rlib",
            "--target",
            "aarch64-linux-android",
            "--extern",
            &format!("noprelude:core={}", rmeta.display()),
        ]);
        let rewritten = rewrite_args(&args, OsString::from("aarch64-linux-android").as_os_str());
        assert!(rewritten.emits_std_dylib);
        assert!(
            rewritten
                .args
                .windows(2)
                .any(|w| w == [OsString::from("--crate-type"), OsString::from("dylib")])
        );
        let expected = OsString::from(format!(
            "noprelude:core={}.rlib",
            rmeta.display().to_string().trim_end_matches(".rmeta")
        ));
        assert!(rewritten.args.contains(&expected));
    }

    #[test]
    fn dependents_get_the_std_dylib_extern_for_rlib_and_rmeta() {
        let dir = tempdir().expect("std out dir");
        let rlib = dir.path().join("libstd-abc123.rlib");
        let rmeta = dir.path().join("libstd-def456.rmeta");
        let dylib_rlib = dir.path().join("libstd-abc123.so");
        let dylib_rmeta = dir.path().join("libstd-def456.so");
        std::fs::write(&rlib, []).expect("rlib");
        std::fs::write(&rmeta, []).expect("rmeta");
        std::fs::write(&dylib_rlib, []).expect("dylib for rlib extern");
        std::fs::write(&dylib_rmeta, []).expect("dylib for rmeta extern");

        let args = os(&[
            "--crate-name",
            "waterui_preview",
            "--target",
            "aarch64-linux-android",
            "--emit=dep-info,metadata,link",
            "--extern",
            &format!("noprelude,nounused:std={}", rlib.display()),
            "--extern",
            &format!("std={}", rmeta.display()),
            "--extern",
            &format!("std_detect={}/libstd_detect-zz.rlib", dir.path().display()),
        ]);
        let rewritten = rewrite_args(&args, OsString::from("aarch64-linux-android").as_os_str());
        for expected in [
            format!("noprelude,nounused:std={}", dylib_rlib.display()),
            format!("std={}", dylib_rmeta.display()),
        ] {
            assert!(
                rewritten.args.contains(&OsString::from(&expected)),
                "missing std dylib extern {expected}: {:?}",
                rewritten.args
            );
        }
        assert!(
            !rewritten
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("libstd_detect-zz.so")),
            "std_detect must not be rewritten"
        );
    }

    #[test]
    fn metadata_only_units_are_not_rewritten() {
        let dir = tempdir().expect("std out dir");
        let rmeta = dir.path().join("libstd-abc123.rmeta");
        let dylib = dir.path().join("libstd-abc123.so");
        std::fs::write(&rmeta, []).expect("rmeta");
        std::fs::write(&dylib, []).expect("dylib");

        let args = os(&[
            "--crate-name",
            "waterui_preview",
            "--target",
            "aarch64-linux-android",
            "--emit=dep-info,metadata",
            "--extern",
            &format!("std={}", rmeta.display()),
        ]);
        let rewritten = rewrite_args(&args, OsString::from("aarch64-linux-android").as_os_str());
        assert_eq!(rewritten.args, args);
    }

    #[test]
    fn publishes_the_dylib_and_replaces_stale_ones() {
        let out = tempdir().expect("out dir");
        let publish = tempdir().expect("publish dir");
        std::fs::write(out.path().join("libstd-new.so"), b"new").expect("new libstd");
        std::fs::write(publish.path().join("libstd-old.so"), b"old").expect("stale libstd");
        std::fs::write(publish.path().join("libwaterui_dylib.so"), b"w").expect("other lib");

        publish_std_dylib(out.path(), publish.path()).expect("publish");

        assert!(!publish.path().join("libstd-old.so").exists());
        assert_eq!(
            std::fs::read(publish.path().join("libstd-new.so")).expect("read published"),
            b"new"
        );
        assert!(publish.path().join("libwaterui_dylib.so").exists());
    }

    #[test]
    fn arg_value_reads_split_and_joined_forms() {
        let args = os(&["--out-dir", "/tmp/out", "--emit=dep-info,metadata,link"]);
        assert_eq!(arg_value(&args, "--out-dir"), OsStr::new("/tmp/out"));
        assert_eq!(arg_value(&args, "--emit"), "dep-info,metadata,link");
        assert_eq!(arg_value(&args, "--missing"), "");
    }
}
