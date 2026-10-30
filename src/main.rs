mod communities;
mod cycles;
mod explore;
mod extract;
mod graph;
mod indexer;
mod insights;
mod lang;
mod mcp;
mod output;
mod query;
mod tests_for;
mod vcs;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use crate::graph::Graph;
use crate::indexer::IndexOptions;
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
    /// Build (or rebuild) the graph and save it to `<repo>/.codenav/graph.cnav`.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        opts: IndexArgs,
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
        #[command(flatten)]
        opts: IndexArgs,
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

#[derive(clap::Args)]
struct IndexArgs {
    /// Nodes whose dependency degree is above this percentile are hubs: their
    /// dependency edges are ignored when grouping code into communities (0-100;
    /// default: the saved index's value, else 99).
    #[arg(long, value_parser = parse_percentile)]
    hub_percentile: Option<f64>,
}

/// Explicit settings win; otherwise keep those of the saved index, so a later
/// re-index (by `serve`, `tool` or a plain `index`) doesn't change the communities.
fn index_options(root: &Path, requested: Option<f64>) -> IndexOptions {
    IndexOptions {
        hub_percentile: requested
            .or_else(|| Graph::saved_hub_percentile(root))
            .unwrap_or(communities::DEFAULT_HUB_PERCENTILE),
    }
}

fn parse_percentile(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if (0.0..=100.0).contains(&v) {
        Ok(v)
    } else {
        Err("must be between 0 and 100".into())
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Index { path, opts } => {
            let root = canonical_root(&path)?;
            let opts = index_options(&root, opts.hub_percentile);
            let (graph, stats) = indexer::index_with(&root, &opts)?;
            let saved = graph.save(&root)?;
            let mut langs: Vec<String> = stats
                .by_lang
                .iter()
                .map(|(l, n)| format!("{l:?}: {n}"))
                .collect();
            langs.sort();
            eprintln!(
                "Indexed {} files ({}) → {} symbols, {} edges, {} communities in {:.2?}",
                stats.files,
                langs.join(", "),
                stats.symbols,
                stats.edges,
                stats.communities,
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
            opts,
        } => {
            let root = canonical_root(&path)?;
            let (graph, opts) = load_or_index(&root, fresh, opts.hub_percentile)?;
            mcp::Server::new(root, graph, !no_refresh, opts).run()?;
        }
        Command::Tool { name, args, path } => {
            let root = canonical_root(&path)?;
            let (graph, opts) = load_or_index(&root, false, None)?;
            let args: serde_json::Value =
                serde_json::from_str(&args).context("arguments must be a JSON object")?;
            let mut server = mcp::Server::new(root, graph, false, opts);
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

/// Reuses the saved graph when it is up to date and was built with the requested
/// settings, otherwise indexes and saves. Returns the settings in effect.
fn load_or_index(
    root: &Path,
    force: bool,
    requested: Option<f64>,
) -> Result<(Graph, IndexOptions)> {
    let index_path = Graph::index_path(root);
    let opts = if force || !index_path.exists() {
        if !force {
            eprintln!("codenav: no index found, indexing {}…", root.display());
        }
        index_options(root, requested)
    } else {
        match Graph::load(root) {
            Ok(mut g) => {
                let opts = IndexOptions {
                    hub_percentile: requested.unwrap_or(g.hub_percentile),
                };
                if opts.hub_percentile != g.hub_percentile {
                    eprintln!(
                        "codenav: hub percentile changed ({} → {}), re-indexing…",
                        g.hub_percentile, opts.hub_percentile
                    );
                } else if indexer::is_stale(root, &g) {
                    eprintln!("codenav: index is stale, re-indexing…");
                } else {
                    // Same code, maybe a new commit: keep the graph, update its Git state.
                    if vcs::refresh_state(&mut g, root)
                        && let Err(e) = g.save(root)
                    {
                        eprintln!("codenav: could not save index: {e:#}");
                    }
                    return Ok((g, opts));
                }
                opts
            }
            Err(e) => {
                eprintln!("codenav: cannot use the saved index ({e:#}), re-indexing…");
                index_options(root, requested)
            }
        }
    };
    let (graph, stats) = indexer::index_with(root, &opts)?;
    eprintln!(
        "codenav: {} files, {} symbols, {} edges, {} communities ({:.2?})",
        stats.files, stats.symbols, stats.edges, stats.communities, stats.elapsed
    );
    if let Err(e) = graph.save(root) {
        eprintln!("codenav: could not save index: {e:#}");
    }
    Ok((graph, opts))
}
