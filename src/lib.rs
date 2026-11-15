//! A fast, low-memory, schema-driven reader for Java Flight Recorder (JFR) files.
//!
//! The crate has no dependencies and no hard-coded event layouts. Everything
//! is decoded from the metadata stored in the recording, so JDK events,
//! async-profiler events and your own custom events all work the same way.
//!
//! # Quick start
//!
//! ```no_run
//! use jfr_parser::JfrFile;
//!
//! # fn main() -> jfr_parser::Result<()> {
//! let file = JfrFile::open("recording.jfr")?;
//!
//! file.for_each_event(&["jdk.GarbageCollection"], |event| {
//!     let name = event.get("name")?;
//!     let pause = event.duration_nanos()?.unwrap_or(0);
//!     println!("{name}: {} ms", pause / 1_000_000);
//!     Ok(())
//! })?;
//! # Ok(()) }
//! ```
//!
//! # Querying
//!
//! A SQL-like language runs in parallel over the whole file. See the
//! [`query`] module for the grammar and functions.
//!
//! ```no_run
//! use jfr_parser::{JfrFile, ParallelOptions};
//!
//! # fn main() -> jfr_parser::Result<()> {
//! let file = JfrFile::open("recording.jfr")?;
//! let top = file.query(
//!     "SELECT topframe(stackTrace) AS frame, count(*) AS samples
//!      FROM jdk.ExecutionSample
//!      GROUP BY frame ORDER BY samples DESC LIMIT 10",
//!     &ParallelOptions::default(),
//! )?;
//! println!("{top}");
//! # Ok(()) }
//! ```
//!
//! # Parallel processing
//!
//! [`JfrFile::par_fold`] runs a fold over every matching event on a thread
//! pool and merges the partial results in file order. Chunks are loaded when
//! a worker needs them and dropped when finished, and a single huge chunk is
//! split into event ranges, so all cores stay busy and memory stays flat.
//!
//! ```no_run
//! use jfr_parser::{JfrFile, ParallelOptions};
//! use std::collections::HashMap;
//!
//! # fn main() -> jfr_parser::Result<()> {
//! let file = JfrFile::open("recording.jfr")?;
//! let counts = file.par_fold(
//!     &ParallelOptions::default(),
//!     &[],
//!     HashMap::<String, u64>::new,
//!     |acc, event| {
//!         *acc.entry(event.name().to_string()).or_default() += 1;
//!         Ok(())
//!     },
//!     |mut a, b| {
//!         for (k, v) in b {
//!             *a.entry(k).or_default() += v;
//!         }
//!         a
//!     },
//! )?;
//! # let _ = counts;
//! # Ok(()) }
//! ```
//!
//! # Reading values
//!
//! [`Event::get`] and [`Obj::path`](value::Obj::path) return a [`Value`](value::Value).
//! Constant-pool references (threads, classes, stack traces, methods) are
//! followed automatically, and a dotted path walks through them:
//!
//! ```no_run
//! # use jfr_parser::JfrFile;
//! # fn main() -> jfr_parser::Result<()> {
//! # let file = JfrFile::open("recording.jfr")?;
//! # file.for_each_event(&["jdk.ExecutionSample"], |event| {
//! let thread = event.path("sampledThread.javaName")?;
//! let state = event.path("state")?;
//! if let Some(trace) = event.stack_trace()?.as_obj() {
//!     if let Some(frames) = trace.get("frames")?.as_array() {
//!         for frame in frames.iter() {
//!             let frame = frame?;
//!             let method = frame.as_obj().unwrap().path("method.name")?;
//!             println!("{thread} {state} {method}");
//!         }
//!     }
//! }
//! # Ok(()) })?;
//! # Ok(()) }
//! ```
//!
//! For hot loops over a few fields, build a [`Projection`]
//! once per event type with [`Chunk::projection`](chunk::Chunk::projection)
//! and decode only the fields you need in a single pass with
//! [`Event::read_projection`].
//!
//! # Runnable examples
//!
//! The `examples/` directory has complete programs: `overview` (chunks and
//! event counts), `read_events` (references and stack traces),
//! `custom_events` (application-defined events), `parallel_projection`
//! (the fast path), `query` and `flamegraph`. Run one with
//! `cargo run --release --example overview -- recording.jfr`.
//!
//! # How it is fast
//!
//! * **Memory-mapped input.** Nothing is copied. Strings borrow from the map
//!   whenever the stored encoding is valid UTF-8 or ASCII.
//! * **Two-stage parsing.** Chunk boundaries come from the 68-byte headers
//!   alone, so a file is split into independent units in microseconds, much
//!   as simdjson finds structure before decoding values.
//! * **Compiled skip programs.** Each event type is compiled once into a short
//!   list of operations in which adjacent fixed-size and varint fields are
//!   merged. Runs of varints are skipped with 64-bit mask arithmetic that
//!   locates the n-th terminator without a per-byte loop.
//! * **Lazy constant pools.** Pools are indexed as `id -> offset` only when a
//!   reference is first followed, and never decoded into objects.
//! * **Shared schemas.** Chunks with identical metadata share one parsed and
//!   compiled schema.
//! * **Read-ahead and drop-behind.** The scheduler issues `madvise` hints for
//!   upcoming chunks and releases finished ones.
//!
//! # Robustness
//!
//! Malformed input produces an [`Error`], never a panic. Chunks that were cut
//! short or were still being written are recovered by scanning the event
//! stream. Each chunk is decoded with its own schema and clock, so recordings
//! assembled from several JVMs are read correctly.
//!
//! # Files larger than memory
//!
//! On 64-bit Unix the file is memory-mapped and never read into the heap.
//! Heap use is independent of file size: it grows with the number of
//! worker threads, the size of one 8 MiB work unit, the constant pools of
//! the chunks being read, and the size of your own accumulator (for a
//! query, the number of groups). Finished work units and chunks are
//! released with `madvise(DONTNEED)`, and the kernel evicts clean mapped
//! pages whenever it needs memory, so a recording can be many times larger
//! than RAM. In a test, a 40 GB file (1.1 billion events) was scanned on a
//! machine with 38 GB of RAM with a peak footprint of 331 MB.
//!
//! On other platforms the file is read into memory when it is opened, so it
//! must fit in RAM.
//!
//! # Limits
//!
//! The file must not be modified or truncated while it is open, as with any
//! memory-mapped file. Integers are always decoded as compressed varints,
//! which is how every JDK since 11 and async-profiler write them.

#![warn(missing_docs)]

pub mod chunk;
pub mod error;
pub mod event;
pub mod file;
pub(crate) mod index;
pub(crate) mod layout;
pub mod metadata;
pub(crate) mod mmap;
pub mod query;
pub(crate) mod reader;
pub mod schema;
pub mod time;
pub mod value;

pub use error::{Error, Result};
pub use event::{Event, Events};
pub use layout::Projection;
pub use file::{JfrFile, OnChunkError, ParResult, ParallelOptions};
pub use query::{Query, QueryResult};
