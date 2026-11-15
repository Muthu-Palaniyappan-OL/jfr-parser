//! Events and event iteration.

use crate::chunk::{Chunk, RawEvent, TYPE_CHECKPOINT, TYPE_METADATA, read_event_header};
use crate::error::{Error, Result};
use crate::layout::{Projection, Step};
use crate::metadata::{TypeDesc, Unit};
use crate::reader::varlong_at;
use crate::value::{Obj, Value};

/// One event in a chunk. Cheap to copy; field values are decoded on access.
#[derive(Clone, Copy)]
pub struct Event<'c> {
    chunk: &'c Chunk<'c>,
    type_idx: u32,
    raw: RawEvent,
}

impl<'c> Event<'c> {
    pub(crate) fn new(chunk: &'c Chunk<'c>, type_idx: u32, raw: RawEvent) -> Self {
        Event { chunk, type_idx, raw }
    }

    /// The chunk this event belongs to.
    pub fn chunk(&self) -> &'c Chunk<'c> {
        self.chunk
    }

    /// Index of the event type in the chunk's schema.
    pub fn type_idx(&self) -> u32 {
        self.type_idx
    }

    /// The event type's metadata (fields, annotations, category, ...).
    pub fn type_desc(&self) -> &'c TypeDesc {
        &self.chunk.schema.meta.types[self.type_idx as usize]
    }

    /// Fully qualified event name, e.g. `jdk.GarbageCollection`.
    pub fn name(&self) -> &'c str {
        &self.type_desc().name
    }

    /// Numeric type id as written in the stream.
    pub fn type_id(&self) -> u64 {
        self.raw.type_id
    }

    /// Size of the whole event record in bytes.
    pub fn size(&self) -> usize {
        self.raw.end - self.raw.pos
    }

    /// Offset of the event inside its chunk.
    pub fn offset(&self) -> usize {
        self.raw.pos
    }

    /// The event body as a struct, for field access by name or path.
    pub fn obj(&self) -> Obj<'c> {
        Obj { chunk: self.chunk, type_idx: self.type_idx, pos: self.raw.payload, limit: self.raw.end }
    }

    /// Reads a field, following constant-pool references.
    pub fn get(&self, name: &str) -> Result<Value<'c>> {
        self.obj().get(name)
    }

    /// Follows a dotted path such as `eventThread.javaName`.
    pub fn path(&self, path: &str) -> Result<Value<'c>> {
        self.obj().path(path)
    }

    fn well_known_raw(&self, field: Option<u32>) -> Result<Option<(i64, Unit)>> {
        let Some(f) = field else { return Ok(None) };
        let buf = &self.chunk.data[..self.raw.end];
        let pos = if f == 0 {
            self.raw.payload
        } else {
            self.chunk.schema.layout.field_pos(self.type_idx, f as usize, buf, self.raw.payload)?
        };
        let (v, _) = varlong_at(buf, pos)?;
        Ok(Some((v as i64, self.type_desc().fields[f as usize].unit)))
    }

    /// Start time in ticks as stored (the `startTime` field), if the event has one.
    pub fn start_ticks(&self) -> Result<Option<i64>> {
        Ok(self.well_known_raw(self.type_desc().well_known.start_time)?.map(|(v, _)| v))
    }

    /// Start time in nanoseconds since the Unix epoch.
    pub fn start_time_nanos(&self) -> Result<Option<i64>> {
        Ok(self
            .well_known_raw(self.type_desc().well_known.start_time)?
            .map(|(v, u)| self.chunk.time.normalize(u, v)))
    }

    /// Duration in nanoseconds, for events that carry a `duration` field.
    pub fn duration_nanos(&self) -> Result<Option<i64>> {
        Ok(self
            .well_known_raw(self.type_desc().well_known.duration)?
            .map(|(v, u)| self.chunk.time.normalize(u, v)))
    }

    /// The `eventThread` (or, for sampling events, `sampledThread`) as a `java.lang.Thread` object.
    pub fn thread(&self) -> Result<Value<'c>> {
        let td = self.type_desc();
        let idx = td.well_known.event_thread.map(|i| i as usize).or_else(|| td.field_index("sampledThread"));
        match idx {
            Some(i) => self.obj().get_idx(i),
            None => Ok(Value::Null),
        }
    }

    /// The `stackTrace` field, if the event carries one.
    pub fn stack_trace(&self) -> Result<Value<'c>> {
        match self.type_desc().well_known.stack_trace {
            Some(i) => self.obj().get_idx(i as usize),
            None => Ok(Value::Null),
        }
    }

    /// Decodes the fields selected by `proj` in one pass into `out`, in the
    /// order they were requested. `out` must be at least as long as the
    /// number of requested fields; slots for unknown fields keep their value.
    pub fn read_projection(&self, proj: &Projection, out: &mut [Value<'c>]) -> Result<()> {
        debug_assert_eq!(proj.type_idx, self.type_idx);
        let layout = &self.chunk.schema.layout;
        let tl = &layout.types[self.type_idx as usize];
        let buf = &self.chunk.data[..self.raw.end];
        let mut pos = self.raw.payload;
        for step in &proj.steps {
            match step {
                Step::Skip(ops) => pos = layout.exec(ops, buf, pos, 0)?,
                Step::Read { field, slot } => {
                    let (v, np) =
                        self.chunk.read_field(&tl.fields[*field as usize], self.raw.end, pos, true, 0)?;
                    out[*slot as usize] = v;
                    pos = np;
                }
                Step::Copy { from, slot } => out[*slot as usize] = out[*from as usize].clone(),
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for Event<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.obj(), f)
    }
}

/// Iterator over decoded events of a chunk (or a range of it).
///
/// Metadata and checkpoint records are skipped. Records whose type id the
/// metadata does not define are skipped too, as the JDK reader does.
pub struct Events<'c> {
    chunk: &'c Chunk<'c>,
    pos: usize,
    end: usize,
    wanted: Option<Vec<bool>>,
    failed: bool,
}

