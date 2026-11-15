//! Lists chunks, event types and per-type event counts.
//!
//! `cargo run --release --example overview -- recording.jfr`

use jfr_parser::{JfrFile, ParallelOptions};
use std::collections::BTreeMap;

fn main() -> jfr_parser::Result<()> {
    let path = std::env::args().nth(1).expect("usage: overview <recording.jfr>");
    let file = JfrFile::open(path)?;

    println!("{} bytes in {} chunks", file.len(), file.chunks().len());
    for info in file.chunks() {
        println!(
            "  chunk {:>3}: {:>12} bytes, v{}.{}",
            info.index,
            info.len(),
            info.header.major,
            info.header.minor
        );
    }

    let counts = file.par_fold(
        &ParallelOptions::default(),
        &[],
        BTreeMap::<String, u64>::new,
        |acc, event| {
            *acc.entry(event.name().to_string()).or_default() += 1;
            Ok(())
        },
        |mut a, b| {
            for (name, n) in b {
                *a.entry(name).or_default() += n;
            }
            a
        },
    )?;

    for (name, n) in counts.value.unwrap_or_default() {
        println!("{n:>12}  {name}");
    }
    Ok(())
}
