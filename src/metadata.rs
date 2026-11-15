//! The metadata event: the self-describing schema of a chunk.
//!
//! Every chunk carries its own type system. Nothing about JDK events is
//! hard-coded in this crate; JDK events, async-profiler events and custom
//! application events are all decoded purely from what is declared here.
//!
//! Wire layout of the metadata event (all integers are varints):
//! `size, type=0, startTicks, duration, metadataId, stringCount, strings..., root element`
//! where an element is `nameIdx, attrCount, (keyIdx, valueIdx)*, childCount, element*`.
//! The root holds a `metadata` element (the `class` definitions) and a
//! `region` element (locale and time zone information).

use crate::error::{Error, Result, corrupt, truncated};
use crate::reader::{Cursor, RawStr};
use std::collections::HashMap;

/// Maximum element nesting accepted while parsing (real files use 5).
const MAX_DEPTH: u32 = 32;

/// How values of a type are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `boolean`, one byte.
    Bool,
    /// `byte`, one byte.
    Byte,
    /// `char`, a varint.
    Char,
    /// `short`, a varint.
    Short,
    /// `int`, a varint.
    Int,
    /// `long`, a varint.
    Long,
    /// `float`, four raw big-endian bytes.
    Float,
    /// `double`, eight raw big-endian bytes.
    Double,
    /// `java.lang.String`.
    Str,
    /// Any other type: a sequence of its declared fields.
    Struct,
}

/// Physical unit of a numeric field, taken from its annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    /// No unit annotation.
    None,
    /// `@Timespan(TICKS)`: duration in chunk ticks.
    TimespanTicks,
    /// `@Timespan(NANOSECONDS)`.
    TimespanNanos,
    /// `@Timespan(MICROSECONDS)`.
    TimespanMicros,
    /// `@Timespan(MILLISECONDS)`.
    TimespanMillis,
    /// `@Timespan(SECONDS)`.
    TimespanSeconds,
    /// `@Timestamp(TICKS)`: absolute time in chunk ticks.
    TimestampTicks,
    /// `@Timestamp(MILLISECONDS_SINCE_EPOCH)`.
    TimestampEpochMillis,
    /// `@Timestamp(NANOSECONDS_SINCE_EPOCH)`.
    TimestampEpochNanos,
    /// `@DataAmount(BYTES)`.
    Bytes,
    /// `@DataAmount(BITS)`.
    Bits,
    /// `@Percentage`: a fraction where 1.0 is 100%.
    Percentage,
    /// `@MemoryAddress`.
    MemoryAddress,
    /// `@Frequency`: events per second.
    Frequency,
}

/// An annotation attached to a type, field or setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Fully qualified annotation type, e.g. `jdk.jfr.Label`.
    pub name: Box<str>,
    /// Attributes in file order. Array elements appear as `name-0`, `name-1`, ...
    pub attrs: Vec<(Box<str>, Box<str>)>,
}

impl Annotation {
    /// Looks up an attribute by name.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| &**k == key).map(|(_, v)| &**v)
    }
    /// The conventional `value` attribute.
    pub fn value(&self) -> Option<&str> {
        self.attr("value")
    }
}

/// A field of a type or event.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    /// Field name as declared.
    pub name: Box<str>,
    /// Index into [`Metadata::types`].
    pub type_idx: u32,
    /// The value is stored as a reference into the constant pool of `type_idx`.
    pub constant_pool: bool,
    /// Array nesting depth: 0 for a plain value, 1 for `T[]`.
    pub dimension: u32,
    /// All annotations on the field.
    pub annotations: Vec<Annotation>,
    /// Unit derived from the annotations.
    pub unit: Unit,
    /// `@Unsigned`: integral values are to be read as unsigned.
    pub unsigned: bool,
}

impl FieldDesc {
    /// Finds an annotation by its fully qualified type name.
    pub fn annotation(&self, name: &str) -> Option<&Annotation> {
        self.annotations.iter().find(|a| &*a.name == name)
    }
    /// `@Label`, if present.
    pub fn label(&self) -> Option<&str> {
        self.annotation("jdk.jfr.Label").and_then(|a| a.value())
    }
    /// `@Description`, if present.
    pub fn description(&self) -> Option<&str> {
        self.annotation("jdk.jfr.Description").and_then(|a| a.value())
    }
}

/// An event setting (threshold, period, enabled, ...).
#[derive(Debug, Clone)]
pub struct SettingDesc {
    /// Setting name, for example `threshold`.
    pub name: Box<str>,
    /// Index of the setting's type in [`Metadata::types`].
    pub type_idx: u32,
    /// Default value, as text.
    pub default_value: Option<Box<str>>,
    /// Annotations on the setting.
    pub annotations: Vec<Annotation>,
}

