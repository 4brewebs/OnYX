use std::path::PathBuf;

use clap::{Parser, Subcommand};
use onyx_scent::{DistillerConfig, ToonPageOptions, WorkspaceIndex};

#[derive(Parser)]
#[command(name = "scent")]
#[command(about = "Deterministic AST distiller and TOON serializer for OnYX")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Emit a compact @TOON v1 outline for a workspace, directory, or file
    Outline {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        #[arg(long, default_value_t = 250)]
        max_rows: usize,
        #[arg(long)]
        public_only: bool,
    },
    /// Emit ranked @TOON v1 thermal impact neighbors around a symbol
    Impact {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long, default_value_t = 3)]
        max_hops: u8,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        #[arg(long, default_value_t = 250)]
        max_rows: usize,
    },
    /// Emit @TOON v1 definitions and direct cross-references for a symbol
    References {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
        #[arg(long, default_value_t = 250)]
        max_rows: usize,
        #[arg(long)]
        public_only: bool,
        #[arg(long)]
        exclude_tests: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut index = WorkspaceIndex::new();

    match cli.command {
        Commands::Outline {
            root,
            path,
            cursor,
            max_rows,
            public_only,
        } => {
            index.index_target(&root, path.as_deref())?;
            let out = index.outline_toon(ToonPageOptions {
                cursor,
                max_rows,
                public_only,
                ..ToonPageOptions::default()
            });
            print!("{out}");
        }
        Commands::Impact {
            root,
            target,
            max_hops,
            cursor,
            max_rows,
        } => {
            index.index_workspace(&root)?;
            let out = index.impact_toon(
                &target,
                &DistillerConfig {
                    iterations: max_hops,
                    ..DistillerConfig::default()
                },
                ToonPageOptions {
                    cursor,
                    max_rows,
                    ..ToonPageOptions::default()
                },
            )?;
            print!("{out}");
        }
        Commands::References {
            root,
            target,
            cursor,
            max_rows,
            public_only,
            exclude_tests,
        } => {
            index.index_workspace(&root)?;
            let out = index.references_toon(
                &target,
                ToonPageOptions {
                    cursor,
                    max_rows,
                    public_only,
                    exclude_tests,
                    ..ToonPageOptions::default()
                },
            )?;
            print!("{out}");
        }
    }
    Ok(())
}
