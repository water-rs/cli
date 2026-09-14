# AGENTS.md

The `water` command: it creates, builds, runs, previews, tests, benchmarks
and packages WaterUI applications. The framework's design principles and the
repository-boundary policy live in `water-rs/waterui` (`AGENTS.md` there) and
govern this crate too; this file carries only what is specific to this
repository. `CLAUDE.md` is a symlink to this file.

## Workflow

- **Finding a problem → GitHub issue in this repository. Solving it → pull
  request to `dev`.** One PR resolves one issue or lands one discrete fix, and
  its body links the issue (`Fixes #N`). `dev` and `main` are pull-request only;
  `main` is the release branch. Merging is the user's decision. A defect in the
  *framework* — a resolution channel that cannot carry a value the CLI needs, a
  scaffold-time fact missing from `framework.json` — is an issue in
  `water-rs/waterui`, and the CLI change waits for it.
- Lint bar: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo nextest run`, `cargo check --no-default-features --all-targets`. No new
  warnings, clippy warnings included. Format changed files with
  `rustfmt --edition 2024 <file>`.
- `Cargo.lock` is committed: this is a binary crate and CI, `cargo install
  --locked`, and `dist` all build the locked graph.
- release-plz owns the version and `CHANGELOG.md`; write conventional commits
  (`feat:`, `fix:`, `feat!:`) and never edit either by hand. The release
  workflow builds the prebuilt binaries with `dist`, hands the Homebrew formula
  to `water-rs/homebrew-water`, and publishes to crates.io over OIDC trusted
  publishing bound to `.github/workflows/release.yml` — renaming that file
  breaks publishing.

## Dependencies on the framework

The CLI links a handful of framework crates (`waterui-assets-core`,
`waterui-assets-planner`, the preview / inspector / MCP protocol crates). They
are pinned to one `water-rs/waterui` revision as git dependencies carrying the
`version` a release will resolve to, the same form `water-rs/gtk-backend` uses:
every framework crate at the same `rev`, so a shared type never exists twice.
Move the pin deliberately, all entries at once, with `cargo update -p` for the
lock; never mix a git copy of one crate with a registry copy of its sibling.
`cargo publish` strips `git` and keeps `version`, so a release can only ship
once those versions are on crates.io.

Everything else the CLI knows about the framework it learns at run time, never
at build time:

- The framework channel (`dev` / `nightly` / `stable`) is resolved in the
  library (`src/project_model/framework.rs`) into exact revisions and versions
  and persisted with the project. `stable` reads the `framework.json` the
  framework's release publishes; `dev` reads the integration branch; `nightly`
  reads the last certified prerelease. No channel falls back to another one.
- Framework-owned scaffold facts — backend coordinates, the Android API floor,
  `minimum-cli-version` — live in the framework's root
  `[package.metadata.waterui]` and reach the CLI through the resolved
  framework. `[package.metadata.waterui-scaffold]` in this manifest holds only
  what no framework manifest supplies, and `build.rs` refuses any other key.
- `ANDROID_NDK_VERSION` (`src/android/ndk_version.rs`) is a source literal so a
  `cargo install`ed CLI can name the NDK package to install; the nightly job
  checks it against the Android runtime's declared `ndkVersion` at the pinned
  framework revision.

## Design rules that bite here

- **Never recover semantics from user source code.** The CLI locates
  previews, tests, benchmarks, assets and capabilities by reading compiler
  artifacts (`waterui_meta_*` symbols in the user crate's rlib,
  `src/artifact_symbols.rs`), the manifests it owns (`Water.toml`), and
  `cargo metadata` — never by grepping or parsing `.rs` files. When no channel
  carries a fact, build the channel in the framework; do not scrape.
- **The terminal layer is thin.** `src/terminal/` parses arguments and formats
  output through `Shell`; every decision — resolution, device selection, build
  orchestration, doctor checks — lives in the library and is unit-testable
  without a terminal.
- **Every probe of the machine goes through `Host`** (`src/toolchain/host.rs`):
  PATH lookup, environment variables, process spawning. `Host::current()` is the
  real machine; tests build a `Host` over a scratch PATH of fake tools
  (`src/toolchain/testing.rs`, `src/toolchain/testdata/fake_tools.{sh,cmd}`)
  and an explicit environment map. No `std::env::var`, `which::which`, or
  `Command::new` outside the host seam, and no environment mutation in tests.
- **Fail fast, no fallbacks.** An unresolvable channel, a missing manifest key,
  a toolchain that reports the wrong shape — each is an error naming what was
  found and what was required, never a silent substitute. `doctor` classifies a
  problem as fixable or not; it does not paper over it.
- Diagnostics go through `tracing` (`RUST_LOG=debug water …` prints them to
  stderr; `--logs` on `water run` is the *device* log level), never `println!`.
  Structured text is serialized (`serde`) or rendered from a typed `askama`
  template under `templates/`, never concatenated.
- No blind sleeps: waiting is on a readiness signal (a port, a file, a device
  state), and `std::thread::sleep` is banned in tests.

## Testing

- `cargo nextest run` is the runner; every test runs in its own process, so
  nothing may rely on state a sibling test initialized.
- `tests/doctor_json.rs` runs the built binary with `doctor --json` and checks
  identity and schema of every item, never status — it must stay deterministic
  on any machine.
- Tests that need a framework checkout or the network are `#[ignore]` with the
  reason and run only in `nightly.yml` (`--run-ignored ignored-only`); the
  per-PR gate must pass offline on a clean runner.
