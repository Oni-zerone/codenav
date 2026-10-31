//! A served graph kept up to date in the background: a worker thread looks for changed
//! sources (and a moved Git HEAD) every few seconds while its project is active,
//! re-indexes incrementally and swaps the new graph in. Tool calls read the latest graph
//! and never wait for a scan.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::graph::Graph;
use crate::indexer::{self, FactsCache, IndexOptions, IndexStats};
use crate::vcs;

/// Shortest pause between two checks; longer on repositories whose scan is slow, so
/// that checking never takes more than a few percent of a core.
const MIN_INTERVAL: Duration = Duration::from_secs(2);
const MAX_INTERVAL: Duration = Duration::from_secs(30);
const SCAN_SHARE: u32 = 50;

pub struct Live {
    shared: Arc<Shared>,
}

struct Shared {
    root: PathBuf,
    index_options: IndexOptions,
    /// The latest graph.
    graph: Mutex<Arc<Graph>>,
    /// Facts of every file, so a refresh only parses changed files. Held while
    /// checking and re-indexing: one at a time.
    facts: Mutex<FactsCache>,
    /// Held while writing the index to disk.
    saving: Mutex<()>,
    reindexing: AtomicBool,
    /// Checks run only while the project is the active one.
    active: AtomicBool,
    signal: Mutex<Signal>,
    wake: Condvar,
    /// Test hook: runs after a re-index read the files, before its graph is published.
    #[cfg(test)]
    after_read: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

#[derive(Default)]
struct Signal {
    stop: bool,
    /// Check now rather than at the end of the pause.
    poke: bool,
}

impl Live {
    /// `watch`: keep the graph up to date with a background worker (it first parses
    /// every file into the cache, once the project is active).
    pub fn new(root: PathBuf, graph: Graph, index_options: IndexOptions, watch: bool) -> Live {
        let shared = Arc::new(Shared {
            root,
            index_options,
            graph: Mutex::new(Arc::new(graph)),
            facts: Mutex::new(FactsCache::default()),
            saving: Mutex::new(()),
            reindexing: AtomicBool::new(false),
            active: AtomicBool::new(true),
            signal: Mutex::new(Signal::default()),
            wake: Condvar::new(),
            #[cfg(test)]
            after_read: Mutex::new(None),
        });
        if watch {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.watch());
        }
        Live { shared }
    }

    pub fn root(&self) -> &Path {
        &self.shared.root
    }

    /// The latest graph.
    pub fn graph(&self) -> Arc<Graph> {
        Arc::clone(&lock(&self.shared.graph))
    }

    /// Whether a re-index is running now.
    pub fn reindexing(&self) -> bool {
        self.shared.reindexing.load(Ordering::Relaxed)
    }

    /// Pauses the background checks while another project is active; on activation,
    /// checks right away.
    pub fn set_active(&self, active: bool) {
        self.shared.active.store(active, Ordering::Relaxed);
        if active {
            lock(&self.shared.signal).poke = true;
            self.shared.wake.notify_all();
        }
    }

    /// Updates the graph now if sources changed or HEAD moved (after a check already
    /// running); returns whether it changed.
    pub fn refresh(&self) -> Result<bool> {
        Ok(self.shared.update()?.0)
    }

    /// Re-indexes every file from scratch.
    pub fn reindex_full(&self) -> Result<IndexStats> {
        let mut facts = lock_facts(&self.shared.facts);
        *facts = FactsCache::default();
        let (graph, stats) = self.shared.reindex(&mut facts, None)?;
        drop(facts);
        self.shared.save(&graph);
        Ok(stats)
    }

    #[cfg(test)]
    fn after_read(&self, hook: impl FnMut() + Send + 'static) {
        *lock(&self.shared.after_read) = Some(Box::new(hook));
    }
}

impl Drop for Live {
    /// Stops the worker without waiting: it may be parsing a whole repository. A
    /// re-index it is running still completes and saves.
    fn drop(&mut self) {
        lock(&self.shared.signal).stop = true;
        self.shared.wake.notify_all();
    }
}

impl Shared {
    fn watch(&self) {
        let mut filled = false;
        let mut interval = MIN_INTERVAL;
        loop {
            if self.active.load(Ordering::Relaxed) {
                let run = catch_unwind(AssertUnwindSafe(|| {
                    if !filled {
                        lock_facts(&self.facts).fill(&self.root);
                        filled = true;
                    }
                    self.update()
                }));
                match run {
                    Ok(Ok((_, scan))) => {
                        interval = (scan * SCAN_SHARE).clamp(MIN_INTERVAL, MAX_INTERVAL);
                    }
                    Ok(Err(e)) => eprintln!("codenav: re-index failed: {e:#}"),
                    Err(_) => eprintln!("codenav: re-index panicked; retrying later"),
                }
            }
            let signal = lock(&self.signal);
            let mut signal = self
                .wake
                .wait_timeout_while(signal, interval, |s| !s.stop && !s.poke)
                .map_or_else(|e| e.into_inner().0, |(s, _)| s);
            if signal.stop {
                return;
            }
            signal.poke = false;
        }
    }

    /// Re-indexes if sources changed, or updates the Git state if only HEAD moved.
    /// Returns whether the graph changed, and how long the scan took.
    fn update(&self) -> Result<(bool, Duration)> {
        let mut facts = lock_facts(&self.facts);
        let started = Instant::now();
        let current = Arc::clone(&lock(&self.graph));
        let tree = indexer::scan(&self.root);
        let scan = started.elapsed();
        let graph = if indexer::is_stale_tree(&current, &tree) {
            self.reindex(&mut facts, Some(tree))?.0
        } else if vcs::head_changed(&current, &self.root) {
            // Same code, new commit: only the Git fields change.
            let mut graph = Graph::clone(&current);
            vcs::record_state(&mut graph, &self.root);
            self.publish(graph)
        } else {
            return Ok((false, scan));
        };
        drop(facts);
        self.save(&graph);
        Ok((true, scan))
    }

