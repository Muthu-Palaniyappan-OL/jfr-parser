//! Runs a query and prints the result as a table.
//!
//! `cargo run --release --example query -- recording.jfr "SELECT name, count(*) FROM jdk.GarbageCollection GROUP BY name"`

use jfr_parser::{JfrFile, ParallelOptions};

fn main() -> jfr_parser::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(sql)) = (args.next(), args.next()) else {
        eprintln!("usage: query <recording.jfr> \"<query>\"");
        std::process::exit(2);
    };

    let file = JfrFile::open(path)?;
    let result = file.query(&sql, &ParallelOptions::default())?;
    println!("{result}");
    eprintln!(
        "scanned {} events, matched {}",
        result.stats.events_scanned, result.stats.events_matched
    );
    Ok(())
}
