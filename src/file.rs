//! Whole-file access and the parallel scheduler.

use crate::chunk::{Chunk, ChunkInfo, scan_chunks};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::mmap::{Advice, Mapping};
use crate::schema::{Schema, SchemaCache};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// An opened recording: the memory-mapped bytes plus the chunk table.
///
/// Opening reads only the 68-byte header of each chunk, so it takes
/// microseconds however large the file is.
pub struct JfrFile {
    map: Mapping,
    chunks: Vec<ChunkInfo>,
    trailing: u64,
    cache: SchemaCache,
}

/// What to do when a chunk cannot be loaded during a parallel run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnChunkError {
    /// Stop and return the error.
    Fail,
    /// Skip the chunk and report it in [`ParResult::skipped`].
    Skip,
}

/// Tuning knobs for parallel processing.
#[derive(Debug, Clone)]
pub struct ParallelOptions {
    /// Worker threads. Defaults to the number of available cores.
    pub threads: usize,
    /// Chunks larger than this are split into event ranges of about this size
    /// so a single huge chunk still uses every core.
    pub target_unit_bytes: usize,
    /// How many chunks ahead of the one being claimed to request read-ahead for.
    pub lookahead_chunks: usize,
    /// Release a chunk's pages from the process once it is finished,
    /// keeping resident memory flat on very large files.
    pub drop_behind: bool,
    /// Policy for chunks that fail to load.
    pub on_chunk_error: OnChunkError,
}

impl Default for ParallelOptions {
    fn default() -> Self {
        ParallelOptions {
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            target_unit_bytes: 8 << 20,
            lookahead_chunks: 2,
            drop_behind: true,
            on_chunk_error: OnChunkError::Fail,
        }
    }
}

impl ParallelOptions {
    /// Sets the number of worker threads (at least one).
    pub fn threads(mut self, n: usize) -> Self {
        self.threads = n.max(1);
        self
    }
    /// Sets the size above which a chunk is split into event ranges.
    pub fn target_unit_bytes(mut self, n: usize) -> Self {
        self.target_unit_bytes = n.max(1);
        self
    }
    /// Sets how many chunks ahead read-ahead is requested for.
    pub fn lookahead_chunks(mut self, n: usize) -> Self {
        self.lookahead_chunks = n;
        self
    }
    /// Enables or disables releasing finished chunks from memory.
    pub fn drop_behind(mut self, on: bool) -> Self {
        self.drop_behind = on;
        self
    }
    /// Chooses what happens when a chunk cannot be loaded.
    pub fn on_chunk_error(mut self, p: OnChunkError) -> Self {
        self.on_chunk_error = p;
        self
    }
}

/// Outcome of a parallel run.
#[derive(Debug)]
pub struct ParResult<A> {
    /// The merged result, or `None` if there was nothing to process.
    pub value: Option<A>,
    /// Chunks skipped under [`OnChunkError::Skip`], with the reason.
    pub skipped: Vec<(usize, Error)>,
}

impl JfrFile {
    /// Memory-maps `path` and indexes its chunks.
    ///
    /// The file must not be truncated or rewritten while it is open.
    pub fn open(path: impl AsRef<Path>) -> Result<JfrFile> {
        Self::from_mapping(Mapping::open(path)?)
    }

