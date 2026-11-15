//! Compiles the metadata into "skip programs".
//!
//! Most of the work in a parser that only needs a few fields is *skipping*
//! the rest. A naive reader walks every field of every event. Here each type
//! is compiled once per distinct metadata into a short list of operations in
//! which adjacent fields of the same wire class are merged:
//!
//! * `Varints(n)`: `n` consecutive varint fields (ints, longs, shorts, chars,
//!   constant-pool references). Skipped with [`skip_varints`], which finds
//!   the end with a few bit operations instead of looping per byte.
//! * `Bytes(n)`: `n` bytes of fixed-size data (booleans, bytes, floats, doubles).
//! * `Str`, `Struct`, `Array`: variable-size values handled individually.
//!
//! A typical event such as `jdk.ExecutionSample` therefore compiles to one or
//! two operations instead of one per field.

use crate::error::{Result, corrupt, truncated};
use crate::metadata::{Kind, Metadata};
use crate::reader::{skip_string, skip_varints, varlong_at};

/// Maximum nesting of inline (non constant-pool) structs while skipping.
/// Guards against self-referential schemas in malicious files.
const MAX_STRUCT_DEPTH: u32 = 64;

/// Wire class of a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elem {
    /// A record made only of `n` varints (an int, long, short, char or
    /// constant-pool reference is `Varints(1)`; a struct of four varint
    /// fields such as `jdk.types.StackFrame` is `Varints(4)`).
    Varints(u32),
    /// A record made only of `n` bytes of fixed-width data.
    Fixed(u32),
    /// An inline string.
    Str,
    /// An inline struct with variable-size content, by type index.
    Struct(u32),
}

/// One step of a skip program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Varints(u32),
    Bytes(u32),
    Str,
    Struct(u32),
    /// A varint element count followed by that many `elem`s, nested `depth` levels deep.
    Array { elem: Elem, depth: u8 },
}

/// Everything the decoder needs to know about one field.
#[derive(Debug, Clone, Copy)]
pub struct FieldLayout {
    pub elem: Elem,
    /// Array nesting depth, 0 for a scalar.
    pub depth: u8,
    /// Primitive kind of the element (or `Struct`/`Str`).
    pub kind: Kind,
    /// The wire value is a constant-pool reference to `type_idx`.
    pub is_ref: bool,
    /// Index of the field's declared type.
    pub type_idx: u32,
    /// `@Unsigned`: byte, short and int values are widened without sign extension.
    pub unsigned: bool,
}

impl FieldLayout {
    /// Layout of a plain (non-array, non-reference) value of a scalar kind.
    pub fn scalar(kind: Kind, type_idx: u32) -> FieldLayout {
        let elem = match kind {
            Kind::Bool | Kind::Byte => Elem::Fixed(1),
            Kind::Char | Kind::Short | Kind::Int | Kind::Long => Elem::Varints(1),
            Kind::Float => Elem::Fixed(4),
            Kind::Double => Elem::Fixed(8),
            Kind::Str => Elem::Str,
            Kind::Struct => Elem::Struct(type_idx),
        };
        FieldLayout { elem, depth: 0, kind, is_ref: false, type_idx, unsigned: false }
    }
}

/// Compiled form of one type.
#[derive(Debug, Clone)]
pub struct TypeLayout {
    pub kind: Kind,
    pub fields: Vec<FieldLayout>,
    /// For a `simpleType` with exactly one field the JDK reads the value as
    /// that field directly (so `jdk.types.Symbol` is just a string). This is
    /// that field.
    pub reduced: Option<FieldLayout>,
    /// Skip program for a whole value of this type.
    pub ops: Vec<Op>,
    /// For each field `i` (and one extra entry for "end"): the index into
    /// `ops` where the field starts, and how many units into that op.
    field_start: Vec<(u32, u32)>,
}

/// The compiled schema of one metadata.
#[derive(Debug)]
pub struct Layout {
    pub types: Vec<TypeLayout>,
}

/// A struct that can be skipped as a single run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pure {
    Varints(u32),
    Fixed(u32),
}

