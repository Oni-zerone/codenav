mod communities;
mod cycles;
mod diff_impact;
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
        /// Repositories to serve; the first is the default for tools called without
        /// `project`, the others are loaded on first use.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
        /// Let tools open any repository by path (`project`), not only the ones served.
        #[arg(long)]
        any_project: bool,
        /// Graphs kept in memory at once; the least recently used are dropped (reloaded
        /// from their index when needed).
        #[arg(long, default_value_t = mcp::DEFAULT_MAX_LOADED as u16,
              value_parser = clap::value_parser!(u16).range(1..))]
        max_loaded: u16,
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
        /// Tool arguments as a JSON object (default `{}`). A single argument that does not
        /// start with `{` is taken as PATH: `codenav tool graph_overview ../repo`.
        args: Option<String>,
        /// Repository (default: current directory), as for `index` and `serve`.
        #[arg(conflicts_with = "path_flag")]
        path: Option<PathBuf>,
        /// Same as the positional PATH (kept for compatibility).
        #[arg(long = "path", value_name = "PATH", id = "path_flag")]
        path_flag: Option<PathBuf>,
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
            let opts = indexer::index_options(&root, opts.hub_percentile);
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
            paths,
            any_project,
            max_loaded,
            fresh,
            no_refresh,
            opts,
        } => {
            let roots: Vec<PathBuf> = paths
                .iter()
                .map(|p| canonical_root(p))
                .collect::<Result<_>>()?;
            let (graph, opts) = indexer::load_or_index(&roots[0], fresh, opts.hub_percentile)?;
            let mut server = mcp::Server::new(roots[0].clone(), graph, !no_refresh, opts);
            for root in &roots[1..] {
                server.add_project(root.clone());
            }
            server.allow_any_project(any_project);
            server.set_max_loaded(usize::from(max_loaded));
            server.run()?;
        }
        Command::Tool {
            name,
            args,
            path,
            path_flag,
        } => {
            let (args, path) = tool_args_and_path(args, path.or(path_flag));
            // Validate before indexing: a wrong argument must not index anything.
            let args: serde_json::Value = serde_json::from_str(&args)
                .ok()
                .filter(serde_json::Value::is_object)
                .ok_or_else(|| anyhow!("arguments must be a JSON object, got `{args}`"))?;
            let root = canonical_root(&path)?;
            let (graph, opts) = indexer::load_or_index(&root, false, None)?;
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

/// `codenav tool NAME [ARGS] [PATH]`: with one extra argument, JSON (starting with `{`)
/// is ARGS and anything else is PATH, so `tool NAME PATH` works like `index PATH`.
fn tool_args_and_path(args: Option<String>, path: Option<PathBuf>) -> (String, PathBuf) {
    match (args, path) {
        (Some(a), None) if !a.trim_start().starts_with('{') => ("{}".into(), PathBuf::from(a)),
        (a, p) => (
            a.unwrap_or_else(|| "{}".into()),
            p.unwrap_or_else(|| PathBuf::from(".")),
        ),
    }
}

fn canonical_root(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))
}
