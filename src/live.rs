//! A served graph kept up to date in the background: a worker thread looks for changed
//! sources (and a moved Git HEAD) every few seconds, re-indexes incrementally and swaps
//! the new graph in. Tool calls read the latest graph and never wait for a scan.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
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
    worker: Option<JoinHandle<()>>,
}

struct Shared {
    root: PathBuf,
    index_options: IndexOptions,
    /// The latest graph.
    graph: Mutex<Arc<Graph>>,
    /// Facts of every file, so a refresh only parses changed files. Held while
    /// re-indexing and saving: one re-index at a time.
    facts: Mutex<FactsCache>,
    reindexing: AtomicBool,
    /// Checks run only while the project is the active one.
    active: AtomicBool,
    stop: Mutex<bool>,
    wake: Condvar,
    /// Test hook: runs after a re-index read the files, before its graph is published.
    #[cfg(test)]
    after_read: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl Live {
    /// `watch`: start the background worker (it first parses every file into the cache).
    pub fn new(root: PathBuf, graph: Graph, index_options: IndexOptions, watch: bool) -> Live {
        let shared = Arc::new(Shared {
            root,
            index_options,
            graph: Mutex::new(Arc::new(graph)),
            facts: Mutex::new(FactsCache::default()),
            reindexing: AtomicBool::new(false),
            active: AtomicBool::new(true),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            #[cfg(test)]
            after_read: Mutex::new(None),
        });
        let worker = watch.then(|| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || shared.watch())
        });
        Live { shared, worker }
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

    /// Pauses the background checks while another project is active.
    pub fn set_active(&self, active: bool) {
        self.shared.active.store(active, Ordering::Relaxed);
    }

    /// Re-indexes now if sources changed or HEAD moved (waiting for a running re-index
    /// first); returns whether the graph changed.
    pub fn refresh(&self) -> Result<bool> {
        self.shared.refresh()
    }

    /// Re-indexes every file from scratch.
    pub fn reindex_full(&self) -> Result<IndexStats> {
        let mut facts = lock_facts(&self.shared.facts);
        *facts = FactsCache::default();
        self.shared.reindex(&mut facts, None)
    }

    #[cfg(test)]
    fn after_read(&self, hook: impl FnMut() + Send + 'static) {
        *lock(&self.shared.after_read) = Some(Box::new(hook));
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        *lock(&self.shared.stop) = true;
        self.shared.wake.notify_all();
        // Lets a running re-index finish its save.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Shared {
    fn watch(&self) {
        lock_facts(&self.facts).fill(&self.root);
        let mut interval = MIN_INTERVAL;
        loop {
            {
                let stop = lock(&self.stop);
                let (stop, _) = self
                    .wake
                    .wait_timeout_while(stop, interval, |stop| !*stop)
                    .unwrap_or_else(|e| e.into_inner());
                if *stop {
                    return;
                }
            }
            if !self.active.load(Ordering::Relaxed) {
                continue;
            }
            let started = Instant::now();
            if let Err(e) = self.refresh() {
                eprintln!("codenav: re-index failed: {e:#}");
            }
            interval = (started.elapsed() * SCAN_SHARE).clamp(MIN_INTERVAL, MAX_INTERVAL);
        }
    }

    fn refresh(&self) -> Result<bool> {
        let mut facts = lock_facts(&self.facts);
        let current = Arc::clone(&lock(&self.graph));
        let tree = indexer::scan(&self.root);
        if !indexer::is_stale_tree(&current, &tree) && !vcs::head_changed(&current, &self.root) {
            return Ok(false);
        }
        self.reindex(&mut facts, Some(tree))?;
        Ok(true)
    }

    /// Re-indexes from the cached facts (parsing changed files), publishes the graph and
    /// saves it. A file changed meanwhile is newer than the graph: the next check sees it.
    fn reindex(&self, facts: &mut FactsCache, tree: Option<indexer::Tree>) -> Result<IndexStats> {
        self.reindexing.store(true, Ordering::Relaxed);
        let result = indexer::index_cached(&self.root, &self.index_options, facts, tree);
        #[cfg(test)]
        if let Some(hook) = lock(&self.after_read).as_mut() {
            hook();
        }
        let result = result.map(|(graph, stats)| {
            let graph = Arc::new(graph);
            *lock(&self.graph) = Arc::clone(&graph);
            (graph, stats)
        });
        self.reindexing.store(false, Ordering::Relaxed);
        let (graph, stats) = result?;
        if let Err(e) = graph.save(&self.root) {
            eprintln!("codenav: could not save index: {e:#}");
        }
        Ok(stats)
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
    use std::fs;

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str, files: &[(&str, &str)]) -> TempRepo {
            let dir =
                std::env::temp_dir().join(format!("codenav-live-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            for (path, src) in files {
                let p = dir.join(path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, src).unwrap();
            }
            TempRepo(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

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
        let repo = TempRepo::new("refresh", &[("a.py", "def one():\n    pass\n")]);
        let live = live(&repo);
        assert!(!live.refresh().unwrap());
        std::thread::sleep(Duration::from_millis(10));
        fs::write(repo.0.join("a.py"), "def two():\n    pass\n").unwrap();
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
    fn a_file_changed_during_a_reindex_triggers_another() {
        let repo = TempRepo::new("during", &[("a.py", "def one():\n    pass\n")]);
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
        let repo = TempRepo::new("worker", &[("a.py", "def one():\n    pass\n")]);
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
