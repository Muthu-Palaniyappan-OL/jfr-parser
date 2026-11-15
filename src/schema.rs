//! A parsed, compiled schema shared between chunks.
//!
//! Recordings made by the JVM repeat the same metadata in every chunk.
//! Re-parsing it 500 times for a 200 MB file would be wasted work, so the
//! parsed [`Schema`] is cached by metadata id and re-used when the raw
//! bytes are identical. The comparison is a byte-for-byte `memcmp`, so a
//! recording whose metadata genuinely changes mid-file is still handled
//! correctly.

use crate::error::Result;
use crate::layout::Layout;
use crate::metadata::Metadata;
use crate::reader::varlong_at;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Ids below this are resolved through a flat table instead of a hash map.
const DENSE_LIMIT: i64 = 1 << 16;

/// A chunk's parsed metadata together with its compiled layout.
#[derive(Debug)]
pub struct Schema {
    /// The types declared in the metadata.
    pub meta: Metadata,
    pub(crate) layout: Layout,
    raw: Box<[u8]>,
    dense: Vec<u32>,
    /// Index of `java.lang.String`, used to resolve pooled strings.
    pub string_type: Option<u32>,
}

impl Schema {
    /// `event_bytes` is exactly one metadata event (size field included).
    pub fn parse(event_bytes: &[u8]) -> Result<Schema> {
        let meta = Metadata::parse(event_bytes)?;
        let layout = Layout::new(&meta);
        let max = meta.types.iter().map(|t| t.id).filter(|&i| (0..DENSE_LIMIT).contains(&i)).max();
        let mut dense = vec![u32::MAX; max.map_or(0, |m| m as usize + 1)];
        for t in &meta.types {
            if (0..DENSE_LIMIT).contains(&t.id) && meta.type_idx_by_id(t.id) == Some(t.idx) {
                dense[t.id as usize] = t.idx;
            }
        }
        let string_type = meta.type_idx_by_name("java.lang.String");
        Ok(Schema { meta, layout, raw: event_bytes.into(), dense, string_type })
    }

    /// Type index for a wire type id.
    #[inline]
    pub fn type_idx(&self, id: u64) -> Option<u32> {
        if id < self.dense.len() as u64 {
            let v = self.dense[id as usize];
            return (v != u32::MAX).then_some(v);
        }
        if id < DENSE_LIMIT as u64 {
            return None;
        }
        self.meta.type_idx_by_id(i64::try_from(id).ok()?)
    }
}

/// Thread-safe cache of schemas keyed by metadata id.
#[derive(Default)]
pub(crate) struct SchemaCache {
    map: Mutex<HashMap<(i64, usize), Arc<Schema>>>,
}

impl SchemaCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the schema for the metadata event at the start of `buf`
    /// (which may extend beyond it).
    pub(crate) fn get(&self, buf: &[u8]) -> Result<Arc<Schema>> {
        let (size, p) = varlong_at(buf, 0)?;
        let size = usize::try_from(size).ok().filter(|&s| s <= buf.len() && s > p).ok_or_else(|| {
            crate::error::truncated("metadata event", 0)
        })?;
        let bytes = &buf[..size];
        let key = {
            let mut q = p;
            let mut id = 0u64;
            for i in 0..4 {
                let (v, np) = varlong_at(bytes, q)?;
                q = np;
                if i == 3 {
                    id = v;
                }
            }
            (id as i64, size)
        };
        if let Some(s) = self.map.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            if *s.raw == *bytes {
                return Ok(s.clone());
            }
        }
        let schema = Arc::new(Schema::parse(bytes)?);
        self.map.lock().unwrap_or_else(|e| e.into_inner()).insert(key, schema.clone());
        Ok(schema)
    }
}