- The nightly end-to-end legs run `doctor --fix`, `create`, `build`, `package`
  and `run` as a fresh OS user on macOS, Windows and Linux against the pinned
  framework revision. A failure there is a bug in this crate or the framework,
  not flakiness to retry.

## Architecture

### Crate Structure

The CLI is split into two parts:

1. **Library (`src/lib.rs`)** - Core logic, platform abstractions, device management
2. **Terminal (`src/terminal/`)** - User interface, argument parsing, output formatting

**Key principle: Terminal handles interaction only, library handles real logic.**

```
src/
├── lib.rs               # Library entry point (re-exports modules)
├── terminal/            # Binary entry point (UI layer)
│   ├── main.rs          # CLI argument parsing (clap)
│   ├── shell.rs         # Output formatting (spinners, colors, macros)
│   └── commands/        # Command implementations (thin wrappers)
├── platforming/         # `TargetPlatform`, `Backend`, bundle/share layouts
├── project_model/       # Water.toml, framework resolution, templates, assets
├── toolchain/           # `Host` seam, doctor, per-tool checks and installers
├── apple/ android/ gtk4/ hydrolysis/ esp32/   # Platform implementations
├── preview/ workflows/ mcp/ tui/ bench/       # Development-loop surfaces
├── artifact_symbols.rs  # `waterui_meta_*` symbol reads from compiled rlibs
└── templates/           # Scaffolding templates (askama; assets under templates/)
```

### Core Abstractions

### Device Trait (`platforming/`, per-platform `device.rs`)

Represents something that can run an app (simulator, emulator, physical device).

```rust
pub trait Device: Send {
    type Platform: Platform;
    
    /// Launch the device (boot simulator/emulator). No-op for physical devices.
    fn launch(&self) -> impl Future<Output = eyre::Result<()>> + Send;
    
    /// Run an artifact on the device. Device must be launched first.
    fn run(&self, artifact: Artifact, options: RunOptions) 
        -> impl Future<Output = Result<Running, FailToRun>> + Send;
    
    fn platform(&self) -> Self::Platform;
}
```

**Important**: `launch()` handles booting. For emulators that need to be started from cold, `launch()` should start the emulator process and wait until it's ready. For already-connected devices, `launch()` is a no-op.

Implementations:
- `AppleSimulator` - iOS/tvOS/watchOS simulator (boots via `simctl boot`)
- `MacOS` - Current machine (no-op launch)
- `AndroidDevice` - Connected Android device (waits for device via adb)
- `AndroidEmulator` - AVD that needs to be launched (starts emulator process)

### Platform Trait (`platforming/platform.rs`)

Represents a build target platform.