/// A type declared in the metadata.
#[derive(Debug, Clone)]
pub struct TypeDesc {
    /// Position in [`Metadata::types`].
    pub idx: u32,
    /// Id used on the wire (event type id, constant pool id).
    pub id: i64,
    /// Fully qualified name, for example `jdk.GarbageCollection`.
    pub name: Box<str>,
    /// Declared super type, for example `jdk.jfr.Event`.
    pub super_type: Option<Box<str>>,
    /// A `simpleType` has a single field and is read as that field's value.
    pub simple_type: bool,
    /// Wire encoding of the type.
    pub kind: Kind,
    /// Declared fields in wire order.
    pub fields: Vec<FieldDesc>,
    /// Declared settings (event types only).
    pub settings: Vec<SettingDesc>,
    /// Annotations on the type.
    pub annotations: Vec<Annotation>,
    /// True for event types (`superType == "jdk.jfr.Event"`).
    pub is_event: bool,
    /// Indexes of well-known event fields, resolved once here.
    pub well_known: WellKnown,
}

/// Indexes of the fields every event may carry.
#[derive(Debug, Clone, Copy, Default)]
pub struct WellKnown {
    /// Index of the `startTime` field.
    pub start_time: Option<u32>,
    /// Index of the `duration` field.
    pub duration: Option<u32>,
    /// Index of the `eventThread` field.
    pub event_thread: Option<u32>,
    /// Index of the `stackTrace` field.
    pub stack_trace: Option<u32>,
}

impl TypeDesc {
    /// Position of the field called `name`.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| &*f.name == name)
    }
    /// Finds an annotation by its fully qualified type name.
    pub fn annotation(&self, name: &str) -> Option<&Annotation> {
        self.annotations.iter().find(|a| &*a.name == name)
    }
    /// `@Label`, if present.
    pub fn label(&self) -> Option<&str> {
        self.annotation("jdk.jfr.Label").and_then(|a| a.value())
    }
    /// `@Description`, if present.
    pub fn description(&self) -> Option<&str> {
        self.annotation("jdk.jfr.Description").and_then(|a| a.value())
    }
    /// `@Category` path, e.g. `["Java Virtual Machine", "GC"]`.
    pub fn category(&self) -> Vec<&str> {
        let Some(a) = self.annotation("jdk.jfr.Category") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(v) = a.attr(&format!("value-{i}")) {
            out.push(v);
            i += 1;
        }
        out
    }
}

/// Parsed metadata of one chunk (shared by all chunks with identical metadata).
#[derive(Debug)]
pub struct Metadata {
    /// The metadata id from the event header; changes when the schema changes.
    pub metadata_id: i64,
    /// Every type declared in the chunk; [`TypeDesc::idx`] is the position here.
    pub types: Vec<TypeDesc>,
    /// Locale of the recording, for example `en_US`.
    pub locale: Option<Box<str>>,
    /// Offset of the recording time zone from GMT, in milliseconds.
    pub gmt_offset_ms: i64,
    /// Daylight saving offset in milliseconds.
    pub dst: i64,
    by_id: HashMap<i64, u32>,
    by_name: HashMap<Box<str>, u32>,
    /// Indexes of event types.
    pub event_types: Vec<u32>,
}

impl Metadata {
    /// Looks up a type by its wire id.
    pub fn type_by_id(&self, id: i64) -> Option<&TypeDesc> {
        self.by_id.get(&id).map(|&i| &self.types[i as usize])
    }
    /// Index of the type with wire id `id`.
    pub fn type_idx_by_id(&self, id: i64) -> Option<u32> {
        self.by_id.get(&id).copied()
    }
    /// Looks up a type by fully qualified name.
    pub fn type_by_name(&self, name: &str) -> Option<&TypeDesc> {
        self.by_name.get(name).map(|&i| &self.types[i as usize])
    }
    /// Index of the type called `name`.
    pub fn type_idx_by_name(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }
    /// Iterates over all event types.
    pub fn events(&self) -> impl Iterator<Item = &TypeDesc> {
        self.event_types.iter().map(|&i| &self.types[i as usize])
    }
}

struct Elem {
    name: u32,
    attrs: Vec<(u32, u32)>,
    children: Vec<Elem>,
}

struct Strings(Vec<Box<str>>);

impl Strings {
    fn get(&self, i: u32) -> &str {
        &self.0[i as usize]
    }
}

