//! Autoshop — AI-assisted automatic development of RAW photographs.
//!
//! Architecture in one line: the AI advisor looks at a RAW preview + metadata
//! and emits an [`recipe::EditRecipe`]; a deterministic render engine applies
//! that recipe. See `docs/ARCHITECTURE.md` for the full design.
//!
//! This is the initial scaffold: the data model and CLI surface are real and
//! compile/test clean; the decode → advise → render pipeline is stubbed and
//! returns an explicit "not yet implemented" error so nothing silently lies
//! about being done.

mod recipe;

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use recipe::EditRecipe;

#[derive(Parser)]
#[command(
    name = "autoshop",
    version,
    about = "AI-assisted automatic development of RAW photographs",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode a RAW file, extract a preview + metadata, ask the AI advisor,
    /// and write the resulting EditRecipe as JSON (no pixels rendered).
    Analyze {
        /// Path to the RAW file (e.g. .CR3, .NEF, .ARW, .RAF, .DNG).
        raw: PathBuf,
        /// Where to write the recipe JSON (default: <raw>.recipe.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Apply an existing EditRecipe to a RAW file and render an output image.
    Apply {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Path to the recipe JSON produced by `analyze`.
        recipe: PathBuf,
        /// Output image path (extension selects format: .tif / .jpg / .png).
        #[arg(short, long)]
        out: PathBuf,
    },
    /// End-to-end: analyze then apply in one shot.
    Auto {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Output image path.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Print the default EditRecipe as JSON — the exact shape the AI must emit.
    RecipeSchema,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::RecipeSchema => {
            // Genuinely useful today: this is the schema we hand the AI as the
            // required output format.
            let template = EditRecipe::default();
            println!("{}", serde_json::to_string_pretty(&template)?);
            Ok(())
        }
        Command::Analyze { raw, out } => {
            let _ = (raw, out);
            bail!("`analyze` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M1");
        }
        Command::Apply { raw, recipe, out } => {
            let _ = (raw, recipe, out);
            bail!("`apply` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M2");
        }
        Command::Auto { raw, out } => {
            let _ = (raw, out);
            bail!("`auto` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M3");
        }
    }
}
