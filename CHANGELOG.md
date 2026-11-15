# Changelog

## 0.1.0

First release.

- `JfrFile`: memory-mapped open, chunk table, sequential and parallel iteration (`for_each_event`, `par_units`, `par_fold`).
- Schema-driven decoding of any JDK, async-profiler or custom event, with lazy constant pools and path access (`event.path("a.b.c")`).
- `Projection` for single-pass decoding of selected fields.
- SQL-like query language with aggregates, percentiles, unit literals and stack helpers.
- Recovery of truncated and unfinished chunks.
- No dependencies. Rust 1.85 or newer.
