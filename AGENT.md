# AGENT.md

Guidance for AI coding agents and contributors working on `jfr_parser`.

## What this is

A dependency-free Rust **library** that reads Java Flight Recorder (`.jfr`) files and runs SQL-like queries over them. Goals, in priority order: correct on any valid or damaged JFR file, fast, low memory on files larger than RAM, able to read any JDK, async-profiler or custom event with no hard-coded layouts.

Non-goals: a CLI, writing JFR files, a runtime dependency of any kind. Do not add a `[[bin]]` or a `main.rs`. The `examples/` programs are documentation.

## Commands

```sh
cargo build --release
cargo clippy --all-targets --release        # must report 0 warnings
cargo doc --no-deps                         # must report 0 warnings
cargo test --doc                            # runs the rustdoc examples
cargo +1.85 check --all-targets             # MSRV
cargo check --lib --target x86_64-pc-windows-gnu   # non-mmap fallback still compiles
cargo package --allow-dirty                 # what crates.io would receive
```

Run an example: `cargo run --release --example query -- file.jfr "SELECT count(*) FROM *"`.

## Verifying changes

There is no checked-in test recording. Verify behaviour against the JDK's own `jfr` tool on a real file:

- `jfr summary file.jfr` must match per-type event counts and sizes (type names can differ for files concatenated from several writers, because `jfr` applies the first chunk's names to all chunks).
- `jfr print --events jdk.ExecutionSample file.jfr` for field values.
- `jfr print --json` as an independent oracle for query aggregates.
- Parallel results must equal sequential results for every thread count.
- Corrupt and truncated input must return `Err`, never panic or hang. Fuzz changes to `reader`, `chunk`, `metadata` and `layout` in a debug build, which has overflow checks.

For performance changes, time `count(*)`, a group-by that resolves references, and `examples/parallel_projection` on a file of at least 200 MB, and report the before and after. Check peak memory with `/usr/bin/time -l` (macOS) or `-v` (Linux).

## Layout

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the format, module map and design decisions. In short: `file.rs` (JfrFile, scheduler) -> `chunk.rs` (one chunk) -> `schema.rs`/`metadata.rs`/`layout.rs` (types and skip programs) -> `event.rs`/`value.rs` (decoding) -> `query/` (language).

## Invariants. Do not break these

1. **Per-chunk schema and clock.** Never reuse one chunk's metadata or `TimeConverter` for another. `SchemaCache` shares only byte-identical metadata.
2. **Constant pools stay lazy.** Do not build the pool index on chunk load. Scans that never follow a reference must not pay for it.
3. **No panics on input.** No `unwrap`, `expect`, unchecked indexing or unchecked arithmetic on values that come from the file. Bound every loop and recursion that the file controls.
4. **`unsafe` only in `mmap.rs`.** Keep a `SAFETY:` comment on each block.
5. **Memory independent of file size.** Anything that keeps per-chunk or per-event state for the whole run is a bug. The scheduler merges chunk results in order as they finish. Keep it that way.
6. **Results do not depend on thread count.** `merge` is associative and runs in file order.
7. **Errors, not silence.** An unknown event name or field is an `Error`, not an empty result.
8. **Public API stays small.** `index`, `layout`, `reader` and `mmap` are `pub(crate)`. New `pub` items need rustdoc (`#![warn(missing_docs)]`) and a reason. The query AST stays private.

## Style

- Rust 2024 edition, MSRV 1.85. No new dependencies.
- Public items get rustdoc, with a runnable or `no_run` example where it helps. Keep comments out of function bodies unless they explain *why* (a bound, an ordering requirement, a safety argument). Do not narrate what the code does.
- No hacks. Fix the cause. Do not special-case a file, a type name or a test.
- Prefer a measured change. Hot paths (`reader.rs`, `layout.rs`, `value.rs`, `query/eval.rs`) were tuned with benchmarks, so benchmark before and after.
- Platform differences: `mmap.rs` uses `cfg(all(unix, target_pointer_width = "64"))`. Elsewhere the file is read into memory. Keep both compiling.

## Release checklist

1. Clippy, doc, doc-tests, MSRV and the Windows type-check pass (commands above).
2. Compare `jfr summary` and `jfr print` output on a real recording.
3. Update `CHANGELOG.md` and the version in `Cargo.toml`.
4. `cargo package --list` shows only `src`, `examples`, `docs`, README, CHANGELOG, LICENSE.
5. `cargo publish --dry-run`, then `cargo publish`.
