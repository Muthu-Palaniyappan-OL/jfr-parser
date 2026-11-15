# jfr_parser

[![crates.io](https://img.shields.io/crates/v/jfr_parser.svg)](https://crates.io/crates/jfr_parser)
[![docs.rs](https://img.shields.io/docsrs/jfr_parser)](https://docs.rs/jfr_parser)
[![license](https://img.shields.io/crates/l/jfr_parser.svg)](LICENSE)

A fast, low-memory reader and query engine for Java Flight Recorder (JFR) files.
It is a library with no dependencies.

- **Fast.** Up to 17 GB/s on a cached file, and 2.9 GB/s over a 40 GB file.
- **Low memory.** A 40 GB recording with 1.1 billion events was scanned with a 331 MB peak footprint.
- **Schema-driven.** Nothing about JDK events is hard-coded. The parser decodes what the recording's own metadata declares, so JDK events, [async-profiler](https://github.com/async-profiler/async-profiler) events and your own custom events all work the same way.
- **Robust.** Malformed input returns an `Error`. Truncated and unfinished chunks are recovered.
- **Queryable.** A SQL-like language runs in parallel over the whole file.

```toml
[dependencies]
jfr_parser = "0.1"
```

Rust 1.85 or newer. Memory-mapped input on 64-bit Unix (Linux, macOS); on other platforms the file is read into memory.

## Read events

```rust
use jfr_parser::JfrFile;

fn main() -> jfr_parser::Result<()> {
    let file = JfrFile::open("recording.jfr")?;

    file.for_each_event(&["jdk.GarbageCollection"], |event| {
        let name = event.get("name")?;
        let pause_ns = event.duration_nanos()?.unwrap_or(0);
        println!("{name}: {} ms", pause_ns / 1_000_000);
        Ok(())
    })
}
```

`event.get("field")` and `event.path("a.b.c")` return a `Value`. References to threads, classes, methods and stack traces are followed for you, and strings borrow from the file when they can. A custom event such as `com.example.Checkout` needs no registration: `event.get("orderId")` works.

## Query

```rust
use jfr_parser::{JfrFile, ParallelOptions};

let file = JfrFile::open("recording.jfr")?;
let result = file.query(
    "SELECT threadname(sampledThread) AS thread, count(*) AS samples
     FROM jdk.ExecutionSample
     WHERE hasframe(stackTrace, 'HashMap.put')
     GROUP BY thread ORDER BY samples DESC LIMIT 10",
    &ParallelOptions::default(),
)?;
println!("{result}");          // aligned table
let csv = result.to_csv();     // or result.to_json(), or read result.rows
```

The language has `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, aggregates (`count`, `sum`, `avg`, `min`, `max`, `count(DISTINCT x)`, `p50` to `p999`), paths through references and arrays, unit literals (`duration > 10ms`, `size >= 64mb`), time bucketing, and stack helpers (`topframe`, `stack`, `hasframe`, `depth`). `stack(stackTrace)` produces folded stacks for flame graphs. The full reference is in the [`query` module docs](https://docs.rs/jfr_parser/latest/jfr_parser/query/index.html).

## Process in parallel

```rust
use jfr_parser::{JfrFile, ParallelOptions};
use std::collections::HashMap;

let file = JfrFile::open("recording.jfr")?;
let counts = file.par_fold(
    &ParallelOptions::default(),
    &[],                                   // all event types
    HashMap::<String, u64>::new,           // one accumulator per work unit
    |acc, event| { *acc.entry(event.name().to_string()).or_default() += 1; Ok(()) },
    |mut a, b| { for (k, v) in b { *a.entry(k).or_default() += v; } a },
)?;
```

Chunks are loaded when a worker needs them and released when done. A single huge chunk is split into event ranges so every core is used. Partial results merge in file order as soon as the chunks before them are finished. For hot loops over a few fields, build a `Projection` and decode just those fields in one pass (see `examples/parallel_projection.rs`).

## Examples

| Example | Shows |
|---|---|
| [`overview`](examples/overview.rs) | chunks, event types and counts |
| [`read_events`](examples/read_events.rs) | following references and walking stack traces |
| [`custom_events`](examples/custom_events.rs) | discovering and dumping application-defined events |
| [`parallel_projection`](examples/parallel_projection.rs) | the fast path: a projection inside `par_units` |
| [`query`](examples/query.rs) | running a query from the command line |
| [`flamegraph`](examples/flamegraph.rs) | folded stacks for flamegraph tools |

```sh
cargo run --release --example overview -- recording.jfr
```

## Does it work on files larger than memory?

Yes. The file is memory-mapped, never read into the heap, and processed one 8 MiB work unit at a time. Finished units and chunks are handed back to the kernel with `madvise(DONTNEED)`, and the kernel can evict clean mapped pages whenever it needs the memory. Heap use depends on the thread count, the constant pools of the chunks being read, and your accumulator (for a query, the number of groups). It does not depend on the file size.

Measured on a 40.1 GB recording with 1.13 billion events, on a machine whose RAM was smaller than the file:

| Task | Time | Peak footprint |
|---|---|---|
| Count events by type | 13.6 s (2.9 GB/s) | 331 MB |
| Sum a field per thread name, 856 M events (projection) | 18.6 s | 59 MB |
| Group by thread name with `sum` and `p99`, 856 M events (query) | 30.3 s | 331 MB |

Peak footprint is the `peak memory footprint` that `/usr/bin/time -l` reports on macOS. The same runs show a maximum resident set of about 14 GB, because macOS counts clean file-backed pages that the kernel is free to reclaim and does not release them on `madvise`. On Linux, `MADV_DONTNEED` drops them immediately. I only measured macOS. See [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Is it the fastest?

I compared it with the one Rust JFR crate I know of, [`jfrs`](https://crates.io/crates/jfrs) 0.2.5, and with the JDK's `jfr summary`, on a 222 MB, 12-chunk, 6.26-million-event recording held in the page cache. The task is counting events by type:

| Reader | Time | Peak memory |
|---|---|---|
| `jfrs` 0.2.5, single thread | 2.63 s | 1.4 GB RSS |
| JDK 25 `jfr summary` | 0.31 s | 110 MB footprint |
| `jfr_parser`, 1 thread | 0.25 s | |
| `jfr_parser`, 11 threads | 0.045 s | 6 MB footprint |

This is not a like-for-like comparison. `jfrs` decodes every event into an object tree, while `jfr_parser` reads only the size and type of each event it does not need, which is the point of the design. When every field of every event is decoded (`Obj::fields`, references left as lazy handles), `jfr_parser` runs at 13 million events per second on one thread and 77 million per second on 11 threads. I did not benchmark async-profiler's converter or other Java readers, so I can't say it is the fastest reader in existence. Measure on your own recordings with the examples.

### Throughput on the 222 MB recording

All numbers are for a warm page cache, so they show CPU-bound speed. MB/s is file size divided by time.

| Query | 1 thread | 11 threads |
|---|---|---|
| count all events | 57 ms (3.9 GB/s) | 13 ms (16.9 GB/s) |
| `sum` of one field | 77 ms (2.9 GB/s) | 14 ms (15.6 GB/s) |
| group by event type | 247 ms (0.9 GB/s) | 45 ms (4.9 GB/s) |
| group by top frame (resolves stack traces) | 94 ms (2.4 GB/s) | 34 ms (6.5 GB/s) |
| group by thread name (resolves references) | 312 ms (0.7 GB/s) | 58 ms (3.9 GB/s) |

Opening the file takes about 60 µs, because only the 68-byte chunk headers are read.

## Why it is fast

- **Memory-mapped, zero copy.** No read buffers. Strings borrow from the map.
- **Two-stage parsing, as in simdjson.** Chunk boundaries come from the headers alone, so the file splits into independent units before any event is decoded.
- **Compiled skip programs.** Each event type is compiled once into a short list of operations in which neighbouring fixed-size and varint fields are merged. A run of varints is skipped with 64-bit mask arithmetic that finds the n-th terminator without a per-byte loop. An array of small structs, such as stack frames, is skipped as a single run.
- **Lazy constant pools.** A pool is indexed as `id -> offset` the first time a reference is followed. Entries are never decoded into objects.
- **Shared schemas.** Chunks with identical metadata share one parsed and compiled schema.
- **Read-ahead and drop-behind.** The scheduler hints upcoming chunks with `madvise(WILLNEED)` and releases finished ranges.

## Correctness

Checked against the JDK's own tools on a real recording:

- per-type event counts and byte sizes match `jfr summary`,
- all 10,352 `jdk.ExecutionSample` events (time, thread, state, top frame line) match `jfr print`,
- query aggregates match values computed independently from `jfr print --json`,
- 9,500 randomly corrupted and truncated inputs ran in a debug build with overflow checks and produced no panics or hangs.

A chunk cut in half still yielded 375,327 of its 392,419 events. Each chunk is decoded with its own schema and clock, so recordings concatenated from several JVMs read correctly. (The JDK reader reuses the first chunk's clock for all chunks, so absolute times differ for such files.)

## Limits

- The file must not be truncated or rewritten while it is open. This is the usual rule for memory-mapped files.
- Integers are read as compressed varints, which every JDK since 11 and async-profiler write.
- Percentiles use a sketch with about 1% relative error. Everything else in the query language is exact.
- Only the memory-mapped path (64-bit Unix) handles files larger than RAM.

## More

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): file format, module map, design decisions.
- [docs/PERFORMANCE.md](docs/PERFORMANCE.md): methodology, memory model and tuning.
- [CHANGELOG.md](CHANGELOG.md)

## License

MIT
