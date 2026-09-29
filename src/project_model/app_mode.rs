//! Detection of projects created in the removed app mode.
//!
//! App mode scaffolded user-owned native projects (Xcode, Gradle, and the
//! Rust backend crates) into the project directory and recorded them in
//! `Water.toml`. The CLI now generates and manages every backend project in
//! the build cache, so such a project is refused with the exact keys and
//! directories to delete — it is never silently reinterpreted.

use std::fmt;
use std::path::{Path, PathBuf};

/// `Water.toml` keys only app mode read, as `(table path, key)`; an empty key
/// names the whole table.
const APP_MODE_KEYS: &[(&[&str], &str)] = &[
    (&["package"], "type"),
    (&["backends"], "path"),
    (&["backends", "apple"], "project_path"),
    (&["backends", "apple"], "scheme"),
    (&["backends", "apple"], "branch"),
    (&["backends", "apple"], "revision"),
    (&["backends", "android"], "project_path"),
    (&["backends", "android"], "version"),
    (&["backends", "esp32"], "project_path"),
    (&["backends", "gtk4"], ""),
    (&["backends", "hydrolysis"], ""),
    (&["backends", "winui"], ""),
];

/// Directories app mode scaffolded user-owned backend projects into, below the
/// project root or its `backends/` directory.
const APP_MODE_DIRECTORIES: &[&str] = &[
    "apple",
    "android",
    "gtk4",
    "hydrolysis",
    "winui",
    "esp32",
    "ffi",
];

/// What an app-mode project carries that the CLI no longer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppModeLeftovers {
    /// Dotted `Water.toml` keys, such as `backends.apple.scheme`.
    pub keys: Vec<String>,
    /// Scaffolded native project directories, relative to the project root.
    pub directories: Vec<PathBuf>,
}

impl AppModeLeftovers {
    /// Collect the app-mode leftovers of the project at `root` whose
    /// `Water.toml` parsed to `manifest`. `None` when there are none.
    #[must_use]
    pub fn find(root: &Path, manifest: &toml::Table) -> Option<Self> {
        let keys: Vec<String> = APP_MODE_KEYS
            .iter()
            .filter(|(table, key)| {
                let Some(table) = table
                    .iter()
                    .try_fold(manifest, |table, name| table.get(*name)?.as_table())
                else {
                    return false;
                };
                key.is_empty() || table.contains_key(*key)
            })
            .map(|(table, key)| {
                let mut dotted = table.join(".");
                if !key.is_empty() {
                    dotted.push('.');
                    dotted.push_str(key);
                }
                dotted
            })
            .collect();
        let directories: Vec<PathBuf> = [Path::new(""), Path::new("backends")]
            .into_iter()
            .flat_map(|base| APP_MODE_DIRECTORIES.iter().map(move |name| base.join(name)))
            .filter(|relative| root.join(relative).is_dir())
            .collect();
        (!keys.is_empty() || !directories.is_empty()).then_some(Self { keys, directories })
    }
}

impl fmt::Display for AppModeLeftovers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "This project carries configuration from the removed app and playground \
             package types: the CLI now generates and manages every backend project \
             itself, and `[package] type` no longer selects a mode. \
             Delete the following, then run the command again."
        )?;
        if !self.keys.is_empty() {
            writeln!(f, "Water.toml keys that are no longer read:")?;
            for key in &self.keys {
                writeln!(f, "  - {key}")?;
            }
        }
        if !self.directories.is_empty() {
            writeln!(f, "Scaffolded native project directories:")?;
            for directory in &self.directories {
                writeln!(f, "  - {}", directory.display())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppModeLeftovers;
    use std::path::PathBuf;

    fn parse(text: &str) -> toml::Table {
        text.parse().expect("fixture manifest parses")
    }

    #[test]
    fn a_current_manifest_has_no_leftovers() {
        let root = tempfile::tempdir().unwrap();
        let manifest = parse(
            r#"
                [package]
                name = "Demo"
                bundle_identifier = "dev.waterui.demo"

                [backends.apple]
                backend_path = "../apple-backend"

                [backends.android]
                backend_path = "/opt/android-backend"

                [backends.esp32]
                chip = "esp32s3"
            "#,
        );
        assert_eq!(AppModeLeftovers::find(root.path(), &manifest), None);
    }

    #[test]
    fn app_mode_keys_and_directories_are_named() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("backends/apple")).unwrap();
        std::fs::create_dir_all(root.path().join("android")).unwrap();
        let manifest = parse(
            r#"
                [package]
                type = "app"
                name = "Demo"
                bundle_identifier = "dev.waterui.demo"

                [backends]
                path = "backends"

                [backends.apple]
                scheme = "demo"
                backend_path = "../apple-backend"

                [backends.gtk4]
                project_path = "gtk4"
            "#,
        );
        let leftovers = AppModeLeftovers::find(root.path(), &manifest).expect("app-mode project");
        assert_eq!(
            leftovers.keys,
            [
                "package.type",
                "backends.path",
                "backends.apple.scheme",
                "backends.gtk4"
            ]
        );
        assert_eq!(
            leftovers.directories,
            [PathBuf::from("android"), PathBuf::from("backends/apple")]
        );
        let message = leftovers.to_string();
        assert!(message.contains("backends.apple.scheme"));
        assert!(message.contains("backends/apple"));
    }

    /// `type = "playground"` is the removed mode selector too: the key is no
    /// longer read, so it is named rather than silently accepted.
    #[test]
    fn the_playground_type_key_is_named() {
        let root = tempfile::tempdir().unwrap();
        let manifest = parse(
            r#"
                [package]
                type = "playground"
                name = "Demo"
                bundle_identifier = "dev.waterui.demo"
            "#,
        );
        let leftovers = AppModeLeftovers::find(root.path(), &manifest).expect("mode key");
        assert_eq!(leftovers.keys, ["package.type"]);
        assert!(leftovers.directories.is_empty());
    }
}
