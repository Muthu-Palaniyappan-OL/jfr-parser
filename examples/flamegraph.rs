//! Prints folded stacks (`a;b;c count`), the input format of flamegraph tools.
//!
//! `cargo run --release --example flamegraph -- recording.jfr > stacks.txt`

use jfr_parser::{JfrFile, ParallelOptions};
use std::io::{BufWriter, Write};

fn main() -> jfr_parser::Result<()> {
    let path = std::env::args().nth(1).expect("usage: flamegraph <recording.jfr>");
    let file = JfrFile::open(path)?;

    let result = file.query(
        "SELECT stack(stackTrace) AS folded, count(*) AS samples
         FROM jdk.ExecutionSample
         GROUP BY folded",
        &ParallelOptions::default(),
    )?;

    let mut out = BufWriter::new(std::io::stdout().lock());
    for row in result.rows {
        writeln!(out, "{} {}", row[0], row[1])?;
    }
    Ok(out.flush()?)
}
