//! Lazily decoded values.
//!
//! Nothing here allocates a tree of objects. An [`Obj`] is a (chunk, type,
//! byte offset) triple; reading a field walks the compiled skip program to
//! that field and decodes just it. Strings borrow from the mapped file when
//! the stored encoding allows it. Constant-pool references are followed on
//! access with one hash probe.

use crate::chunk::Chunk;
use crate::error::{Error, Result, corrupt};
use crate::layout::FieldLayout;
use crate::metadata::{FieldDesc, Kind, TypeDesc};
use crate::reader::{RawStr, f32_at, f64_at, string_at, u8_at, varlong_at};
use std::borrow::Cow;
use std::fmt;

/// Bound on reference chains followed while resolving a single value.
const MAX_RESOLVE_DEPTH: u32 = 16;

/// An unresolved constant-pool reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ref {
    /// Index of the referenced type in the schema.
    pub type_idx: u32,
    /// Id within that type's constant pool.
    pub id: u64,
}

/// A decoded value.
#[derive(Clone)]
pub enum Value<'c> {
    /// A null string, or a reference whose target is absent (id 0 or not in any pool).
    Null,
    /// `boolean`.
    Bool(bool),
    /// `byte`.
    Byte(i8),
    /// `char`, as a UTF-16 code unit.
    Char(u16),
    /// `short`.
    Short(i16),
    /// `int`.
    Int(i32),
    /// `long`.
    Long(i64),
    /// `float`.
    Float(f32),
    /// `double`.
    Double(f64),
    /// A string, borrowed from the file whenever possible.
    Str(Cow<'c, str>),
    /// A constant-pool reference that was deliberately not followed.
    Ref(Ref),
    /// A struct: an inline value or a resolved constant-pool entry.
    Obj(Obj<'c>),
    /// An array of any element type.
    Array(Array<'c>),
}

impl<'c> Value<'c> {
    /// Integral values (and booleans, chars) widened to `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        Some(match *self {
            Value::Bool(b) => b as i64,
            Value::Byte(v) => v as i64,
            Value::Char(v) => v as i64,
            Value::Short(v) => v as i64,
            Value::Int(v) => v as i64,
            Value::Long(v) => v,
            _ => return None,
        })
    }
    /// Any numeric value as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Float(v) => Some(v as f64),
            Value::Double(v) => Some(v),
            _ => self.as_i64().map(|v| v as f64),
        }
    }
    /// The value if it is a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }
    /// The string, if the value is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// The struct, if the value is one.
    pub fn as_obj(&self) -> Option<Obj<'c>> {
        match self {
            Value::Obj(o) => Some(*o),
            _ => None,
        }
    }
    /// The array, if the value is one.
    pub fn as_array(&self) -> Option<Array<'c>> {
        match self {
            Value::Array(a) => Some(*a),
            _ => None,
        }
    }
    /// True for `Value::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    fn fmt_depth(&self, f: &mut fmt::Formatter<'_>, depth: u32) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Byte(v) => write!(f, "{v}"),
            Value::Char(v) => match char::from_u32(*v as u32) {
                Some(c) => write!(f, "'{c}'"),
                None => write!(f, "'\\u{v:04x}'"),
            },
            Value::Short(v) => write!(f, "{v}"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Long(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::Double(v) => write!(f, "{v}"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Ref(r) => write!(f, "ref({}, {})", r.type_idx, r.id),
            Value::Obj(o) => o.fmt_depth(f, depth),
            Value::Array(a) => {
                f.write_str("[")?;
                for (i, v) in a.iter().enumerate() {
                    if i == 16 {
                        write!(f, ", … {} more", a.len() - 16)?;
                        break;
                    }
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    match v {
                        Ok(v) => v.fmt_depth(f, depth + 1)?,
                        Err(e) => write!(f, "<{e}>")?,
                    }
                }
                f.write_str("]")
            }
        }
    }
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_depth(f, 0)
    }
}

impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => f.write_str(s),
            other => other.fmt_depth(f, 0),
        }
    }
}

/// A struct value located in a chunk.
#[derive(Clone, Copy)]
pub struct Obj<'c> {
    pub(crate) chunk: &'c Chunk<'c>,
    pub(crate) type_idx: u32,
    /// Offset of the first field within the chunk.
    pub(crate) pos: usize,
    /// Reads are confined to `chunk.data[..limit]`.
    pub(crate) limit: usize,
}

