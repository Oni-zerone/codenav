//! codenav as a library. The graph and its file format (`graph`, with the types it
//! stores) are always built, also for WebAssembly; the `indexer` feature adds everything
//! that builds graphs from sources and serves them.

pub mod communities;
pub mod graph;
pub mod lang;
pub mod packages;

#[cfg(feature = "indexer")]
pub mod cycles;
#[cfg(feature = "indexer")]
pub mod diff_impact;
#[cfg(feature = "indexer")]
pub mod embeddings;
#[cfg(feature = "indexer")]
pub mod explore;
#[cfg(feature = "indexer")]
pub mod extract;
#[cfg(feature = "indexer")]
pub mod indexer;
#[cfg(feature = "indexer")]
pub mod insights;
#[cfg(feature = "indexer")]
pub mod live;
#[cfg(feature = "indexer")]
pub mod mcp;
#[cfg(feature = "indexer")]
pub mod output;
#[cfg(feature = "indexer")]
pub mod query;
#[cfg(feature = "indexer")]
pub mod tests_for;
#[cfg(feature = "indexer")]
pub mod vcs;
