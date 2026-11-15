//! Fast path: decode only the fields you need, on every core.
//!
//! Totals allocated bytes per thread. A `Projection` is built once per work
//! unit and decodes the chosen fields in a single pass.
//!
//! `cargo run --release --example parallel_projection -- recording.jfr`

use jfr_parser::value::Value;
use jfr_parser::{JfrFile, ParallelOptions};
use std::collections::HashMap;

const EVENT: &str = "jdk.ObjectAllocationInNewTLAB";

fn main() -> jfr_parser::Result<()> {
    let path = std::env::args().nth(1).expect("usage: parallel_projection <recording.jfr>");
    let file = JfrFile::open(path)?;

    let totals = file.par_units(
        &ParallelOptions::default(),
        |chunk, start, end| {
            let mut totals = HashMap::<String, i64>::new();
            let Ok(proj) = chunk.projection(EVENT, &["eventThread", "allocationSize"]) else {
                return Ok(totals);
            };
            let mut fields = [Value::Null, Value::Null];
            for event in chunk.events_in(start, end, &[EVENT])? {
                event?.read_projection(&proj, &mut fields)?;
                let thread = fields[0].as_obj().map(|t| t.get("javaName")).transpose()?;
                let name = thread.as_ref().and_then(Value::as_str).unwrap_or("<unknown>");
                let bytes = fields[1].as_i64().unwrap_or(0);
                match totals.get_mut(name) {
                    Some(total) => *total += bytes,
                    None => {
                        totals.insert(name.to_string(), bytes);
                    }
                }
            }
            Ok(totals)
        },
        |mut a, b| {
            for (name, bytes) in b {
                *a.entry(name).or_default() += bytes;
            }
            a
        },
    )?;

    let mut rows: Vec<_> = totals.value.unwrap_or_default().into_iter().collect();
    rows.sort_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
    for (name, bytes) in rows.into_iter().take(10) {
        println!("{:>10.1} MiB  {name}", bytes as f64 / (1 << 20) as f64);
    }
    Ok(())
}