impl<'c> Obj<'c> {
    /// The chunk this value lives in.
    pub fn chunk(&self) -> &'c Chunk<'c> {
        self.chunk
    }

    /// Index of the value's type in the chunk's schema.
    pub fn type_idx(&self) -> u32 {
        self.type_idx
    }

    /// The value's type description.
    pub fn type_desc(&self) -> &'c TypeDesc {
        &self.chunk.schema.meta.types[self.type_idx as usize]
    }

    /// The value's fully qualified type name.
    pub fn type_name(&self) -> &'c str {
        &self.type_desc().name
    }

    /// Declared fields of the type, in wire order.
    pub fn field_descs(&self) -> &'c [FieldDesc] {
        &self.type_desc().fields
    }

    fn field_pos(&self, idx: usize) -> Result<usize> {
        let buf = &self.chunk.data[..self.limit];
        self.chunk.schema.layout.field_pos(self.type_idx, idx, buf, self.pos)
    }

    /// Reads field `idx`, following constant-pool references.
    pub fn get_idx(&self, idx: usize) -> Result<Value<'c>> {
        self.read(idx, true)
    }

    /// Reads field `idx` without following a constant-pool reference.
    pub fn get_raw_idx(&self, idx: usize) -> Result<Value<'c>> {
        self.read(idx, false)
    }

    fn read(&self, idx: usize, resolve: bool) -> Result<Value<'c>> {
        let tl = &self.chunk.schema.layout.types[self.type_idx as usize];
        let fl = tl.fields.get(idx).ok_or_else(|| self.no_field(&idx.to_string()))?;
        let pos = self.field_pos(idx)?;
        self.chunk.read_field(fl, self.limit, pos, resolve, 0).map(|(v, _)| v)
    }

    fn no_field(&self, name: &str) -> Error {
        Error::NoSuchField { type_name: self.type_name().to_string(), field: name.to_string() }
    }

    /// Reads a field by name, following constant-pool references.
    pub fn get(&self, name: &str) -> Result<Value<'c>> {
        let idx = self.type_desc().field_index(name).ok_or_else(|| self.no_field(name))?;
        self.get_idx(idx)
    }

    /// Reads a field by name without following a constant-pool reference.
    pub fn get_raw(&self, name: &str) -> Result<Value<'c>> {
        let idx = self.type_desc().field_index(name).ok_or_else(|| self.no_field(name))?;
        self.get_raw_idx(idx)
    }

    /// Follows a dotted path such as `eventThread.javaName` or
    /// `stackTrace.frames.0.method.name`. Numeric segments index arrays.
    /// A null along the way yields `Value::Null`.
    ///
    /// Single-field `simpleType`s (for example `jdk.types.ThreadState`) are
    /// read as their inner value, so `state` and `state.name` are equivalent.
    pub fn path(&self, path: &str) -> Result<Value<'c>> {
        let mut cur = Value::Obj(*self);
        let mut reduced: Option<&str> = None;
        for seg in path.split('.') {
            cur = match cur {
                Value::Obj(o) => {
                    let v = o.get(seg)?;
                    reduced = o.reduced_field_name(seg);
                    v
                }
                Value::Array(a) => {
                    let i: usize = seg.parse().map_err(|_| Error::Query(format!("`{seg}` is not an array index")))?;
                    reduced = None;
                    match a.get(i) {
                        Some(v) => v?,
                        None => Value::Null,
                    }
                }
                Value::Null => return Ok(Value::Null),
                other if reduced == Some(seg) => {
                    reduced = None;
                    other
                }
                _ => return Err(Error::Query(format!("cannot read `{seg}` from a scalar"))),
            };
        }
        Ok(cur)
    }

    fn reduced_field_name(&self, field: &str) -> Option<&'c str> {
        let fd = &self.type_desc().fields[self.type_desc().field_index(field)?];
        let tl = &self.chunk.schema.layout.types[fd.type_idx as usize];
        tl.reduced?;
        let inner = &self.chunk.schema.meta.types[fd.type_idx as usize];
        inner.fields.first().map(|f| &*f.name)
    }

    /// Iterates over `(descriptor, value)` for every field in one pass.
    pub fn fields(&self) -> ObjFields<'c> {
        ObjFields { obj: *self, idx: 0, pos: self.pos }
    }

    fn fmt_depth(&self, f: &mut fmt::Formatter<'_>, depth: u32) -> fmt::Result {
        write!(f, "{}", self.type_name().rsplit('.').next().unwrap_or(""))?;
        if depth >= 3 {
            return f.write_str("{…}");
        }
        f.write_str(" {")?;
        for (i, (fd, v)) in self.fields().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}=", fd.name)?;
            match v {
                Ok(v) => v.fmt_depth(f, depth + 1)?,
                Err(e) => write!(f, "<{e}>")?,
            }
        }
        f.write_str("}")
    }
}