```rust
pub trait Platform: Send {
    type Toolchain: Toolchain;
    type Device: Device;
    
    fn scan(&self) -> impl Future<Output = eyre::Result<Vec<Self::Device>>> + Send;
    fn build(&self, project: &Project, options: BuildOptions) -> impl Future<...>;
    fn package(&self, project: &Project, options: PackageOptions) -> impl Future<...>;
    fn clean(&self, project: &Project) -> impl Future<...>;
    fn toolchain(&self) -> Self::Toolchain;
    fn triple(&self) -> Triple;
}
```

Implementations:
- `ApplePlatform` - iOS, iOS Simulator, macOS, tvOS, etc.
- `AndroidPlatform` - Android with different ABIs (arm64-v8a, x86_64, etc.)

### Project (`project_model/project.rs`)

Manages `Water.toml` manifest and orchestrates builds.

Key methods:
- `Project::open()` - Open existing project
- `Project::create()` - Create new project
- `Project::run()` - Build, package, launch device, and run app
- `Project::build()` - Build Rust library
- `Project::package()` - Package for platform

### Terminal Layer Conventions

Terminal commands in `src/terminal/commands/` should:

1. **Parse arguments** using clap
2. **Show progress** using `shell::spinner()`, `success!()`, `error!()`, etc.
3. **Delegate to library** for actual work
4. **Format output** for the user

Example pattern:
```rust
pub async fn run(args: Args) -> Result<()> {
    let project = Project::open(&args.path).await?;
    
    // Show progress
    let spinner = shell::spinner("Building...");
    
    // Delegate to library
    let result = project.build(platform, options).await;
    
    // Handle result with user-friendly output
    match result {
        Ok(_) => success!("Build complete"),
        Err(e) => error!("Build failed: {e}"),
    }
}
```

**Do NOT put heavy logic in terminal commands.** If you find yourself writing complex logic (loops, polling, process management), it belongs in the library layer.

### Device Lifecycle

The correct flow for running an app:

1. **Scan** - `Platform::scan()` returns available devices
2. **Select** - Choose a device (or create an emulator device if none available)
3. **Launch** - Terminal calls `Device::launch()` to boot simulator/emulator (can run in background while building)
4. **Run** - `Project::run()` builds, packages, and runs on the device (assumes device is already launched)

**Important**: `Project::run()` does NOT launch the device - it assumes the device is already launched and ready. The terminal layer (`water run` command) is responsible for:
- Spawning `device.launch()` as a background task
- Building and packaging the app in parallel with device launch
- Waiting for device to be ready before running the app

This allows simulator/emulator boot time to overlap with build time for better UX.

### Adding New Device Types

When adding a new device type:

1. Create a struct in the platform's `device.rs`
2. Implement `Device` trait
3. Put launching logic in `launch()` method
4. Reuse existing device's `run()` when possible

Example: `AndroidEmulator` (in `android/device.rs`):
```rust
pub struct AndroidEmulator {
    avd_name: String,
    device: OnceLock<AndroidDevice>,  // Set after launch
}

impl Device for AndroidEmulator {
    async fn launch(&self) -> Result<()> {
        // Start emulator process
        // Wait for it to boot (poll adb devices)
        // Store resulting AndroidDevice in self.device
    }
    
    async fn run(&self, artifact, options) -> Result<Running, FailToRun> {
        // Delegate to the inner AndroidDevice
        self.device.get().unwrap().run(artifact, options).await
    }
}
```

The terminal command just needs to create the right device type:
```rust
// In terminal/commands/run.rs
if let Some(dev) = devices.into_iter().next() {
    Ok(SelectedDevice::AndroidDevice(dev))
} else {
    // No connected devices - create an emulator device
    let avd = AndroidPlatform::list_avds().await?.first()...;
    Ok(SelectedDevice::AndroidEmulator(AndroidEmulator::new(avd)))
}
```

Then `Project::run()` calls `device.launch()` which handles the emulator startup.

### Error Handling

- Use `color_eyre::eyre::Result` for library functions
- Use `thiserror` for custom error enums (e.g., `FailToRun`, `FailToOpenProject`)
- Terminal layer converts errors to user-friendly messages

### Async Runtime

Uses `smol` for async:
- `smol::process::Command` for spawning processes
- `smol::spawn()` for background tasks
- `smol::Timer` for delays
- `smol::channel` for event streaming
- `smol::future::zip` for parallel operations
