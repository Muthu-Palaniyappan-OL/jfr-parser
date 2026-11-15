# Performance

## Method

- Small recording: 222 MB, 12 chunks, 6.26 million events, JDK and async-profiler events. Timed in a warm page cache, best of several runs, so the numbers show CPU-bound speed.
- Large recording: the small one repeated 180 times, 40.1 GB, 1.13 billion events. A JFR file is a sequence of chunks, so a concatenation is a valid recording. The file was larger than RAM, so most of it came from disk.
- Memory: `/usr/bin/time -l`. *Peak memory footprint* counts memory the process owns. *Maximum resident set size* also counts clean file-backed pages that the kernel may reclaim.

Reproduce with the examples:

```sh
cargo build --release --examples
time target/release/examples/query rec.jfr 'SELECT count(*) FROM *'
/usr/bin/time -l target/release/examples/parallel_projection rec.jfr
```

## Results

See the tables in the [README](../README.md#is-it-the-fastest). Highlights:

- Counting events by type on the 222 MB recording: 0.25 s on one thread and 0.045 s on 11 threads. `jfrs` 0.2.5 takes 2.63 s and 1.4 GB of RSS for the same count. JDK 25 `jfr summary` takes 0.31 s.
- Decoding every top-level field of every event (references stay lazy): 13 M events/s on one thread, 77 M events/s on 11.
- 40.1 GB file, count by type: 13.6 s (2.9 GB/s) with a 331 MB footprint.

The single-thread count over all events runs at 3.9 GB/s because most events are skipped after reading their size and type. Queries that follow references (thread name, stack frames) cost more because each lookup is a hash probe into the constant-pool index and a decode.

## Memory model

Heap use is the sum of:

- the compiled schema of each distinct metadata (kilobytes),
- per active chunk, the constant-pool index, 12 bytes per slot at load factor at most one half, built only if a reference is followed,
- per work unit, the accumulator the caller creates,
- the result: for a query, one entry per group, plus a bounded sketch per percentile.

None of these depends on the file size. The mapped bytes are not heap. They are page cache that the kernel can drop at any time.

## Tuning

| Option | Default | When to change |
|---|---|---|
| `threads` | available cores | Lower it to leave cores free. |
| `target_unit_bytes` | 8 MiB | Raise it if merge cost dominates (very many groups). Lower it for better balance on a few huge chunks. |
| `lookahead_chunks` | 2 | Raise it on slow or network storage. |
| `drop_behind` | true | Turn it off if you re-scan the same file and have the RAM to cache it. |

Use `Projection` and `par_units` when a query would be too slow, since a projection decodes only the fields you name in one pass.

## Not measured

- Cold-storage throughput with a dropped page cache. The 40 GB test is the nearest evidence.
- Linux. The `madvise` constants and behaviour were checked by type-checking for `x86_64-unknown-linux-gnu`, not by running there.
- Other readers besides `jfrs` and the JDK's `jfr` tool.