impl fmt::Debug for Obj<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_depth(f, 0)
    }
}

/// Sequential field reader returned by [`Obj::fields`].
pub struct ObjFields<'c> {
    obj: Obj<'c>,
    idx: usize,
    pos: usize,
}

impl<'c> Iterator for ObjFields<'c> {
    type Item = (&'c FieldDesc, Result<Value<'c>>);
    fn next(&mut self) -> Option<Self::Item> {
        let c = self.obj.chunk;
        let fd = self.obj.type_desc().fields.get(self.idx)?;
        let fl = &c.schema.layout.types[self.obj.type_idx as usize].fields[self.idx];
        self.idx += 1;
        match c.read_field(fl, self.obj.limit, self.pos, true, 0) {
            Ok((v, np)) => {
                self.pos = np;
                Some((fd, Ok(v)))
            }
            Err(e) => {
                self.idx = usize::MAX;
                Some((fd, Err(e)))
            }
        }
    }
}

/// An array value.
#[derive(Clone, Copy)]
pub struct Array<'c> {
    chunk: &'c Chunk<'c>,
    /// Layout of the array field itself (`depth >= 1`).
    field: FieldLayout,
    count: usize,
    first: usize,
    limit: usize,
}

impl<'c> Array<'c> {
    /// Number of elements.
    pub fn len(&self) -> usize {
        self.count
    }
    /// True if the array has no elements.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The raw bytes when this is a one-dimensional `byte[]`.
    pub fn as_bytes(&self) -> Option<&'c [u8]> {
        if self.field.depth == 1 && self.field.kind == Kind::Byte && !self.field.is_ref {
            self.chunk.data[..self.limit].get(self.first..self.first.checked_add(self.count)?)
        } else {
            None
        }
    }

    /// Iterates over the elements in order, decoding each as it is reached.
    pub fn iter(&self) -> ArrayIter<'c> {
        ArrayIter { arr: *self, i: 0, pos: self.first }
    }

    /// Element `i`. Elements are variable-sized, so this walks from the start.
    pub fn get(&self, i: usize) -> Option<Result<Value<'c>>> {
        self.iter().nth(i)
    }
}

/// Iterator over the elements of an [`Array`].
pub struct ArrayIter<'c> {
    arr: Array<'c>,
    i: usize,
    pos: usize,
}

impl<'c> Iterator for ArrayIter<'c> {
    type Item = Result<Value<'c>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.arr.count {
            return None;
        }
        let el = FieldLayout { depth: self.arr.field.depth - 1, ..self.arr.field };
        match self.arr.chunk.read_field(&el, self.arr.limit, self.pos, true, 0) {
            Ok((v, np)) => {
                self.i += 1;
                self.pos = np;
                Some(Ok(v))
            }
            Err(e) => {
                self.i = usize::MAX;
                Some(Err(e))
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.arr.count.saturating_sub(self.i);
        (0, Some(n))
    }
}

