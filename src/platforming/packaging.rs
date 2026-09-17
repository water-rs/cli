//! Helpers shared by the backends that ship a Cargo-built binary.

use std::path::{Path, PathBuf};

use crate::utils::copy_file;

/// Copy `binary_path` into `dir` under `name`, returning the packaged path.
///
/// Generated backend binaries carry the crate's project-root tag so
/// same-named projects cannot clobber each other in the shared Cargo target
/// directory. The tag is internal to the build — a packaged executable ships
/// under the product name. Copying, rather than renaming, also leaves
/// Cargo's own artifact in place for its freshness checks.
///
/// # Errors
/// Returns an error if the binary cannot be copied.
pub async fn stage_binary_as(binary_path: &Path, dir: &Path, name: &str) -> eyre::Result<PathBuf> {
    let destination = dir.join(name);
    copy_file(binary_path, &destination).await?;
    Ok(destination)
}

/// The `dist/<platform>[/<profile>]` directory a backend's shipped
/// artifacts stage into.
///
/// `backend_path` already carries the per-project `managed_backends`
/// component, so a staged executable — and the runtime files `$ORIGIN`
/// resolves beside it — lands in a path unique to the project. Staging
/// into the shared Cargo profile directory instead would collide two
/// same-named projects on `<profile>/<product>`: last-writer-wins copies,
/// `ETXTBSY` against a still-running staged binary, and previously
/// reported artifact paths silently re-targeting a sibling's bytes.
#[must_use]
pub fn dist_dir(backend_path: &Path, platform: &str, profile: Option<&str>) -> PathBuf {
    let mut dir = backend_path.join("dist").join(platform);
    if let Some(profile) = profile {
        dir = dir.join(profile);
    }
    dir
}

#[cfg(test)]
mod tests {
    /// The packaged path is the product name; the tagged Cargo artifact it
    /// was copied from stays in place.
    #[test]
    fn staged_binary_ships_under_the_product_name() {
        smol::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let binary = dir.path().join("demo-gtk4-deadbeef");
            std::fs::write(&binary, b"bin").expect("binary");
            let staged = super::stage_binary_as(&binary, dir.path(), "demo-gtk4")
                .await
                .expect("binary must stage");
            assert_eq!(staged, dir.path().join("demo-gtk4"));
            assert_eq!(
                std::fs::read(&staged).expect("staged binary must read"),
                b"bin"
            );
            assert!(binary.exists(), "the tagged Cargo artifact is kept");
        });
    }
}
