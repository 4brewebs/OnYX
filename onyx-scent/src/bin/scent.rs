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
        /// Disables the persistent `.onyx/scent.idx` cache (in-memory only)
        #[arg(long)]
        no_cache: bool,
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
        /// Disables the persistent `.onyx/scent.idx` cache (in-memory only)
        #[arg(long)]
        no_cache: bool,
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
        /// Disables the persistent `.onyx/scent.idx` cache (in-memory only)
        #[arg(long)]
        no_cache: bool,
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
            no_cache,
        } => {
            let opts = ToonPageOptions {
                cursor,
                max_rows,
                public_only,
                ..ToonPageOptions::default()
            };
            let out = if no_cache {
                index.index_target(&root, path.as_deref())?;
                index.outline_toon(opts)
            } else {
                index.index_target_with_cache(&root, path.as_deref())?;
                index.outline_toon(opts)
            };
            print!("{out}");
        }
        Commands::Impact {
            root,
            target,
            max_hops,
            cursor,
            max_rows,
            no_cache,
        } => {
            let opts = ToonPageOptions {
                cursor,
                max_rows,
                ..ToonPageOptions::default()
            };
            if no_cache {
                index.index_workspace(&root)?;
            } else {
                index.index_workspace_with_cache(&root)?;
            }
            let out = index.impact_toon(
                &target,
                &DistillerConfig {
                    iterations: max_hops,
                    ..DistillerConfig::default()
                },
                opts,
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
            no_cache,
        } => {
            if no_cache {
                index.index_workspace(&root)?;
            } else {
                index.index_workspace_with_cache(&root)?;
            }
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
