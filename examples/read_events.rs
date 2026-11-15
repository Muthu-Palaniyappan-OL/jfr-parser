//! Reads one event type sequentially and follows references.
//!
//! `cargo run --release --example read_events -- recording.jfr`

use jfr_parser::JfrFile;

fn main() -> jfr_parser::Result<()> {
    let path = std::env::args().nth(1).expect("usage: read_events <recording.jfr>");
    let file = JfrFile::open(path)?;

    let mut shown = 0;
    file.for_each_event(&["jdk.ExecutionSample"], |event| {
        if shown == 5 {
            return Ok(());
        }
        shown += 1;

        let thread = event.path("sampledThread.javaName")?;
        let state = event.get("state")?;
        let time = event.start_time_nanos()?.unwrap_or(0);
        println!("t={time}ns thread={thread} state={state}");

        if let Some(trace) = event.stack_trace()?.as_obj() {
            if let Some(frames) = trace.get("frames")?.as_array() {
                for frame in frames.iter().take(3) {
                    let frame = frame?;
                    if let Some(frame) = frame.as_obj() {
                        let class = frame.path("method.type.name")?;
                        let method = frame.path("method.name")?;
                        let line = frame.get("lineNumber")?;
                        println!("    {class}.{method}:{line}");
                    }
                }
            }
        }
        Ok(())
    })
}
