use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};

use codenav::lang::Lang;
use codenav::{embeddings, graph::Graph, indexer, mcp};

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
        /// from their index when needed). While switching to an unloaded repository, the
        /// previously active graph is held until the new one is loaded.
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
        /// Static word embeddings (word2vec/fastText/GloVe text format) to let explore
        /// match synonyms of the question's words in names and docs.
        #[arg(long, value_name = "FILE")]
        embeddings: Option<PathBuf>,
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
        /// Static word embeddings for explore (as for `serve`).
        #[arg(long, value_name = "FILE")]
        embeddings: Option<PathBuf>,
        /// Use the saved index without checking for changed sources (faster, for callers
        /// that know it is current); also set by the CODENAV_NO_REFRESH variable.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Print a saved graph (`.codenav/graph.cnav` of PATH, or a .cnav file) as JSON.
    Export {
        #[arg(default_value = ".")]
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
            embeddings,
        } => {
            let embeddings = load_embeddings(embeddings.as_deref())?;
            let roots: Vec<PathBuf> = paths
                .iter()
                .map(|p| canonical_root(p))
                .collect::<Result<_>>()?;
            let load = mcp::LoadOptions {
                fresh,
                hub_percentile: opts.hub_percentile,
            };
            let (graph, opts) =
                indexer::load_or_index(&roots[0], fresh, opts.hub_percentile, true)?;
            let mut server = mcp::Server::new(roots[0].clone(), graph, !no_refresh, opts);
            server.set_load_options(load);
            for root in &roots[1..] {
                server.add_project(root.clone());
            }
            server.allow_any_project(any_project);
            server.set_max_loaded(usize::from(max_loaded));
            server.set_embeddings(embeddings);
            server.run()?;
        }
        Command::Tool {
            name,
            args,
            path,
            path_flag,
            embeddings,
            no_refresh,
        } => {
            let (args, path) = tool_args_and_path(args, path.or(path_flag));
            // Validate before indexing: a wrong argument must not index anything.
            let args: serde_json::Value = serde_json::from_str(&args)
                .ok()
                .filter(serde_json::Value::is_object)
                .ok_or_else(|| anyhow!("arguments must be a JSON object, got `{args}`"))?;
            let root = canonical_root(&path)?;
            let no_refresh = no_refresh || std::env::var_os("CODENAV_NO_REFRESH").is_some();
            let (graph, opts) = indexer::load_or_index(&root, false, None, !no_refresh)?;
            let mut server = mcp::Server::new(root, graph, false, opts);
            server.set_source_check(!no_refresh);
            // Only explore uses them, and only after the arguments are known to be valid.
            if name == "explore" {
                server.set_embeddings(load_embeddings(embeddings.as_deref())?);
            }
            let result = server.call_tool(&name, &args)?;
            println!("{result}");
        }
        Command::Export { path } => {
            let file = if path.is_dir() {
                Graph::index_path(&path)
            } else {
                path
            };
            let data =
                std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
            let graph = Graph::from_bytes(&data)?;
            let mut out = std::io::stdout().lock();
            // A reader that stops early (`| head`) is not an error.
            match serde_json::to_writer(&mut out, &graph) {
                Err(e) if e.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe) => {}
                written => {
                    written?;
                    use std::io::Write;
                    let _ = writeln!(out);
                }
            }
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

fn load_embeddings(path: Option<&Path>) -> Result<Option<embeddings::Embeddings>> {
    let Some(path) = path else { return Ok(None) };
    let e = embeddings::Embeddings::load(path)?;
    eprintln!(
        "codenav: {} word vectors ({} dims) for synonyms",
        e.len(),
        e.dim()
    );
    Ok(Some(e))
}

fn canonical_root(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))
}
