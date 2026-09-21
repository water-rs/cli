//! `water fetch` command implementation.

use std::path::PathBuf;

use clap::Args as ClapArgs;
use eyre::Result;

use crate::shell::Shell;
use crate::{note, success, warn};
use waterui_cli::FetchOutcome;
use waterui_cli::project::{ManagedBackends, Project};

/// Arguments for the fetch command.
#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Project directory whose fonts are fetched (defaults to the current
    /// directory).
    #[arg(long, default_value = ".")]
    path: PathBuf,
}

/// Run the fetch command.
pub async fn run(shell: &Shell, args: Args) -> Result<()> {
    let project_path = crate::project_path::canonicalize(&args.path)?;
    let project = Project::open(&project_path, ManagedBackends::NONE).await?;

    let spinner = shell.spinner("Fetching fonts...");
    let outcomes = waterui_cli::seed_font_cache(&project).await;
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    let outcomes = outcomes?;

    if outcomes.is_empty() {
        note!(shell, "No fonts are declared");
        return Ok(());
    }

    let mut unsatisfied = 0usize;
    for outcome in outcomes {
        match outcome {
            FetchOutcome::Satisfied { name, path } => {
                note!(
                    shell,
                    "Font '{name}' is already cached at {}",
                    path.display()
                );
            }
            FetchOutcome::Fetched { name, path } => {
                success!(shell, "Fetched font '{name}' to {}", path.display());
            }
            // Declarations fetching cannot fix — a name the registry does not
            // know, a missing crate-local file — come back with the same
            // report the build gives them.
            FetchOutcome::Unsatisfiable { error, .. } => {
                unsatisfied += 1;
                warn!(shell, "{error:#}");
            }
        }
    }

    if unsatisfied > 0 {
        eyre::bail!(
            "{unsatisfied} declared font(s) cannot be satisfied by fetching — the build \
             reports them the same way"
        );
    }
    Ok(())
}
