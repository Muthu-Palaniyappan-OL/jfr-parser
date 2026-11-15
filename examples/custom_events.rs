//! Describes and dumps application-defined events without knowing their layout.
//!
//! Every event type in the recording's metadata is readable the same way, so
//! custom events need no registration.
//!
//! `cargo run --release --example custom_events -- recording.jfr`

use jfr_parser::JfrFile;
use std::collections::BTreeMap;

fn main() -> jfr_parser::Result<()> {
    let path = std::env::args().nth(1).expect("usage: custom_events <recording.jfr>");
    let file = JfrFile::open(path)?;

    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    file.for_each_event(&[], |event| {
        if event.name().starts_with("jdk.") {
            return Ok(());
        }
        let seen = counts.entry(event.name().to_string()).or_default();
        if *seen == 0 {
            let ty = event.type_desc();
            println!("{} ({})", ty.name, ty.label().unwrap_or("no label"));
            for (field, value) in event.obj().fields() {
                println!("    {} = {}", field.name, value?);
            }
        }
        *seen += 1;
        Ok(())
    })?;

    for (name, n) in counts {
        println!("{n:>10}  {name}");
    }
    Ok(())
}
