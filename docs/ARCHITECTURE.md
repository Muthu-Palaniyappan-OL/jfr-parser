# Architecture

`jfr_parser` reads a recording in two stages, much as simdjson finds structure before it decodes values.

1. **Structure.** Chunk boundaries come from the 68-byte chunk headers alone. Nothing inside a chunk is read.
2. **Content.** Each chunk is independent. Its metadata is compiled into skip programs, its constant pools are indexed on first use, and its events are decoded on demand.

## File format

A recording is a sequence of self-contained chunks.

```text
chunk header (68 bytes, big endian)
  0  magic "FLR\0"                 4  major u16, minor u16
  8  chunk size u64               16  newest checkpoint offset u64
 24  metadata offset u64          32  start time, ns since epoch
 40  duration ns                  48  start ticks
 56  ticks per second             64  file state u8, 67 feature flags

event stream: [size varint][type varint][payload] ...
  size counts the whole record including the size field
  type 0 = metadata, type 1 = checkpoint, anything else is an event
```

- **Varints** are LEB128 with a ninth raw byte. **Floats and doubles** are raw big-endian.
- **Strings** start with an encoding byte: 0 null, 1 empty, 2 constant-pool ref, 3 UTF-8, 4 `char[]`, 5 Latin-1.
- **Metadata** is a string table followed by a tree of `class`, `field`, `annotation` and `setting` elements.
- **Checkpoints** hold constant pools: `start, duration, delta, flush, pool count, (type id, count, (key, value)*)*`. `delta` links to the previous checkpoint, so the newest one in the header leads to all of them.

## Modules

| Module | Role |
|---|---|
| `file` | `JfrFile`, the chunk table, the parallel scheduler (`par_units`, `par_fold`) |
| `chunk` | header parsing, chunk scan, schema lookup, checkpoint chain, recovery |
| `metadata` | the type system: classes, fields, annotations, units |
| `schema` | metadata plus compiled layout, shared between chunks with equal metadata |
| `layout` (private) | the skip-program compiler and `Projection` |
| `index` (private) | open-addressing `id -> offset` table for constant pools |
| `reader` (private) | varints, the SWAR `skip_varints`, string headers |
| `event`, `value` | `Event`, and the lazily decoded `Value`, `Obj`, `Array` |
| `query` | parser, evaluator and aggregates of the SQL-like language |
| `mmap` (private) | `mmap` and `madvise` through `extern "C"`, with a `Vec` fallback |
| `time`, `error` | tick conversion and unit normalization, the error type |

The only `unsafe` code is in `mmap.rs`.

## Design decisions

**Zero dependencies.** `mmap` and `madvise` are three `extern "C"` declarations. Threads use `std::thread::scope`. A dependency on `memmap2` or `rayon` would add little and cost compile time and supply-chain surface.

**Skip programs.** An event is mostly skipped, so each type is compiled once into a short list of ops: `Varints(n)`, `Bytes(n)`, `Str`, `Struct`, `Array`. Neighbouring fields of the same wire class merge. Structs made only of varints, such as `StackFrame`, flatten into `Varints(4)`, so an array of frames skips as one run. `skip_varints` locates the n-th terminator with 64-bit mask arithmetic.

**Lazy constant pools.** A pool is indexed as `id -> offset` the first time a reference is followed, and entries are decoded from the mapped file on demand. Loading a chunk went from 46 ms to 1 ms when pools stopped being built eagerly. Scans that never follow a reference never build them. Checkpoints are applied oldest first so newer entries win.

**Per-chunk schema and clock.** `SchemaCache` is keyed by `(metadata id, size)` and verified byte for byte, so identical metadata shares one compiled schema while different writers do not collide. Each chunk converts ticks with its own clock. A recording concatenated from several JVMs can reuse type ids with different meanings, and the JDK's reader mislabels those.

**Reduced simple types.** A type with a single field (for example `jdk.types.ThreadState`) is read as that field, as the JDK does. `state` and `state.name` both work.

**Recovery.** If a chunk's header pointers are zero or stale (an unfinished or cut file), `anchors_from_scan` hops the event stream to find the metadata and checkpoints. Unknown event types are skipped, as in the JDK. A chunk whose size field is zero, or runs past the end of the file, extends to the end of the file.

**Never panic on bad input.** Every length, offset and nesting depth read from a file is checked. Struct nesting, reference chains and checkpoint chains have bounds, and cycles are detected. A corpus of 9,500 corrupted and truncated inputs ran without a panic.

## Parallel scheduler

`JfrFile::par_units` runs a closure over event ranges on `std::thread::scope` workers.

1. A worker takes the next range of an active chunk, or claims and loads the next chunk.
2. Chunks over `target_unit_bytes` (8 MiB) are split into event ranges at record boundaries, so one huge chunk still uses every core.
3. Loading a chunk issues `madvise(WILLNEED)` for the next `lookahead_chunks`.
4. Each finished range is released with `madvise(DONTNEED)`, and so is the whole chunk when its last range ends.
5. Chunk results merge in file order. As soon as chunks `0..k` are done, one worker folds them into the running result, outside the lock. Memory therefore does not grow with the number of chunks.

`merge` must be associative. The result does not depend on the thread count.

## Query engine

`parse.rs` turns text into an AST that stays private, `eval.rs` evaluates expressions against an `Env`, and `mod.rs` plans and runs the query on `par_units`.

- Field names and paths are resolved once per chunk into projections and a per-type plan.
- Aggregates are mergeable. Percentiles use a log-bucketed sketch (about 1% relative error).
- Units are normalized: durations to nanoseconds, timestamps to epoch nanoseconds.
- `ORDER BY ... LIMIT n` keeps only the best rows in each unit.
- Names in `FROM` and fields that no chunk defines are errors, not empty results.
