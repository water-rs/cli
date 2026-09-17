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