fn read_elem(c: &mut Cursor<'_>, nstr: usize, depth: u32) -> Result<Elem> {
    if depth > MAX_DEPTH {
        return Err(corrupt("metadata nesting too deep", c.pos));
    }
    let idx = |c: &mut Cursor<'_>| -> Result<u32> {
        let at = c.pos;
        let v = c.varlong()?;
        if v >= nstr as u64 {
            return Err(corrupt("metadata string index out of range", at));
        }
        Ok(v as u32)
    };
    let name = idx(c)?;
    let n_attrs = c.count("metadata attribute count")?;
    let mut attrs = Vec::with_capacity(n_attrs);
    for _ in 0..n_attrs {
        let k = idx(c)?;
        let v = idx(c)?;
        attrs.push((k, v));
    }
    let n_children = c.count("metadata child count")?;
    let mut children = Vec::with_capacity(n_children);
    for _ in 0..n_children {
        children.push(read_elem(c, nstr, depth + 1)?);
    }
    Ok(Elem { name, attrs, children })
}

fn attr<'s>(s: &'s Strings, e: &Elem, key: &str) -> Option<&'s str> {
    e.attrs.iter().find(|(k, _)| s.get(*k) == key).map(|(_, v)| s.get(*v))
}

fn parse_i64(v: &str, what: &'static str) -> Result<i64> {
    v.trim().parse::<i64>().map_err(|_| corrupt(what, 0))
}

fn kind_for(name: &str) -> Kind {
    match name {
        "boolean" => Kind::Bool,
        "byte" => Kind::Byte,
        "char" => Kind::Char,
        "short" => Kind::Short,
        "int" => Kind::Int,
        "long" => Kind::Long,
        "float" => Kind::Float,
        "double" => Kind::Double,
        "java.lang.String" => Kind::Str,
        _ => Kind::Struct,
    }
}

fn derive_unit(annotations: &[Annotation]) -> (Unit, bool) {
    let mut unit = Unit::None;
    let mut unsigned = false;
    for a in annotations {
        match &*a.name {
            "jdk.jfr.Timespan" => {
                unit = match a.value().unwrap_or("NANOSECONDS") {
                    "TICKS" => Unit::TimespanTicks,
                    "MICROSECONDS" => Unit::TimespanMicros,
                    "MILLISECONDS" => Unit::TimespanMillis,
                    "SECONDS" => Unit::TimespanSeconds,
                    _ => Unit::TimespanNanos,
                }
            }
            "jdk.jfr.Timestamp" => {
                unit = match a.value().unwrap_or("MILLISECONDS_SINCE_EPOCH") {
                    "TICKS" => Unit::TimestampTicks,
                    "NANOSECONDS_SINCE_EPOCH" => Unit::TimestampEpochNanos,
                    _ => Unit::TimestampEpochMillis,
                }
            }
            "jdk.jfr.DataAmount" => {
                unit = if a.value() == Some("BITS") { Unit::Bits } else { Unit::Bytes };
            }
            "jdk.jfr.Percentage" => unit = Unit::Percentage,
            "jdk.jfr.MemoryAddress" => unit = Unit::MemoryAddress,
            "jdk.jfr.Frequency" => unit = Unit::Frequency,
            "jdk.jfr.Unsigned" => unsigned = true,
            _ => {}
        }
    }
    (unit, unsigned)
}

impl Metadata {
    /// Parses a metadata event. `buf` must start at the event's size field
    /// (the chunk's metadata offset) and contain at least the whole event.
    pub fn parse(buf: &[u8]) -> Result<Metadata> {
        let mut c = Cursor::new(buf, 0);
        let size = c.varlong()?;
        let size = usize::try_from(size).map_err(|_| corrupt("metadata size", 0))?;
        if size == 0 || size > buf.len() {
            return Err(truncated("metadata event", 0));
        }
        let mut c = Cursor::new(&buf[..size], c.pos);
        let ty = c.long()?;
        if ty != 0 {
            return Err(corrupt("expected metadata event (type 0)", 0));
        }
        let _start = c.long()?;
        let _duration = c.long()?;
        let metadata_id = c.long()?;
        let nstr = c.count("metadata string count")?;
        let mut pool = Vec::with_capacity(nstr);
        for _ in 0..nstr {
            let s = match c.string()? {
                RawStr::Null => "".into(),
                RawStr::Pool(_) => return Err(corrupt("string pool reference inside metadata", c.pos)),
                other => other.to_str().map(|s| s.into_owned().into_boxed_str()).unwrap_or_default(),
            };
            pool.push(s);
        }
        let strings = Strings(pool);
        let root = read_elem(&mut c, nstr, 0)?;
        build(&strings, &root, metadata_id)
    }
}

