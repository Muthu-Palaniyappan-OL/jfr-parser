//! Scalar values used by the query evaluator, and mergeable aggregate state.

use crate::value::Value;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// A query value. Borrows strings and objects from the chunk it came from.
#[derive(Clone, Debug)]
pub enum Val<'c> {
    /// SQL null: a missing field, a null reference or an undefined result.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer. Durations are in nanoseconds and timestamps in nanoseconds since the Unix epoch.
    Int(i64),
    /// A floating point number.
    Float(f64),
    /// A string, borrowed from the recording when possible.
    Str(Cow<'c, str>),
    /// A string shared between rows.
    Shared(Arc<str>),
    /// A non-scalar (struct or array) that only functions can inspect.
    Raw(Value<'c>),
}

/// A value that no longer borrows from a chunk: what query results contain.
pub type Cell = Val<'static>;

impl<'c> Val<'c> {
    /// The string, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            Val::Shared(s) => Some(s),
            _ => None,
        }
    }

    /// True for `Val::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    /// Numbers and booleans as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Val::Int(v) => Some(v as f64),
            Val::Float(v) => Some(v),
            Val::Bool(b) => Some(b as i64 as f64),
            _ => None,
        }
    }

    /// Integers, whole-valued floats and booleans as `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Val::Int(v) => Some(v),
            Val::Float(v) if v.fract() == 0.0 && v.abs() < 9.2e18 => Some(v as i64),
            Val::Bool(b) => Some(b as i64),
            _ => None,
        }
    }

    /// Converts to a value that owns its data. Structs and arrays become
    /// their text rendering.
    pub fn into_owned(self) -> Cell {
        match self {
            Val::Null => Val::Null,
            Val::Bool(b) => Val::Bool(b),
            Val::Int(v) => Val::Int(v),
            Val::Float(v) => Val::Float(v),
            Val::Str(s) => Val::Str(Cow::Owned(s.into_owned())),
            Val::Shared(s) => Val::Shared(s),
            Val::Raw(v) => Val::Str(Cow::Owned(format!("{v:?}"))),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Val::Null => 0,
            Val::Bool(_) => 1,
            Val::Int(_) | Val::Float(_) => 2,
            Val::Str(_) | Val::Shared(_) => 3,
            Val::Raw(_) => 4,
        }
    }

    /// Total order for sorting and min/max: null < bool < numbers < strings.
    pub fn total_cmp(&self, o: &Val<'_>) -> Ordering {
        match (self, o) {
            (Val::Int(a), Val::Int(b)) => a.cmp(b),
            (a, b) if a.rank() == 2 && b.rank() == 2 => {
                a.as_f64().unwrap_or(0.0).total_cmp(&b.as_f64().unwrap_or(0.0))
            }
            (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
            (a, b) if a.rank() == 3 && b.rank() == 3 => a.as_str().cmp(&b.as_str()),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }

    /// SQL-style comparison: `None` when either side is null or the types
    /// are not comparable.
    pub fn sql_cmp(&self, o: &Val<'_>) -> Option<Ordering> {
        match (self, o) {
            (Val::Null, _) | (_, Val::Null) => None,
            (Val::Int(a), Val::Int(b)) => Some(a.cmp(b)),
            (a, b) if a.rank() == 2 && b.rank() == 2 => a.as_f64()?.partial_cmp(&b.as_f64()?),
            (Val::Bool(a), Val::Bool(b)) => Some(a.cmp(b)),
            (a, b) if a.rank() == 3 && b.rank() == 3 => Some(a.as_str()?.cmp(b.as_str()?)),
            _ => None,
        }
    }

    pub(crate) fn hash_into(&self, h: &mut u64) {
        #[inline]
        fn mix(h: &mut u64, v: u64) {
            *h = (h.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        match self {
            Val::Null => mix(h, 0),
            Val::Bool(b) => mix(h, 1 + *b as u64),
            Val::Int(v) => mix(h, *v as u64 ^ 0x1000),
            Val::Float(f) => {
                if f.fract() == 0.0 && f.abs() < 9.2e18 {
                    mix(h, *f as i64 as u64 ^ 0x1000)
                } else {
                    mix(h, f.to_bits() ^ 0x2000)
                }
            }
            Val::Str(_) | Val::Shared(_) => {
                let s = self.as_str().unwrap_or("");
                let mut x = 0xcbf2_9ce4_8422_2325u64;
                for chunk in s.as_bytes().chunks(8) {
                    let mut w = [0u8; 8];
                    w[..chunk.len()].copy_from_slice(chunk);
                    x = (x.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(0x517c_c1b7_2722_0a95);
                }
                mix(h, x ^ s.len() as u64);
            }
            Val::Raw(v) => {
                let t = format!("{v:?}");
                Val::Str(Cow::Owned(t)).hash_into(h)
            }
        }
    }

    /// Equality used for grouping: numbers compare by value, null equals null.
    pub fn group_eq(&self, o: &Val<'_>) -> bool {
        match (self, o) {
            (Val::Null, Val::Null) => true,
            (a, b) if a.rank() == 3 && b.rank() == 3 => a.as_str() == b.as_str(),
            (a, b) if a.rank() == 2 && b.rank() == 2 => a.total_cmp(b) == Ordering::Equal,
            (Val::Bool(a), Val::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl fmt::Display for Val<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::Null => f.write_str("null"),
            Val::Bool(b) => write!(f, "{b}"),
            Val::Int(v) => write!(f, "{v}"),
            Val::Float(v) => {
                if v.fract() == 0.0 && v.abs() < 1e15 {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            Val::Str(s) => f.write_str(s),
            Val::Shared(s) => f.write_str(s),
            Val::Raw(v) => write!(f, "{v:?}"),
        }
    }
}

impl PartialEq for Val<'_> {
    fn eq(&self, o: &Self) -> bool {
        self.group_eq(o)
    }
}

impl Eq for Val<'_> {}

impl Hash for Val<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut h = 0u64;
        self.hash_into(&mut h);
        state.write_u64(h);
    }
}

/// Log-bucketed histogram with bounded relative error (about 1%), in the
/// style of DDSketch. Mergeable by adding bucket counts, which is what makes
/// percentiles computable across parallel workers without keeping every
/// value.
#[derive(Clone, Debug, Default)]
pub struct Sketch {
    pos: HashMap<i32, u64>,
    neg: HashMap<i32, u64>,
    zero: u64,
    count: u64,
    min: f64,
    max: f64,
}

const GAMMA: f64 = 1.02;

impl Sketch {
    fn key(v: f64) -> i32 {
        (v.ln() / GAMMA.ln()).ceil() as i32
    }
    fn value(k: i32) -> f64 {
        2.0 * GAMMA.powi(k) / (GAMMA + 1.0)
    }

    pub fn add(&mut self, v: f64) {
        if v.is_nan() {
            return;
        }
        if self.count == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.count += 1;
        if v > 0.0 && v.is_finite() {
            *self.pos.entry(Self::key(v)).or_default() += 1;
        } else if v < 0.0 && v.is_finite() {
            *self.neg.entry(Self::key(-v)).or_default() += 1;
        } else if v == 0.0 {
            self.zero += 1;
        } else if v > 0.0 {
            *self.pos.entry(i32::MAX).or_default() += 1;
        } else {
            *self.neg.entry(i32::MAX).or_default() += 1;
        }
    }

    pub fn merge(&mut self, o: Sketch) {
        if o.count == 0 {
            return;
        }
        if self.count == 0 {
            self.min = o.min;
            self.max = o.max;
        } else {
            self.min = self.min.min(o.min);
            self.max = self.max.max(o.max);
        }
        self.count += o.count;
        self.zero += o.zero;
        for (k, c) in o.pos {
            *self.pos.entry(k).or_default() += c;
        }
        for (k, c) in o.neg {
            *self.neg.entry(k).or_default() += c;
        }
    }

    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        if q <= 0.0 {
            return Some(self.min);
        }
        if q >= 1.0 {
            return Some(self.max);
        }
        let rank = (q * (self.count - 1) as f64).floor() as u64;
        let mut seen = 0u64;
        let mut neg: Vec<_> = self.neg.iter().collect();
        neg.sort_by(|a, b| b.0.cmp(a.0));
        for (k, c) in neg {
            seen += c;
            if seen > rank {
                return Some(-Self::value(*k));
            }
        }
        seen += self.zero;
        if seen > rank && self.zero > 0 {
            return Some(0.0);
        }
        let mut pos: Vec<_> = self.pos.iter().collect();
        pos.sort_by(|a, b| a.0.cmp(b.0));
        for (k, c) in pos {
            seen += c;
            if seen > rank {
                return Some(Self::value(*k).clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }
}

use super::parse::{AggKind, AggSpec};

#[derive(Debug, Clone)]
pub(crate) enum AggState {
    Count(u64),
    Distinct(HashSet<Cell>),
    Sum { int: i128, float: f64, any_float: bool, n: u64 },
    Avg { sum: f64, n: u64 },
    Extreme(Option<Cell>),
    Quantile(Sketch),
}

impl AggState {
    pub(crate) fn new(spec: &AggSpec) -> AggState {
        match spec.kind {
            AggKind::Count => AggState::Count(0),
            AggKind::CountDistinct => AggState::Distinct(HashSet::new()),
            AggKind::Sum => AggState::Sum { int: 0, float: 0.0, any_float: false, n: 0 },
            AggKind::Avg => AggState::Avg { sum: 0.0, n: 0 },
            AggKind::Min | AggKind::Max => AggState::Extreme(None),
            AggKind::Percentile => AggState::Quantile(Sketch::default()),
        }
    }

    /// `v` is `None` for `count(*)`.
    pub(crate) fn update(&mut self, spec: &AggSpec, v: Option<&Val<'_>>) {
        match (self, v) {
            (AggState::Count(n), None) => *n += 1,
            (AggState::Count(n), Some(v)) => *n += !v.is_null() as u64,
            (AggState::Distinct(s), Some(v)) if !v.is_null() => {
                s.insert(v.clone().into_owned());
            }
            (AggState::Sum { int, float, any_float, n }, Some(v)) => match v {
                Val::Int(i) => {
                    *int += *i as i128;
                    *n += 1;
                }
                Val::Float(f) => {
                    *float += *f;
                    *any_float = true;
                    *n += 1;
                }
                Val::Bool(b) => {
                    *int += *b as i128;
                    *n += 1;
                }
                _ => {}
            },
            (AggState::Avg { sum, n }, Some(v)) => {
                if let Some(f) = v.as_f64() {
                    *sum += f;
                    *n += 1;
                }
            }
            (AggState::Extreme(cur), Some(v)) if !v.is_null() => {
                let want_max = spec.kind == AggKind::Max;
                let replace = match cur {
                    None => true,
                    Some(c) => {
                        let o = v.total_cmp(c);
                        if want_max { o == Ordering::Greater } else { o == Ordering::Less }
                    }
                };
                if replace {
                    *cur = Some(v.clone().into_owned());
                }
            }
            (AggState::Quantile(s), Some(v)) => {
                if let Some(f) = v.as_f64() {
                    s.add(f);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn merge(&mut self, spec: &AggSpec, o: AggState) {
        match (self, o) {
            (AggState::Count(a), AggState::Count(b)) => *a += b,
            (AggState::Distinct(a), AggState::Distinct(b)) => a.extend(b),
            (
                AggState::Sum { int, float, any_float, n },
                AggState::Sum { int: i2, float: f2, any_float: a2, n: n2 },
            ) => {
                *int += i2;
                *float += f2;
                *any_float |= a2;
                *n += n2;
            }
            (AggState::Avg { sum, n }, AggState::Avg { sum: s2, n: n2 }) => {
                *sum += s2;
                *n += n2;
            }
            (AggState::Extreme(a), AggState::Extreme(Some(b))) => {
                let want_max = spec.kind == AggKind::Max;
                let replace = match a {
                    None => true,
                    Some(c) => {
                        let o = b.total_cmp(c);
                        if want_max { o == Ordering::Greater } else { o == Ordering::Less }
                    }
                };
                if replace {
                    *a = Some(b);
                }
            }
            (AggState::Quantile(a), AggState::Quantile(b)) => a.merge(b),
            _ => {}
        }
    }

    pub(crate) fn finish(&self, spec: &AggSpec) -> Cell {
        match self {
            AggState::Count(n) => Val::Int(*n as i64),
            AggState::Distinct(s) => Val::Int(s.len() as i64),
            AggState::Sum { int, float, any_float, n } => {
                if *n == 0 {
                    Val::Null
                } else if *any_float {
                    Val::Float(*float + *int as f64)
                } else {
                    match i64::try_from(*int) {
                        Ok(v) => Val::Int(v),
                        Err(_) => Val::Float(*int as f64),
                    }
                }
            }
            AggState::Avg { sum, n } => {
                if *n == 0 {
                    Val::Null
                } else {
                    Val::Float(sum / *n as f64)
                }
            }
            AggState::Extreme(v) => v.clone().unwrap_or(Val::Null),
            AggState::Quantile(s) => s.quantile(spec.quantile).map_or(Val::Null, Val::Float),
        }
    }
}