impl<'c> Events<'c> {
    pub(crate) fn new(chunk: &'c Chunk<'c>, start: usize, end: usize, wanted: Option<Vec<bool>>) -> Self {
        Events { chunk, pos: start, end: end.min(chunk.data.len()), wanted, failed: false }
    }
}

impl<'c> Iterator for Events<'c> {
    type Item = Result<Event<'c>>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.end && !self.failed {
            let raw = match read_event_header(self.chunk.data, self.pos, self.end) {
                Ok(r) => r,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            };
            self.pos = raw.end;
            if raw.type_id == TYPE_METADATA || raw.type_id == TYPE_CHECKPOINT {
                continue;
            }
            let Some(t) = self.chunk.schema.type_idx(raw.type_id) else { continue };
            if let Some(w) = &self.wanted {
                if !w[t as usize] {
                    continue;
                }
            }
            return Some(Ok(Event::new(self.chunk, t, raw)));
        }
        None
    }
}

impl<'f> Chunk<'f> {
    /// Iterates over every event in the chunk.
    pub fn events(&'f self) -> Events<'f> {
        let (s, e) = self.event_range();
        Events::new(self, s, e, None)
    }

    /// Iterates over events of the named types only. Other events are
    /// skipped after reading just their size and type.
    pub fn events_of(&'f self, names: &[&str]) -> Result<Events<'f>> {
        let (s, e) = self.event_range();
        self.events_in(s, e, names)
    }

    /// Like [`events_of`](Self::events_of) restricted to a byte range obtained
    /// from [`split_events`](Self::split_events). An empty `names` selects all events.
    pub fn events_in(&'f self, start: usize, end: usize, names: &[&str]) -> Result<Events<'f>> {
        if names.is_empty() {
            return Ok(Events::new(self, start, end, None));
        }
        let mut wanted = vec![false; self.schema.meta.types.len()];
        for n in names {
            let t = self
                .schema
                .meta
                .type_idx_by_name(n)
                .filter(|&t| self.schema.meta.types[t as usize].is_event)
                .ok_or_else(|| Error::NoSuchType((*n).to_string()))?;
            wanted[t as usize] = true;
        }
        Ok(Events::new(self, start, end, Some(wanted)))
    }

    /// Name-filtered iteration for scheduler use: names this chunk does not
    /// define simply match nothing.
    pub(crate) fn events_matching(&'f self, start: usize, end: usize, names: &[&str]) -> Events<'f> {
        if names.is_empty() {
            return Events::new(self, start, end, None);
        }
        let mut wanted = vec![false; self.schema.meta.types.len()];
        for n in names {
            if let Some(t) = self.schema.meta.type_idx_by_name(n) {
                if self.schema.meta.types[t as usize].is_event {
                    wanted[t as usize] = true;
                }
            }
        }
        Events::new(self, start, end, Some(wanted))
    }

    /// Splits the event stream into at most `parts` byte ranges of roughly equal
    /// size, each starting on an event boundary. Costs one size-and-type
    /// read per event, so it is cheap relative to decoding.
    pub fn split_events(&self, parts: usize) -> Result<Vec<(usize, usize)>> {
        let (start, end) = self.event_range();
        let parts = parts.max(1);
        let target = ((end - start) / parts).max(1);
        let mut out = Vec::with_capacity(parts);
        let mut range_start = start;
        let mut pos = start;
        while pos < end {
            let ev = read_event_header(self.data, pos, end)?;
            pos = ev.end;
            if pos - range_start >= target && out.len() + 1 < parts {
                out.push((range_start, pos));
                range_start = pos;
            }
        }
        if range_start < end {
            out.push((range_start, end));
        }
        Ok(out)
    }

    /// Builds a [`Projection`] for the named event type and fields.
    pub fn projection(&self, event: &str, fields: &[&str]) -> Result<Projection> {
        let t = self
            .schema
            .meta
            .type_idx_by_name(event)
            .ok_or_else(|| Error::NoSuchType(event.to_string()))?;
        let td = &self.schema.meta.types[t as usize];
        let idx = fields
            .iter()
            .map(|f| {
                td.field_index(f)
                    .map(|i| i as u32)
                    .ok_or_else(|| Error::NoSuchField { type_name: event.to_string(), field: (*f).to_string() })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Projection::new(&self.schema.layout, t, &idx))
    }
}