    /// Re-indexes from the cached facts (parsing changed files) and publishes the graph.
    /// A file changed meanwhile is newer than the graph: the next check sees it.
    fn reindex(
        &self,
        facts: &mut FactsCache,
        tree: Option<indexer::Tree>,
    ) -> Result<(Arc<Graph>, IndexStats)> {
        struct Reindexing<'a>(&'a AtomicBool);
        impl Drop for Reindexing<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        self.reindexing.store(true, Ordering::Relaxed);
        let _reindexing = Reindexing(&self.reindexing);
        let (graph, stats) = indexer::index_cached(&self.root, &self.index_options, facts, tree)?;
        #[cfg(test)]
        if let Some(hook) = lock(&self.after_read).as_mut() {
            hook();
        }
        Ok((self.publish(graph), stats))
    }

    fn publish(&self, graph: Graph) -> Arc<Graph> {
        let graph = Arc::new(graph);
        *lock(&self.graph) = Arc::clone(&graph);
        graph
    }

    /// Writes `graph` to disk unless a newer one was published meanwhile (it is saved
    /// in its turn).
    fn save(&self, graph: &Arc<Graph>) {
        let _saving = lock(&self.saving);
        if !Arc::ptr_eq(graph, &lock(&self.graph)) {
            return;
        }
        if let Err(e) = graph.save(&self.root) {
            eprintln!("codenav: could not save index: {e:#}");
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The facts cache, emptied if a panic left it poisoned (it may be partly updated).
fn lock_facts(facts: &Mutex<FactsCache>) -> MutexGuard<'_, FactsCache> {
    facts.lock().unwrap_or_else(|poisoned| {
        facts.clear_poison();
        let mut cache = poisoned.into_inner();
        *cache = FactsCache::default();
        cache
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::tests::TempRepo;
    use std::fs;

    fn live(repo: &TempRepo) -> Live {
        // Written in the millisecond the index starts, a file would count as changed.
        std::thread::sleep(Duration::from_millis(5));
        let opts = IndexOptions::default();
        let (graph, _) = indexer::index_with(&repo.0, &opts).unwrap();
        Live::new(repo.0.clone(), graph, opts, false)
    }

    fn has(live: &Live, name: &str) -> bool {
        live.graph().nodes.iter().any(|n| n.name == name)
    }

    #[test]
    fn refresh_reindexes_only_when_sources_changed() {
        let repo = TempRepo::new("live-refresh", &[("a.py", "def one():\n    pass\n")]);
        let live = live(&repo);
        assert!(!live.refresh().unwrap());
        std::thread::sleep(Duration::from_millis(10));
        fs::write(repo.0.join("a.py"), "def two():\n    pass\n").unwrap();
        // An edit in the millisecond a re-index starts makes the next check re-index too.
        std::thread::sleep(Duration::from_millis(5));
        assert!(live.refresh().unwrap());
        assert!(has(&live, "two") && !has(&live, "one"));
        assert!(!live.refresh().unwrap());
        // Saved too: a later load gets the new graph.
        assert!(
            Graph::load(&repo.0)
                .unwrap()
                .nodes
                .iter()
                .any(|n| n.name == "two")
        );
    }

    #[test]
    fn a_new_commit_only_updates_the_git_state() {
        let repo = TempRepo::new("live-commit", &[("a.py", "def one():\n    pass\n")]);
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo.0)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !git(&["init", "-q"]) || !git(&["add", "."]) || !git(&["commit", "-qm", "one"]) {
            return; // no git here
        }
        let live = live(&repo);
        let before = live.graph();
        assert!(!live.refresh().unwrap());
        assert!(git(&["commit", "-q", "--allow-empty", "-m", "two"]));
        assert!(live.refresh().unwrap());
        let after = live.graph();
        assert_ne!(after.git_commit, before.git_commit);
        assert_eq!(after.indexed_at_ms, before.indexed_at_ms, "not re-indexed");
        assert!(!live.refresh().unwrap());
    }

    #[test]
    fn a_file_changed_during_a_reindex_triggers_another() {
        let repo = TempRepo::new("live-during", &[("a.py", "def one():\n    pass\n")]);
        let live = live(&repo);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(repo.0.join("a.py"), "def two():\n    pass\n").unwrap();
        // Edited again after the re-index read the files, before it finished.
        let path = repo.0.join("a.py");
        let mut edits = 0;
        live.after_read(move || {
            if edits == 0 {
                std::thread::sleep(Duration::from_millis(10));
                fs::write(&path, "def three():\n    pass\n").unwrap();
            }
            edits += 1;
        });
        assert!(live.refresh().unwrap());
        assert!(has(&live, "two"));
        assert!(live.refresh().unwrap());
        assert!(has(&live, "three") && !has(&live, "two"));
        assert!(!live.refresh().unwrap());
    }

    #[test]
    fn the_worker_picks_up_changes_by_itself() {
        let repo = TempRepo::new("live-worker", &[("a.py", "def one():\n    pass\n")]);
        let opts = IndexOptions::default();
        let (graph, _) = indexer::index_with(&repo.0, &opts).unwrap();
        let live = Live::new(repo.0.clone(), graph, opts, true);
        std::thread::sleep(Duration::from_millis(10));
        fs::write(repo.0.join("a.py"), "def two():\n    pass\n").unwrap();
        let started = Instant::now();
        while !has(&live, "two") {
            assert!(started.elapsed() < Duration::from_secs(10), "not refreshed");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