fn field_elem(kind: Kind, is_ref: bool, type_idx: u32, pure: &[Option<Pure>]) -> Elem {
    if is_ref {
        return Elem::Varints(1);
    }
    match kind {
        Kind::Bool | Kind::Byte => Elem::Fixed(1),
        Kind::Char | Kind::Short | Kind::Int | Kind::Long => Elem::Varints(1),
        Kind::Float => Elem::Fixed(4),
        Kind::Double => Elem::Fixed(8),
        Kind::Str => Elem::Str,
        Kind::Struct => match pure[type_idx as usize] {
            Some(Pure::Varints(n)) => Elem::Varints(n),
            Some(Pure::Fixed(n)) => Elem::Fixed(n),
            None => Elem::Struct(type_idx),
        },
    }
}

/// Decides, for every type, whether its inline encoding is a pure run.
/// Memoised depth-first search; a type that is reached again while it is
/// still being analysed (a cyclic inline definition) is not flattened.
fn compute_pure(meta: &Metadata) -> Vec<Option<Pure>> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        New,
        Busy,
        Done(Option<Pure>),
    }
    fn go(meta: &Metadata, t: usize, st: &mut Vec<St>) -> Option<Pure> {
        match st[t] {
            St::Done(r) => return r,
            St::Busy => return None,
            St::New => {}
        }
        st[t] = St::Busy;
        let ty = &meta.types[t];
        let r = match ty.kind {
            Kind::Bool | Kind::Byte => Some(Pure::Fixed(1)),
            Kind::Float => Some(Pure::Fixed(4)),
            Kind::Double => Some(Pure::Fixed(8)),
            Kind::Char | Kind::Short | Kind::Int | Kind::Long => Some(Pure::Varints(1)),
            Kind::Str => None,
            Kind::Struct => {
                let (mut v, mut f) = (0u32, 0u32);
                let mut ok = true;
                for fd in &ty.fields {
                    if fd.dimension > 0 {
                        ok = false;
                        break;
                    }
                    if fd.constant_pool {
                        v = v.saturating_add(1);
                        continue;
                    }
                    match go(meta, fd.type_idx as usize, st) {
                        Some(Pure::Varints(n)) => v = v.saturating_add(n),
                        Some(Pure::Fixed(n)) => f = f.saturating_add(n),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                match (ok, v, f) {
                    (true, v, 0) => Some(Pure::Varints(v)),
                    (true, 0, f) => Some(Pure::Fixed(f)),
                    _ => None, // mixed varint + fixed layouts stay as structs
                }
            }
        };
        st[t] = St::Done(r);
        r
    }
    let mut st = vec![St::New; meta.types.len()];
    (0..meta.types.len()).map(|t| go(meta, t, &mut st)).collect()
}

fn op_for(f: &FieldLayout) -> Op {
    if f.depth > 0 {
        return Op::Array { elem: f.elem, depth: f.depth };
    }
    match f.elem {
        Elem::Varints(n) => Op::Varints(n),
        Elem::Fixed(n) => Op::Bytes(n),
        Elem::Str => Op::Str,
        Elem::Struct(t) => Op::Struct(t),
    }
}

/// Appends `op`, merging into the previous op when both are the same run kind.
/// Returns (index of the op holding it, units already in that op before it).
fn push_op(ops: &mut Vec<Op>, op: Op) -> (u32, u32) {
    match (ops.last_mut(), op) {
        (Some(Op::Varints(n)), Op::Varints(m)) => {
            let before = *n;
            *n += m;
            ((ops.len() - 1) as u32, before)
        }
        (Some(Op::Bytes(n)), Op::Bytes(m)) => {
            let before = *n;
            *n += m;
            ((ops.len() - 1) as u32, before)
        }
        _ => {
            ops.push(op);
            ((ops.len() - 1) as u32, 0)
        }
    }
}

/// Compiles a list of fields into a coalesced op list plus per-field starts.
fn compile_fields(fields: &[FieldLayout]) -> (Vec<Op>, Vec<(u32, u32)>) {
    let mut ops = Vec::new();
    let mut starts = Vec::with_capacity(fields.len() + 1);
    for f in fields {
        starts.push(push_op(&mut ops, op_for(f)));
    }
    starts.push((ops.len() as u32, 0));
    (ops, starts)
}

impl Layout {
    pub fn new(meta: &Metadata) -> Layout {
        let pure = compute_pure(meta);
        let types = meta
            .types
            .iter()
            .map(|t| {
                let fields: Vec<FieldLayout> = t
                    .fields
                    .iter()
                    .map(|f| {
                        let ft = &meta.types[f.type_idx as usize];
                        FieldLayout {
                            elem: field_elem(ft.kind, f.constant_pool, f.type_idx, &pure),
                            depth: f.dimension.min(u8::MAX as u32) as u8,
                            kind: ft.kind,
                            is_ref: f.constant_pool,
                            type_idx: f.type_idx,
                            unsigned: f.unsigned,
                        }
                    })
                    .collect();
                match t.kind {
                    Kind::Struct => {
                        let (ops, field_start) = compile_fields(&fields);
                        let reduced = (t.simple_type && fields.len() == 1).then(|| fields[0]);
                        TypeLayout { kind: t.kind, fields, reduced, ops, field_start }
                    }
                    k => {
                        let op = op_for(&FieldLayout {
                            elem: field_elem(k, false, t.idx, &pure),
                            depth: 0,
                            kind: k,
                            is_ref: false,
                            type_idx: t.idx,
                            unsigned: false,
                        });
                        TypeLayout { kind: k, fields: Vec::new(), reduced: None, ops: vec![op], field_start: vec![(1, 0)] }
                    }
                }
            })
            .collect();
        Layout { types }
    }

    /// Skips one complete value of `type_idx` starting at `pos`.
    #[inline]
    pub fn skip_value(&self, type_idx: u32, buf: &[u8], pos: usize) -> Result<usize> {
        self.exec(&self.types[type_idx as usize].ops, buf, pos, 0)
    }

    /// Returns the position where field `field` of a struct value of
    /// `type_idx` begins, given that the value begins at `pos`.
    pub fn field_pos(&self, type_idx: u32, field: usize, buf: &[u8], pos: usize) -> Result<usize> {
        let tl = &self.types[type_idx as usize];
        let &(op_idx, units) = tl
            .field_start
            .get(field)
            .ok_or_else(|| corrupt("field index out of range", pos))?;
        let mut p = self.exec(&tl.ops[..op_idx as usize], buf, pos, 0)?;
        if units > 0 {
            p = match tl.ops[op_idx as usize] {
                Op::Varints(_) => skip_varints(buf, p, units as usize)?,
                Op::Bytes(_) => advance(buf, p, units as usize)?,
                _ => p,
            };
        }
        Ok(p)
    }

    /// Runs a skip program.
    #[inline]
    pub fn exec(&self, ops: &[Op], buf: &[u8], mut pos: usize, depth: u32) -> Result<usize> {
        for op in ops {
            pos = match *op {
                Op::Varints(n) => skip_varints(buf, pos, n as usize)?,
                Op::Bytes(n) => advance(buf, pos, n as usize)?,
                Op::Str => skip_string(buf, pos)?,
                Op::Struct(t) => self.skip_struct(t, buf, pos, depth)?,
                Op::Array { elem, depth: d } => self.skip_array(elem, d, buf, pos, depth)?,
            };
        }
        Ok(pos)
    }

    fn skip_struct(&self, t: u32, buf: &[u8], pos: usize, depth: u32) -> Result<usize> {
        if depth >= MAX_STRUCT_DEPTH {
            return Err(corrupt("inline struct nesting too deep", pos));
        }
        self.exec(&self.types[t as usize].ops, buf, pos, depth + 1)
    }

    pub(crate) fn skip_elem(&self, e: Elem, buf: &[u8], pos: usize, depth: u32) -> Result<usize> {
        match e {
            Elem::Varints(n) => skip_varints(buf, pos, n as usize),
            Elem::Fixed(n) => advance(buf, pos, n as usize),
            Elem::Str => skip_string(buf, pos),
            Elem::Struct(t) => self.skip_struct(t, buf, pos, depth),
        }
    }

    /// Skips a (possibly nested) array: count, then elements.
    pub fn skip_array(&self, elem: Elem, array_depth: u8, buf: &[u8], pos: usize, depth: u32) -> Result<usize> {
        let (count, mut p) = varlong_at(buf, pos)?;
        let count = usize::try_from(count).map_err(|_| corrupt("array length", pos))?;
        if array_depth > 1 {
            for _ in 0..count {
                p = self.skip_array(elem, array_depth - 1, buf, p, depth)?;
            }
            return Ok(p);
        }
        let remaining = buf.len().saturating_sub(p);
        match elem {
            Elem::Varints(n) => {
                // Every varint takes at least one byte, which bounds `total`
                // by the bytes that remain before anything is scanned.
                let total = count.checked_mul(n as usize).filter(|&c| c <= remaining);
                match total {
                    Some(total) => skip_varints(buf, p, total),
                    None => Err(truncated("array elements", p)),
                }
            }
            Elem::Fixed(n) => {
                let bytes = count.checked_mul(n as usize).filter(|&b| b <= remaining);
                bytes.map(|b| p + b).ok_or_else(|| truncated("array elements", p))
            }
            Elem::Str | Elem::Struct(_) => {
                // Elements may legitimately occupy zero bytes (an empty
                // struct), so stop looping once an element consumes nothing.
                for _ in 0..count {
                    let np = self.skip_elem(elem, buf, p, depth)?;
                    if np == p {
                        break;
                    }
                    p = np;
                }
                Ok(p)
            }
        }
    }
}

#[inline(always)]
fn advance(buf: &[u8], pos: usize, n: usize) -> Result<usize> {
    match pos.checked_add(n) {
        Some(e) if e <= buf.len() => Ok(e),
        _ => Err(truncated("fixed-size field", pos)),
    }
}

/// A precompiled single-pass read of selected fields of one event type.
///
/// Fields not selected are skipped with the coalesced skip programs; the
/// selected ones are handed to a decoder in order. Build once per type, run
/// on every event.
#[derive(Debug, Clone)]
pub struct Projection {
    pub(crate) type_idx: u32,
    pub(crate) steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub enum Step {
    Skip(Vec<Op>),
    /// Decode field `field` into output slot `slot`.
    Read { field: u32, slot: u32 },
    /// Duplicate selection: copy an already decoded slot.
    Copy { from: u32, slot: u32 },
}

impl Projection {
    /// `wanted` lists field indexes in the order the caller wants them in
    /// the output slots. Duplicates are allowed and cost nothing extra (the
    /// later slot copies the earlier one). Out-of-range indexes are ignored
    /// and leave their slot unset.
    pub fn new(layout: &Layout, type_idx: u32, wanted: &[u32]) -> Projection {
        let tl = &layout.types[type_idx as usize];
        let mut order: Vec<(u32, u32)> = wanted.iter().enumerate().map(|(slot, &f)| (f, slot as u32)).collect();
        order.sort();
        let mut steps = Vec::new();
        let mut next_field = 0usize;
        let mut last: Option<(u32, u32)> = None;
        for (field, slot) in order {
            let f = field as usize;
            if f >= tl.fields.len() {
                continue;
            }
            if let Some((lf, ls)) = last {
                if lf == field {
                    steps.push(Step::Copy { from: ls, slot });
                    continue;
                }
            }
            if f > next_field {
                let (ops, _) = compile_fields(&tl.fields[next_field..f]);
                if !ops.is_empty() {
                    steps.push(Step::Skip(ops));
                }
            }
            steps.push(Step::Read { field, slot });
            next_field = f + 1;
            last = Some((field, slot));
        }
        Projection { type_idx, steps }
    }
}
