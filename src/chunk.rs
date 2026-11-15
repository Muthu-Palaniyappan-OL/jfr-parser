//! One chunk of a recording: header, schema, constant pools and the event stream.
//!
//! A JFR file is a sequence of self-contained chunks. Each starts with a
//! 68-byte header whose `chunk size` field gives the start of the next
//! chunk, so the whole file can be split into chunks without looking at a
//! single event. That is the cheap first stage; per-chunk work (schema,
//! constant pools, events) is independent and can run in parallel.
//!
//! Header layout (big endian, as in the JDK's `ChunkHeader`):
//! ```text
//!  0  magic "FLR\0"          4  major u16, minor u16
//!  8  chunk size u64        16  constant pool offset u64
//! 24  metadata offset u64   32  start time, ns since epoch
//! 40  duration ns           48  start ticks
//! 56  ticks per second      64  file state u8, 65..66 reserved, 67 feature flags
//! ```
//! Event stream: `[size varint][type varint][payload]*`, where `size`
//! counts the whole event including the size field itself.

use crate::error::{Error, Result, corrupt, truncated};
use crate::index::IdIndex;
use crate::reader::{skip_varints, varlong_at};
use crate::schema::{Schema, SchemaCache};
use crate::time::TimeConverter;
use std::sync::{Arc, OnceLock};

/// Size of a chunk header in bytes; the event stream starts right after it.
pub const HEADER_SIZE: usize = 68;
/// The four bytes every chunk starts with.
pub const MAGIC: [u8; 4] = *b"FLR\0";

/// Event type id of the metadata event.
pub const TYPE_METADATA: u64 = 0;
/// Event type id of checkpoint (constant pool) events.
pub const TYPE_CHECKPOINT: u64 = 1;

/// The fixed 68-byte header at the start of every chunk.
#[derive(Debug, Clone)]
pub struct ChunkHeader {
    /// Major format version (1 or 2).
    pub major: u16,
    /// Minor format version.
    pub minor: u16,
    /// Chunk length in bytes; zero while the JVM has not finalized the chunk.
    pub size: u64,
    /// Offset of the newest checkpoint event, relative to the chunk start.
    pub constant_pool_offset: u64,
    /// Offset of the metadata event, relative to the chunk start.
    pub metadata_offset: u64,
    /// Wall-clock start of the chunk, nanoseconds since the Unix epoch.
    pub start_nanos: i64,
    /// Wall-clock length of the chunk in nanoseconds.
    pub duration_nanos: i64,
    /// Value of the tick counter at the start of the chunk.
    pub start_ticks: i64,
    /// Tick counter frequency.
    pub ticks_per_second: i64,
    /// Zero once the JVM has closed the chunk.
    pub file_state: u8,
    /// Feature flag byte (bit 0: compressed integers, bit 1: final chunk).
    pub flags: u8,
}

fn be_u64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3], b[at + 4], b[at + 5], b[at + 6], b[at + 7]])
}