impl<'c> Chunk<'c> {
    /// Decodes one field value at `pos` and returns the position after it.
    pub(crate) fn read_field(
        &'c self,
        f: &FieldLayout,
        limit: usize,
        pos: usize,
        resolve: bool,
        depth: u32,
    ) -> Result<(Value<'c>, usize)> {
        let buf: &'c [u8] = &self.data[..limit];
        let layout = &self.schema.layout;
        if f.depth > 0 {
            let (count, first) = varlong_at(buf, pos)?;
            let end = layout.skip_array(f.elem, f.depth, buf, pos, 0)?;
            let count = usize::try_from(count).map_err(|_| corrupt("array length", pos))?;
            return Ok((Value::Array(Array { chunk: self, field: *f, count, first, limit }), end));
        }
        if f.is_ref {
            let (id, p) = varlong_at(buf, pos)?;
            let v = if resolve {
                self.resolve(f.type_idx, id, depth)?
            } else {
                Value::Ref(Ref { type_idx: f.type_idx, id })
            };
            return Ok((v, p));
        }
        Ok(match f.kind {
            Kind::Bool => (Value::Bool(u8_at(buf, pos)? != 0), pos + 1),
            Kind::Byte => (Value::Byte(u8_at(buf, pos)? as i8), pos + 1),
            Kind::Char => {
                let (v, p) = varlong_at(buf, pos)?;
                (Value::Char(v as u16), p)
            }
            Kind::Short => {
                let (v, p) = varlong_at(buf, pos)?;
                (if f.unsigned { Value::Int(v as u16 as i32) } else { Value::Short(v as i16) }, p)
            }
            Kind::Int => {
                let (v, p) = varlong_at(buf, pos)?;
                (if f.unsigned { Value::Long(v as u32 as i64) } else { Value::Int(v as i32) }, p)
            }
            Kind::Long => {
                let (v, p) = varlong_at(buf, pos)?;
                (Value::Long(v as i64), p)
            }
            Kind::Float => (Value::Float(f32_at(buf, pos)?), pos + 4),
            Kind::Double => (Value::Double(f64_at(buf, pos)?), pos + 8),
            Kind::Str => {
                let (raw, p) = string_at(buf, pos)?;
                (self.string_value(raw, depth)?, p)
            }
            Kind::Struct => {
                let tl = &layout.types[f.type_idx as usize];
                if let Some(r) = tl.reduced {
                    if depth >= MAX_RESOLVE_DEPTH {
                        return Err(corrupt("reference chain too deep", pos));
                    }
                    return self.read_field(&r, limit, pos, resolve, depth + 1);
                }
                let end = layout.skip_value(f.type_idx, buf, pos)?;
                (Value::Obj(Obj { chunk: self, type_idx: f.type_idx, pos, limit }), end)
            }
        })
    }

    fn string_value(&'c self, raw: RawStr<'c>, depth: u32) -> Result<Value<'c>> {
        match raw {
            RawStr::Null => Ok(Value::Null),
            RawStr::Pool(id) => {
                if depth >= MAX_RESOLVE_DEPTH {
                    return Err(corrupt("string reference chain too deep", 0));
                }
                let Some(st) = self.schema.string_type else { return Ok(Value::Null) };
                let Some(off) = self.pool_offset(st, id)? else { return Ok(Value::Null) };
                let (inner, _) = string_at(self.data, off)?;
                self.string_value(inner, depth + 1)
            }
            other => Ok(other.to_str().map_or(Value::Null, Value::Str)),
        }
    }

    /// Resolves a constant-pool reference to its value.
    /// Absent targets (including the conventional null id 0) give `Value::Null`.
    pub(crate) fn resolve(&'c self, type_idx: u32, id: u64, depth: u32) -> Result<Value<'c>> {
        if depth >= MAX_RESOLVE_DEPTH {
            return Err(corrupt("reference chain too deep", 0));
        }
        let Some(off) = self.pool_offset(type_idx, id)? else { return Ok(Value::Null) };
        let tl = &self.schema.layout.types[type_idx as usize];
        let limit = self.data.len();
        match tl.kind {
            Kind::Struct => match tl.reduced {
                Some(r) => self.read_field(&r, limit, off, true, depth + 1).map(|(v, _)| v),
                None => Ok(Value::Obj(Obj { chunk: self, type_idx, pos: off, limit })),
            },
            k => self
                .read_field(&FieldLayout::scalar(k, type_idx), limit, off, true, depth + 1)
                .map(|(v, _)| v),
        }
    }

    /// Resolves a [`Ref`] obtained from a raw read.
    pub fn resolve_ref(&'c self, r: Ref) -> Result<Value<'c>> {
        self.resolve(r.type_idx, r.id, 0)
    }

    /// Looks up a constant-pool entry by type name and id.
    pub fn pool_get(&'c self, type_name: &str, id: u64) -> Result<Value<'c>> {
        let t = self
            .schema
            .meta
            .type_idx_by_name(type_name)
            .ok_or_else(|| Error::NoSuchType(type_name.to_string()))?;
        self.resolve(t, id, 0)
    }

    /// Iterates over all `(id, value)` of one constant pool.
    pub fn pool_entries(&'c self, type_name: &str) -> Result<Vec<(u64, Value<'c>)>> {
        let t = self
            .schema
            .meta
            .type_idx_by_name(type_name)
            .ok_or_else(|| Error::NoSuchType(type_name.to_string()))?;
        let mut v: Vec<(u64, usize)> = self.pools()?[t as usize].iter().collect();
        v.sort_unstable_by_key(|e| e.1);
        v.into_iter().map(|(id, _)| Ok((id, self.resolve(t, id, 0)?))).collect()
    }
}