fn build(s: &Strings, root: &Elem, metadata_id: i64) -> Result<Metadata> {
    let meta_el = root
        .children
        .iter()
        .find(|e| s.get(e.name) == "metadata")
        .ok_or_else(|| corrupt("metadata element missing", 0))?;

    let class_els: Vec<&Elem> = meta_el.children.iter().filter(|e| s.get(e.name) == "class").collect();

    let mut types: Vec<TypeDesc> = Vec::with_capacity(class_els.len());
    let mut by_id = HashMap::with_capacity(class_els.len());
    let mut by_name = HashMap::with_capacity(class_els.len());
    for (i, e) in class_els.iter().enumerate() {
        let id = parse_i64(attr(s, e, "id").ok_or_else(|| corrupt("class without id", 0))?, "class id")?;
        let name: Box<str> = attr(s, e, "name").unwrap_or("").into();
        let super_type: Option<Box<str>> = attr(s, e, "superType").map(Into::into);
        let simple_type = attr(s, e, "simpleType").is_some_and(|v| v != "false");
        let is_event = super_type.as_deref() == Some("jdk.jfr.Event");
        by_id.insert(id, i as u32);
        by_name.entry(name.clone()).or_insert(i as u32);
        types.push(TypeDesc {
            idx: i as u32,
            id,
            kind: kind_for(&name),
            name,
            super_type,
            simple_type,
            fields: Vec::new(),
            settings: Vec::new(),
            annotations: Vec::new(),
            is_event,
            well_known: WellKnown::default(),
        });
    }

    let lookup = |id_str: &str| -> Result<u32> {
        let id = parse_i64(id_str, "type reference")?;
        by_id.get(&id).copied().ok_or(Error::UnknownType(id))
    };

    let make_annotations = |parent: &Elem| -> Result<Vec<Annotation>> {
        let mut out = Vec::new();
        for a in parent.children.iter().filter(|c| s.get(c.name) == "annotation") {
            let ty = lookup(attr(s, a, "class").ok_or_else(|| corrupt("annotation without class", 0))?)?;
            let attrs = a
                .attrs
                .iter()
                .filter(|(k, _)| s.get(*k) != "class")
                .map(|(k, v)| (s.get(*k).into(), s.get(*v).into()))
                .collect();
            out.push(Annotation { name: types[ty as usize].name.clone(), attrs });
        }
        Ok(out)
    };

    let mut filled: Vec<(Vec<FieldDesc>, Vec<SettingDesc>, Vec<Annotation>)> = Vec::with_capacity(types.len());
    for e in &class_els {
        let mut fields = Vec::new();
        let mut settings = Vec::new();
        for ch in &e.children {
            match s.get(ch.name) {
                "field" => {
                    let type_idx =
                        lookup(attr(s, ch, "class").ok_or_else(|| corrupt("field without class", 0))?)?;
                    let dimension = match attr(s, ch, "dimension") {
                        Some(d) => u32::try_from(parse_i64(d, "field dimension")?)
                            .map_err(|_| corrupt("negative field dimension", 0))?,
                        None => 0,
                    };
                    let annotations = make_annotations(ch)?;
                    let (unit, unsigned) = derive_unit(&annotations);
                    fields.push(FieldDesc {
                        name: attr(s, ch, "name").unwrap_or("").into(),
                        type_idx,
                        constant_pool: attr(s, ch, "constantPool").is_some_and(|v| v != "false"),
                        dimension,
                        annotations,
                        unit,
                        unsigned,
                    });
                }
                "setting" => {
                    let type_idx =
                        lookup(attr(s, ch, "class").ok_or_else(|| corrupt("setting without class", 0))?)?;
                    settings.push(SettingDesc {
                        name: attr(s, ch, "name").unwrap_or("").into(),
                        type_idx,
                        default_value: attr(s, ch, "defaultValue").map(Into::into),
                        annotations: make_annotations(ch)?,
                    });
                }
                _ => {}
            }
        }
        filled.push((fields, settings, make_annotations(e)?));
    }

    let mut event_types = Vec::new();
    for (t, (fields, settings, annotations)) in types.iter_mut().zip(filled) {
        let pos = |n: &str| fields.iter().position(|f| &*f.name == n).map(|p| p as u32);
        t.well_known = WellKnown {
            start_time: pos("startTime"),
            duration: pos("duration"),
            event_thread: pos("eventThread"),
            stack_trace: pos("stackTrace"),
        };
        t.fields = fields;
        t.settings = settings;
        t.annotations = annotations;
        if t.is_event {
            event_types.push(t.idx);
        }
    }

    let (mut locale, mut gmt_offset_ms, mut dst) = (None, 0, 0);
    if let Some(region) = root.children.iter().find(|e| s.get(e.name) == "region") {
        locale = attr(s, region, "locale").map(Into::into);
        gmt_offset_ms = attr(s, region, "gmtOffset").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        dst = attr(s, region, "dst").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    }

    Ok(Metadata { metadata_id, types, locale, gmt_offset_ms, dst, by_id, by_name, event_types })
}
