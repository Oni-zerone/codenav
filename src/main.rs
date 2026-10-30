mod explore;
mod extract;
mod graph;
mod indexer;
mod lang;
mod mcp;
mod output;
mod query;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use crate::graph::Graph;
use crate::lang::Lang;

/// Index a repository into a code graph and serve it to LLMs over MCP.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build (or rebuild) the graph and save it to `<repo>/.codenav/graph.json`.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Run the MCP server on stdio (indexes first if needed).
    Serve {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Always re-index on startup instead of reusing a fresh cached graph.
        #[arg(long)]
        fresh: bool,
        /// Do not re-index automatically when source files change.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Call one MCP tool from the command line and print its JSON result.
    Tool {
        /// Tool name, e.g. graph_overview, search_symbols, get_symbol, impact_analysis.
        name: String,
        /// Tool arguments as a JSON object.
        #[arg(default_value = "{}")]
        args: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Print the tree-sitter syntax tree of a file (useful to write extraction queries).
    Ast { file: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Index { path } => {
            let root = canonical_root(&path)?;
            let (graph, stats) = indexer::index(&root)?;
            let saved = graph.save(&root)?;
            let mut langs: Vec<String> = stats
                .by_lang
                .iter()
                .map(|(l, n)| format!("{l:?}: {n}"))
                .collect();
            langs.sort();
            eprintln!(
                "Indexed {} files ({}) → {} symbols, {} edges in {:.2?}",
                stats.files,
                langs.join(", "),
                stats.symbols,
                stats.edges,
                stats.elapsed
            );
            for e in &stats.errors {
                eprintln!("  warning: {e}");
            }
            eprintln!("Saved to {}", saved.display());
        }
        Command::Serve {
            path,
            fresh,
            no_refresh,
        } => {
            let root = canonical_root(&path)?;
            let graph = load_or_index(&root, fresh)?;
            mcp::Server::new(root, graph, !no_refresh).run()?;
        }
        Command::Tool { name, args, path } => {
            let root = canonical_root(&path)?;
            let graph = load_or_index(&root, false)?;
            let args: serde_json::Value =
                serde_json::from_str(&args).context("arguments must be a JSON object")?;
            let mut server = mcp::Server::new(root, graph, false);
            let result = server.call_tool(&name, &args)?;
            println!("{result}");
        }
        Command::Ast { file } => {
            let lang = Lang::from_path(&file).ok_or_else(|| anyhow!("unsupported file type"))?;
            let src = std::fs::read(&file)?;
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&lang.grammar())?;
            let tree = parser
                .parse(&src, None)
                .ok_or_else(|| anyhow!("parse failed"))?;
            println!("{}", tree.root_node().to_sexp());
        }
    }
    Ok(())
}

fn canonical_root(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))
}

/// Reuses the saved graph when it is up to date, otherwise indexes and saves.
fn load_or_index(root: &Path, force: bool) -> Result<Graph> {
    if !force {
        match Graph::load(root) {
            Ok(g) if !indexer::is_stale(root, &g) => return Ok(g),
            Ok(_) => eprintln!("codenav: index is stale, re-indexing…"),
            Err(_) => eprintln!("codenav: no index found, indexing {}…", root.display()),
        }
    }
    let (graph, stats) = indexer::index(root)?;
    eprintln!(
        "codenav: {} files, {} symbols, {} edges ({:.2?})",
        stats.files, stats.symbols, stats.edges, stats.elapsed
    );
    if let Err(e) = graph.save(root) {
        eprintln!("codenav: could not save index: {e:#}");
    }
    Ok(graph)
}