impl ChunkHeader {
    /// Parses the header at `at` in `file`.
    pub fn parse(file: &[u8], at: usize) -> Result<ChunkHeader> {
        let h = at
            .checked_add(HEADER_SIZE)
            .and_then(|e| file.get(at..e))
            .ok_or_else(|| truncated("chunk header", at))?;
        if h[..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        let major = u16::from_be_bytes([h[4], h[5]]);
        let minor = u16::from_be_bytes([h[6], h[7]]);
        if major != 1 && major != 2 {
            return Err(Error::UnsupportedVersion { major, minor });
        }
        Ok(ChunkHeader {
            major,
            minor,
            size: be_u64(h, 8),
            constant_pool_offset: be_u64(h, 16),
            metadata_offset: be_u64(h, 24),
            start_nanos: be_u64(h, 32) as i64,
            duration_nanos: be_u64(h, 40) as i64,
            start_ticks: be_u64(h, 48) as i64,
            ticks_per_second: be_u64(h, 56) as i64,
            file_state: h[64],
            flags: h[67],
        })
    }

    /// Whether the header advertises compressed integers.
    pub fn compressed_ints(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Whether this is the last chunk of the recording.
    pub fn is_final_chunk(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Location and header of a chunk inside a file.
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    /// Position in the file's chunk list.
    pub index: usize,
    /// Absolute offset of the chunk's first byte.
    pub start: u64,
    /// Absolute offset one past the chunk's last byte (clamped to the file length).
    pub end: u64,
    /// The parsed header.
    pub header: ChunkHeader,
    /// False if the header size is zero or the chunk runs past the end of the file
    /// (a recording that was still being written or was cut short).
    pub complete: bool,
}

impl ChunkInfo {
    /// Length of the chunk in bytes (clamped to the file).
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
    /// True for a zero-length chunk.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Splits a file into chunks by following the size field of each header.
/// Returns the chunks and the number of unparseable bytes after the last one.
pub(crate) fn scan_chunks(file: &[u8]) -> Result<(Vec<ChunkInfo>, u64)> {
    let len = file.len();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < len {
        let header = match ChunkHeader::parse(file, pos) {
            Ok(h) => h,
            Err(e) if out.is_empty() => return Err(e),
            Err(_) => return Ok((out, (len - pos) as u64)),
        };
        let size = header.size;
        let (end, complete) = if size == 0 {
            (len, false)
        } else if size < HEADER_SIZE as u64 {
            return if out.is_empty() {
                Err(corrupt("chunk size smaller than its header", pos))
            } else {
                Ok((out, (len - pos) as u64))
            };
        } else {
            match usize::try_from(size).ok().and_then(|s| pos.checked_add(s)).filter(|&e| e <= len) {
                Some(e) => (e, true),
                None => (len, false),
            }
        };
        out.push(ChunkInfo { index: out.len(), start: pos as u64, end: end as u64, header, complete });
        pos = end;
    }
    Ok((out, 0))
}

/// An event located in the stream but not yet decoded.
#[derive(Debug, Clone, Copy)]
pub struct RawEvent {
    /// Numeric event type id.
    pub type_id: u64,
    /// Offset (within the chunk) of the event's size field.
    pub pos: usize,
    /// Offset of the first payload byte (after size and type).
    pub payload: usize,
    /// Offset one past the event.
    pub end: usize,
}

/// Reads the size and type of the event at `pos` and validates the bounds.
#[inline(always)]
pub fn read_event_header(data: &[u8], pos: usize, limit: usize) -> Result<RawEvent> {
    let (size, p) = varlong_at(data, pos)?;
    let (type_id, payload) = varlong_at(data, p)?;
    let end = usize::try_from(size).ok().and_then(|s| pos.checked_add(s));
    match end {
        Some(end) if end <= limit && end >= payload => Ok(RawEvent { type_id, pos, payload, end }),
        _ => Err(corrupt("event size out of range", pos)),
    }
}

/// Iterates the events of a byte range of a chunk without decoding payloads.
/// Costs two varint reads per event.
pub struct RawEvents<'c> {
    data: &'c [u8],
    pos: usize,
    end: usize,
    failed: bool,
}

impl<'c> RawEvents<'c> {
    /// Iterates the records of `data[start..end]`, which must begin on a record boundary.
    pub fn new(data: &'c [u8], start: usize, end: usize) -> Self {
        RawEvents { data, pos: start, end: end.min(data.len()), failed: false }
    }
}

impl Iterator for RawEvents<'_> {
    type Item = Result<RawEvent>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end || self.failed {
            return None;
        }
        match read_event_header(self.data, self.pos, self.end) {
            Ok(e) => {
                self.pos = e.end;
                Some(Ok(e))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// A chunk loaded for reading: schema compiled, constant pools indexed.
pub struct Chunk<'f> {
    /// Where the chunk lives in the file and its header.
    pub info: ChunkInfo,
    pub(crate) data: &'f [u8],
    pub(crate) schema: Arc<Schema>,
    pools: OnceLock<Result<Vec<IdIndex>>>,
    checkpoints: Vec<usize>,
    /// Converts this chunk's ticks to wall-clock time.
    pub time: TimeConverter,
    /// End of the readable event stream (chunk-relative).
    pub(crate) events_end: usize,
}

/// Where the metadata event and checkpoint chain were found.
struct Anchors {
    metadata: usize,
    checkpoints: Vec<usize>,
    events_end: usize,
}

impl<'f> Chunk<'f> {
    /// Loads the chunk described by `info` from `file`: parses (or fetches from
    /// `cache`) the schema.
    ///
    /// Constant pools are indexed lazily, the first time a reference is
    /// followed, so scans that never resolve a reference do not pay for it.
    /// Call [`index_constant_pools`](Self::index_constant_pools) to force the
    /// work and surface any corruption up front.
    pub(crate) fn load(file: &'f [u8], info: &ChunkInfo, cache: &SchemaCache) -> Result<Chunk<'f>> {
        let data = Self::chunk_bytes(file, info)?;
        let anchors = Self::anchors(data, info)?;
        let schema = cache.get(data.get(anchors.metadata..).ok_or_else(|| truncated("metadata", anchors.metadata))?)?;
        let h = &info.header;
        Ok(Chunk {
            info: info.clone(),
            data,
            schema,
            pools: OnceLock::new(),
            checkpoints: anchors.checkpoints,
            time: TimeConverter::new(h.start_nanos, h.start_ticks, h.ticks_per_second),
            events_end: anchors.events_end,
        })
    }

    /// Builds the constant pool index now (if not built yet) and reports errors.
    pub fn index_constant_pools(&self) -> Result<()> {
        self.pools().map(|_| ())
    }

    pub(crate) fn pools(&self) -> Result<&[IdIndex]> {
        match self.pools.get_or_init(|| build_pools(self.data, &self.schema, &self.checkpoints)) {
            Ok(p) => Ok(p),
            Err(e) => Err(e.clone()),
        }
    }

    /// Loads only the schema of a chunk, without indexing constant pools.
    pub(crate) fn load_schema(file: &'f [u8], info: &ChunkInfo, cache: &SchemaCache) -> Result<Arc<Schema>> {
        let data = Self::chunk_bytes(file, info)?;
        let anchors = Self::anchors(data, info)?;
        cache.get(data.get(anchors.metadata..).ok_or_else(|| truncated("metadata", anchors.metadata))?)
    }

    fn chunk_bytes(file: &'f [u8], info: &ChunkInfo) -> Result<&'f [u8]> {
        let start = usize::try_from(info.start).map_err(|_| corrupt("chunk offset", 0))?;
        let end = usize::try_from(info.end).map_err(|_| corrupt("chunk offset", 0))?;
        file.get(start..end).ok_or_else(|| truncated("chunk", start))
    }

    fn anchors(data: &[u8], info: &ChunkInfo) -> Result<Anchors> {
        match Self::anchors_from_header(data, &info.header) {
            Ok(a) => Ok(a),
            Err(e) if info.complete && info.header.file_state == 0 => Err(e),
            Err(_) => Self::anchors_from_scan(data),
        }
    }

    fn anchors_from_header(data: &[u8], h: &ChunkHeader) -> Result<Anchors> {
        let meta = usize::try_from(h.metadata_offset).ok().filter(|&o| (HEADER_SIZE..data.len()).contains(&o));
        let cp = usize::try_from(h.constant_pool_offset).ok().filter(|&o| (HEADER_SIZE..data.len()).contains(&o));
        let (Some(metadata), Some(cp)) = (meta, cp) else {
            return Err(corrupt("metadata or constant pool offset outside the chunk", 0));
        };
        let mut positions = vec![cp];
        let mut seen = std::collections::HashSet::from([cp]);
        let mut pos = cp;
        loop {
            let ev = read_event_header(data, pos, data.len())?;
            if ev.type_id != TYPE_CHECKPOINT {
                return Err(corrupt("expected a checkpoint event", pos));
            }
            let p = skip_varints(&data[..ev.end], ev.payload, 2)?;
            let (delta, _) = varlong_at(&data[..ev.end], p)?;
            let delta = delta as i64;
            if delta == 0 {
                break;
            }
            let next = i64::try_from(pos)
                .ok()
                .and_then(|p| p.checked_add(delta))
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| (HEADER_SIZE..data.len()).contains(&n))
                .ok_or_else(|| corrupt("checkpoint chain leaves the chunk", pos))?;
            if !seen.insert(next) {
                return Err(corrupt("cycle in checkpoint chain", pos));
            }
            positions.push(next);
            pos = next;
        }
        positions.sort_unstable();
        Ok(Anchors { metadata, checkpoints: positions, events_end: data.len() })
    }

    /// Recovery path: find the metadata and all checkpoints by hopping the stream.
    fn anchors_from_scan(data: &[u8]) -> Result<Anchors> {
        let mut metadata = None;
        let mut checkpoints = Vec::new();
        let mut pos = HEADER_SIZE;
        let mut events_end = HEADER_SIZE;
        while pos < data.len() {
            let Ok(ev) = read_event_header(data, pos, data.len()) else { break };
            match ev.type_id {
                TYPE_METADATA => metadata = Some(pos),
                TYPE_CHECKPOINT => checkpoints.push(pos),
                _ => {}
            }
            pos = ev.end;
            events_end = ev.end;
        }
        let metadata = metadata.ok_or_else(|| corrupt("no metadata event found in chunk", 0))?;
        Ok(Anchors { metadata, checkpoints, events_end })
    }

    /// The raw bytes of this chunk.
    pub fn bytes(&self) -> &'f [u8] {
        self.data
    }

    /// The chunk's schema, shared with other chunks that have identical metadata.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Absolute events byte range of this chunk's stream (chunk-relative).
    pub fn event_range(&self) -> (usize, usize) {
        (HEADER_SIZE, self.events_end)
    }

    /// Iterates over every record in the stream, including metadata and
    /// checkpoint events.
    pub fn raw_events(&self) -> RawEvents<'f> {
        RawEvents::new(self.data, HEADER_SIZE, self.events_end)
    }

    /// Number of entries in the constant pool of type `type_idx`.
    pub fn pool_len(&self, type_idx: u32) -> Result<usize> {
        Ok(self.pools()?.get(type_idx as usize).map_or(0, IdIndex::len))
    }

    /// Offset of the pool entry `id` of type `type_idx`.
    #[inline]
    pub(crate) fn pool_offset(&self, type_idx: u32, id: u64) -> Result<Option<usize>> {
        Ok(self.pools()?.get(type_idx as usize).and_then(|p| p.get(id)))
    }

    /// Approximate heap memory used by the constant pool indexes.
    pub fn index_heap_bytes(&self) -> Result<usize> {
        Ok(self.pools()?.iter().map(IdIndex::heap_bytes).sum())
    }
}

/// Indexes every checkpoint, oldest first so newer entries override older ones.
fn build_pools(data: &[u8], schema: &Schema, checkpoints: &[usize]) -> Result<Vec<IdIndex>> {
    let wide = data.len() >= u32::MAX as usize;
    let mut pools: Vec<IdIndex> = (0..schema.meta.types.len()).map(|_| IdIndex::new(wide)).collect();
    for &at in checkpoints {
        let ev = read_event_header(data, at, data.len())?;
        if ev.type_id != TYPE_CHECKPOINT {
            return Err(corrupt("expected a checkpoint event", at));
        }
        let buf = &data[..ev.end];
        let p = skip_varints(buf, ev.payload, 4)?;
        let (count, mut p) = varlong_at(buf, p)?;
        for _ in 0..count {
            let (tid, np) = varlong_at(buf, p)?;
            let tidx = schema.type_idx(tid).ok_or(Error::UnknownType(tid as i64))?;
            let (n, np) = varlong_at(buf, np)?;
            p = np;
            let n = usize::try_from(n).map_err(|_| corrupt("pool entry count", p))?;
            if n > buf.len().saturating_sub(p) {
                return Err(corrupt("pool entry count exceeds the checkpoint", p));
            }
            let index = &mut pools[tidx as usize];
            index.reserve(n);
            for _ in 0..n {
                let (key, vp) = varlong_at(buf, p)?;
                index.insert(key, vp);
                p = schema.layout.skip_value(tidx, buf, vp)?;
            }
        }
    }
    Ok(pools)
}