    /// Reads a recording that is already in memory.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<JfrFile> {
        Self::from_mapping(Mapping::from_vec(bytes))
    }

    fn from_mapping(map: Mapping) -> Result<JfrFile> {
        let (chunks, trailing) = scan_chunks(&map)?;
        Ok(JfrFile { map, chunks, trailing, cache: SchemaCache::new() })
    }

    /// Size of the file in bytes.
    pub fn len(&self) -> u64 {
        self.map.len() as u64
    }

    /// True if the file has no bytes.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The chunk table.
    pub fn chunks(&self) -> &[ChunkInfo] {
        &self.chunks
    }

    /// Bytes after the last valid chunk that could not be parsed as a chunk header.
    pub fn trailing_bytes(&self) -> u64 {
        self.trailing
    }

    /// The raw bytes of the whole file.
    pub fn bytes(&self) -> &[u8] {
        &self.map
    }

    /// Loads chunk `index`: compiles its schema and indexes its constant pools.
    pub fn chunk(&self, index: usize) -> Result<Chunk<'_>> {
        let info = self
            .chunks
            .get(index)
            .ok_or_else(|| Error::Query(format!("chunk {index} out of range (file has {})", self.chunks.len())))?;
        Chunk::load(&self.map, info, &self.cache)
    }

    /// Distinct schemas used by the file, in order of first appearance.
    /// Reads only the metadata event of each chunk.
    pub fn schemas(&self) -> Result<Vec<Arc<Schema>>> {
        let mut out: Vec<Arc<Schema>> = Vec::new();
        for info in &self.chunks {
            let s = Chunk::load_schema(&self.map, info, &self.cache)?;
            if !out.iter().any(|o| Arc::ptr_eq(o, &s)) {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Names of all event types defined by any chunk, sorted.
    pub fn event_names(&self) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        for s in self.schemas()? {
            for t in s.meta.events() {
                names.insert(t.name.to_string());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Calls `f` for every event of the named types (all events if `names`
    /// is empty), chunk by chunk on the calling thread. Names that no chunk
    /// defines are an error; names a particular chunk lacks match nothing there.
    pub fn for_each_event<F>(&self, names: &[&str], mut f: F) -> Result<()>
    where
        F: for<'e> FnMut(Event<'e>) -> Result<()>,
    {
        self.check_names(names)?;
        for i in 0..self.chunks.len() {
            let chunk = self.chunk(i)?;
            let (s, e) = chunk.event_range();
            for ev in chunk.events_matching(s, e, names) {
                f(ev?)?;
            }
            self.map.advise(chunk.info.start, chunk.info.end, Advice::DontNeed);
        }
        Ok(())
    }

    fn check_names(&self, names: &[&str]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let known = self.event_names()?;
        match names.iter().find(|n| known.binary_search_by(|k| k.as_str().cmp(n)).is_err()) {
            Some(n) => Err(Error::NoSuchType((*n).to_string())),
            None => Ok(()),
        }
    }

    /// Runs `unit` over the event ranges of every chunk on a pool of threads
    /// and merges the results in file order.
    ///
    /// Chunks are loaded when a worker needs them and released as soon as
    /// their last range is done, so memory use stays proportional to the
    /// number of threads rather than the file size. `merge` must be associative.
    pub fn par_units<A, F, M>(&self, opts: &ParallelOptions, unit: F, merge: M) -> Result<ParResult<A>>
    where
        A: Send,
        F: for<'c> Fn(&'c Chunk<'c>, usize, usize) -> Result<A> + Sync,
        M: Fn(A, A) -> A + Sync,
    {
        let n = self.chunks.len();
        let sched = Scheduler::<A> {
            file: self,
            opts,
            unit: &unit,
            merge: &merge,
            state: Mutex::new(State {
                next_chunk: 0,
                loading: 0,
                active: Vec::new(),
                slots: (0..n).map(|_| None).collect(),
                merged_upto: 0,
                merging: false,
                acc: None,
                skipped: Vec::new(),
                error: None,
            }),
            cv: Condvar::new(),
        };
        let threads = opts.threads.clamp(1, 1024);
        std::thread::scope(|s| {
            for _ in 1..threads {
                s.spawn(|| sched.worker());
            }
            sched.worker();
        });
        let st = sched.state.into_inner().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = st.error {
            return Err(e);
        }
        Ok(ParResult { value: st.acc, skipped: st.skipped })
    }

    /// Folds every event of the named types (all if `names` is empty) into an
    /// accumulator in parallel.
    ///
    /// Each worker folds its event ranges into a fresh `init()` value and the
    /// partial values are combined with `merge` in file order.
    pub fn par_fold<A, I, F, M>(
        &self,
        opts: &ParallelOptions,
        names: &[&str],
        init: I,
        fold: F,
        merge: M,
    ) -> Result<ParResult<A>>
    where
        A: Send,
        I: Fn() -> A + Sync,
        F: for<'e> Fn(&mut A, Event<'e>) -> Result<()> + Sync,
        M: Fn(A, A) -> A + Sync,
    {
        self.check_names(names)?;
        self.par_units(
            opts,
            |chunk, start, end| {
                let mut acc = init();
                for ev in chunk.events_matching(start, end, names) {
                    fold(&mut acc, ev?)?;
                }
                Ok(acc)
            },
            merge,
        )
    }
}

struct Job<'a, A> {
    chunk: Chunk<'a>,
    ranges: Vec<(usize, usize)>,
    next: AtomicUsize,
    remaining: AtomicUsize,
    results: Mutex<Vec<Option<A>>>,
}

struct State<'a, A> {
    next_chunk: usize,
    loading: usize,
    active: Vec<Arc<Job<'a, A>>>,
    slots: Vec<Option<Option<A>>>,
    merged_upto: usize,
    merging: bool,
    acc: Option<A>,
    skipped: Vec<(usize, Error)>,
    error: Option<Error>,
}

enum Work<'a, A> {
    Unit(Arc<Job<'a, A>>, usize),
    Load(usize),
    Done,
}

struct Scheduler<'s, A> {
    file: &'s JfrFile,
    opts: &'s ParallelOptions,
    unit: &'s (dyn for<'c> Fn(&'c Chunk<'c>, usize, usize) -> Result<A> + Sync),
    merge: &'s (dyn Fn(A, A) -> A + Sync),
    state: Mutex<State<'s, A>>,
    cv: Condvar,
}

impl<'s, A: Send> Scheduler<'s, A> {
    fn lock(&self) -> MutexGuard<'_, State<'s, A>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_work(&self) -> Work<'s, A> {
        let mut g = self.lock();
        loop {
            if g.error.is_some() {
                return Work::Done;
            }
            for job in &g.active {
                if job.next.load(Ordering::Relaxed) < job.ranges.len() {
                    let i = job.next.fetch_add(1, Ordering::Relaxed);
                    if i < job.ranges.len() {
                        return Work::Unit(job.clone(), i);
                    }
                }
            }
            if g.next_chunk < self.file.chunks.len() {
                let ci = g.next_chunk;
                g.next_chunk += 1;
                g.loading += 1;
                return Work::Load(ci);
            }
            if g.loading == 0 {
                return Work::Done;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn worker(&self) {
        loop {
            match self.next_work() {
                Work::Done => {
                    self.cv.notify_all();
                    return;
                }
                Work::Load(ci) => self.load(ci),
                Work::Unit(job, i) => self.run_unit(job, i),
            }
        }
    }

    fn load(&self, ci: usize) {
        let file = self.file;
        for k in 1..=self.opts.lookahead_chunks {
            if let Some(next) = file.chunks.get(ci + k) {
                file.map.advise(next.start, next.end, Advice::WillNeed);
            }
        }
        let loaded = self.prepare(ci);
        let mut g = self.lock();
        g.loading -= 1;
        match loaded {
            Ok(job) if job.ranges.is_empty() => g.slots[ci] = Some(None),
            Ok(job) => g.active.push(Arc::new(job)),
            Err(e) => match self.opts.on_chunk_error {
                OnChunkError::Skip => {
                    g.skipped.push((ci, e));
                    g.slots[ci] = Some(None);
                }
                OnChunkError::Fail => {
                    g.error.get_or_insert(e);
                }
            },
        }
        self.advance(g);
        self.cv.notify_all();
    }

    /// Folds finished chunks into the running result, in file order, as soon
    /// as the chunks before them are done. Only one thread merges at a time and
    /// it does so without holding the lock.
    fn advance<'a>(&'a self, mut g: MutexGuard<'a, State<'s, A>>) {
        if g.merging {
            return;
        }
        g.merging = true;
        loop {
            let mut batch = Vec::new();
            loop {
                let i = g.merged_upto;
                let Some(done) = g.slots.get_mut(i).and_then(Option::take) else { break };
                batch.push(done);
                g.merged_upto += 1;
            }
            if batch.is_empty() {
                g.merging = false;
                return;
            }
            let mut acc = g.acc.take();
            drop(g);
            for part in batch.into_iter().flatten() {
                acc = Some(match acc {
                    None => part,
                    Some(a) => (self.merge)(a, part),
                });
            }
            g = self.lock();
            g.acc = acc;
        }
    }

    fn prepare(&self, ci: usize) -> Result<Job<'s, A>> {
        let chunk = self.file.chunk(ci)?;
        let (start, end) = chunk.event_range();
        let size = end.saturating_sub(start);
        let ranges = if size == 0 {
            Vec::new()
        } else if size <= self.opts.target_unit_bytes {
            vec![(start, end)]
        } else {
            let parts = size.div_ceil(self.opts.target_unit_bytes).min(self.opts.threads.max(1) * 8);
            chunk.split_events(parts)?
        };
        let n = ranges.len();
        Ok(Job {
            chunk,
            ranges,
            next: AtomicUsize::new(0),
            remaining: AtomicUsize::new(n),
            results: Mutex::new((0..n).map(|_| None).collect()),
        })
    }

    fn run_unit(&self, job: Arc<Job<'s, A>>, i: usize) {
        let (s, e) = job.ranges[i];
        let result = (self.unit)(&job.chunk, s, e);
        if self.opts.drop_behind && job.ranges.len() > 1 {
            let base = job.chunk.info.start;
            self.file.map.advise(base + s as u64, base + e as u64, Advice::DontNeed);
        }
        match result {
            Ok(r) => {
                job.results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            }
            Err(err) => {
                self.lock().error.get_or_insert(err);
                self.cv.notify_all();
                return;
            }
        }
        if job.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let parts = std::mem::take(&mut *job.results.lock().unwrap_or_else(|e| e.into_inner()));
            let mut merged: Option<A> = None;
            for r in parts.into_iter().flatten() {
                merged = Some(match merged {
                    None => r,
                    Some(acc) => (self.merge)(acc, r),
                });
            }
            let info = &job.chunk.info;
            if self.opts.drop_behind {
                self.file.map.advise(info.start, info.end, Advice::DontNeed);
            }
            let mut g = self.lock();
            g.slots[info.index] = Some(merged);
            g.active.retain(|j| !Arc::ptr_eq(j, &job));
            self.advance(g);
            self.cv.notify_all();
        }
    }
}
