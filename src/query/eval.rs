//! Expression evaluation.

use super::parse::{BinOp, Expr, Seg, Virt};
use super::val::{Cell, Val};
use crate::chunk::Chunk;
use crate::error::{Error, Result};
use crate::event::Event;
use crate::metadata::Unit;
use crate::value::{Array, Obj, Value};
use std::borrow::Cow;
use std::cmp::Ordering;

/// Everything an expression can read while it is evaluated.
pub(crate) struct Env<'a, 'c> {
    pub chunk: Option<&'c Chunk<'c>>,
    pub event: Option<&'a Event<'c>>,
    pub slots: &'a [Value<'c>],
    pub units: &'a [Unit],
    pub groups: &'a [Cell],
    pub aggs: &'a [Cell],
}

pub(crate) fn to_val<'c>(v: Value<'c>, unit: Unit, chunk: &Chunk<'_>) -> Val<'c> {
    let time = unit.is_time();
    match v {
        Value::Null => Val::Null,
        Value::Bool(b) => Val::Bool(b),
        Value::Byte(x) => Val::Int(x as i64),
        Value::Char(x) => Val::Int(x as i64),
        Value::Short(x) => Val::Int(x as i64),
        Value::Int(x) => Val::Int(if time { chunk.time.normalize(unit, x as i64) } else { x as i64 }),
        Value::Long(x) => Val::Int(if time { chunk.time.normalize(unit, x) } else { x }),
        Value::Float(x) => Val::Float(x as f64),
        Value::Double(x) => Val::Float(x),
        Value::Str(s) => Val::Str(s),
        other => Val::Raw(other),
    }
}

fn path_val<'c>(root: &Value<'c>, root_unit: Unit, rest: &[Seg], chunk: &'c Chunk<'c>) -> Result<Val<'c>> {
    let mut cur = root.clone();
    let mut unit = root_unit;
    let mut reduced: Option<String> = None;
    for seg in rest {
        cur = match (cur, seg) {
            (Value::Null, _) => return Ok(Val::Null),
            (Value::Obj(o), Seg::Name(n)) => {
                let Some(idx) = o.type_desc().field_index(n) else {
                    if reduced.as_deref() == Some(n.as_str()) {
                        reduced = None;
                        cur = Value::Obj(o);
                        continue;
                    }
                    return Ok(Val::Null);
                };
                unit = o.type_desc().fields[idx].unit;
                let fd_type = o.type_desc().fields[idx].type_idx;
                let tl = &chunk.schema().layout.types[fd_type as usize];
                reduced = tl.reduced.and_then(|_| {
                    chunk.schema().meta.types[fd_type as usize].fields.first().map(|f| f.name.to_string())
                });
                o.get_idx(idx)?
            }
            (Value::Array(a), Seg::Index(i)) => {
                unit = Unit::None;
                reduced = None;
                match a.get(*i) {
                    Some(v) => v?,
                    None => return Ok(Val::Null),
                }
            }
            (Value::Array(a), Seg::Name(n)) if n == "length" || n == "size" => Value::Long(a.len() as i64),
            (other, Seg::Name(n)) if reduced.as_deref() == Some(n.as_str()) => {
                reduced = None;
                other
            }
            (Value::Obj(o), Seg::Index(_)) => {
                return Err(Error::Query(format!("cannot index into struct {}", o.type_name())));
            }
            _ => return Ok(Val::Null),
        };
    }
    Ok(to_val(cur, unit, chunk))
}

fn tri(v: &Val<'_>) -> Option<bool> {
    match v {
        Val::Bool(b) => Some(*b),
        Val::Null => None,
        Val::Int(i) => Some(*i != 0),
        _ => None,
    }
}

fn from_tri<'c>(t: Option<bool>) -> Val<'c> {
    t.map_or(Val::Null, Val::Bool)
}

pub(crate) fn eval<'c>(e: &Expr, env: &Env<'_, 'c>) -> Result<Val<'c>> {
    Ok(match e {
        Expr::Null => Val::Null,
        Expr::Bool(b) => Val::Bool(*b),
        Expr::Int(v, _) => Val::Int(*v),
        Expr::Float(v, _) => Val::Float(*v),
        Expr::Str(s) => Val::Str(Cow::Owned(s.clone())),
        Expr::Path { root, rest } => {
            let chunk = env.chunk.ok_or_else(|| Error::Query("field access is not available after aggregation".into()))?;
            let v = &env.slots[*root];
            let unit = env.units[*root];
            if rest.is_empty() {
                to_val(v.clone(), unit, chunk)
            } else {
                path_val(v, unit, rest, chunk)?
            }
        }
        Expr::Virt(v) => match (v, env.event) {
            (Virt::Event, Some(ev)) => Val::Str(Cow::Borrowed(ev.name())),
            (Virt::Chunk, _) => env.chunk.map_or(Val::Null, |c| Val::Int(c.info.index as i64)),
            (Virt::Size, Some(ev)) => Val::Int(ev.size() as i64),
            _ => Val::Null,
        },
        Expr::Not(x) => from_tri(tri(&eval(x, env)?).map(|b| !b)),
        Expr::Neg(x) => match eval(x, env)? {
            Val::Int(i) => Val::Int(i.wrapping_neg()),
            Val::Float(f) => Val::Float(-f),
            _ => Val::Null,
        },
        Expr::Bin(op, l, r) => match op {
            BinOp::And => {
                let a = tri(&eval(l, env)?);
                if a == Some(false) {
                    return Ok(Val::Bool(false));
                }
                let b = tri(&eval(r, env)?);
                match (a, b) {
                    (_, Some(false)) => Val::Bool(false),
                    (Some(true), Some(true)) => Val::Bool(true),
                    _ => Val::Null,
                }
            }
            BinOp::Or => {
                let a = tri(&eval(l, env)?);
                if a == Some(true) {
                    return Ok(Val::Bool(true));
                }
                let b = tri(&eval(r, env)?);
                match (a, b) {
                    (_, Some(true)) => Val::Bool(true),
                    (Some(false), Some(false)) => Val::Bool(false),
                    _ => Val::Null,
                }
            }
            _ => binary(*op, eval(l, env)?, eval(r, env)?),
        },
        Expr::In { expr, list, negated } => {
            let v = eval(expr, env)?;
            if v.is_null() {
                return Ok(Val::Null);
            }
            let mut saw_null = false;
            let mut hit = false;
            for x in list {
                let x = eval(x, env)?;
                match v.sql_cmp(&x) {
                    Some(Ordering::Equal) => {
                        hit = true;
                        break;
                    }
                    None => saw_null = true,
                    _ => {}
                }
            }
            if hit {
                Val::Bool(!negated)
            } else if saw_null {
                Val::Null
            } else {
                Val::Bool(*negated)
            }
        }
        Expr::Between { expr, lo, hi, negated } => {
            let v = eval(expr, env)?;
            let (lo, hi) = (eval(lo, env)?, eval(hi, env)?);
            match (v.sql_cmp(&lo), v.sql_cmp(&hi)) {
                (Some(a), Some(b)) => Val::Bool((a != Ordering::Less && b != Ordering::Greater) != *negated),
                _ => Val::Null,
            }
        }
        Expr::Like { expr, pattern, negated } => {
            let (v, p) = (eval(expr, env)?, eval(pattern, env)?);
            match (v.as_str(), p.as_str()) {
                (Some(s), Some(p)) => Val::Bool(like(s, p) != *negated),
                _ => Val::Null,
            }
        }
        Expr::IsNull { expr, negated } => Val::Bool(eval(expr, env)?.is_null() != *negated),
        Expr::Func(name, args) => call(name, args, env)?,
        Expr::Agg(i) => env.aggs.get(*i).cloned().unwrap_or(Val::Null),
        Expr::Group(i) => env.groups.get(*i).cloned().unwrap_or(Val::Null),
    })
}

fn binary<'c>(op: BinOp, a: Val<'c>, b: Val<'c>) -> Val<'c> {
    use BinOp::*;
    match op {
        Eq | Ne | Lt | Le | Gt | Ge => match a.sql_cmp(&b) {
            None => Val::Null,
            Some(o) => Val::Bool(match op {
                Eq => o == Ordering::Equal,
                Ne => o != Ordering::Equal,
                Lt => o == Ordering::Less,
                Le => o != Ordering::Greater,
                Gt => o == Ordering::Greater,
                _ => o != Ordering::Less,
            }),
        },
        Add if a.as_str().is_some() && b.as_str().is_some() => {
            Val::Str(Cow::Owned(format!("{}{}", a.as_str().unwrap_or(""), b.as_str().unwrap_or(""))))
        }
        Add | Sub | Mul | Div | Mod => {
            if let (Val::Int(x), Val::Int(y)) = (&a, &b) {
                let r = match op {
                    Add => x.checked_add(*y),
                    Sub => x.checked_sub(*y),
                    Mul => x.checked_mul(*y),
                    Div => (*y != 0 && x % y == 0).then(|| x / y),
                    _ => (*y != 0).then(|| x.wrapping_rem(*y)),
                };
                if let Some(r) = r {
                    return Val::Int(r);
                }
                if matches!(op, Mod) {
                    return Val::Null;
                }
            }
            match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => {
                    if y == 0.0 && matches!(op, Div | Mod) {
                        return Val::Null;
                    }
                    Val::Float(match op {
                        Add => x + y,
                        Sub => x - y,
                        Mul => x * y,
                        Div => x / y,
                        _ => x % y,
                    })
                }
                _ => Val::Null,
            }
        }
        And | Or => Val::Null,
    }
}

/// SQL `LIKE`: `%` matches any run, `_` matches one character.
pub(crate) fn like(s: &str, p: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = p.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if j < p.len() && (p[j] == '_' || p[j] == s[i]) && p[j] != '%' {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = j;
            mark = i;
            j += 1;
        } else if star != usize::MAX {
            j = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

/// Matches an event name against a `FROM` pattern. `*` matches any run of
/// characters; a pattern without a dot also matches `jdk.<pattern>`.
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        let (mut i, mut j) = (0, 0);
        let (mut star, mut mark) = (usize::MAX, 0);
        while j < n.len() {
            if i < p.len() && p[i] == b'*' {
                star = i;
                mark = j;
                i += 1;
            } else if i < p.len() && p[i] == n[j] {
                i += 1;
                j += 1;
            } else if star != usize::MAX {
                i = star + 1;
                mark += 1;
                j = mark;
            } else {
                return false;
            }
        }
        while i < p.len() && p[i] == b'*' {
            i += 1;
        }
        i == p.len()
    }
    go(pattern.as_bytes(), name.as_bytes()) || (!pattern.contains('.') && go(pattern.as_bytes(), name.strip_prefix("jdk.").unwrap_or("\0").as_bytes()))
}

fn arg<'c>(args: &[Expr], i: usize, env: &Env<'_, 'c>) -> Result<Val<'c>> {
    match args.get(i) {
        Some(e) => eval(e, env),
        None => Err(Error::Query("missing function argument".into())),
    }
}

fn frame_text(f: &Obj<'_>) -> Result<Option<String>> {
    let name = f.path("method.name")?;
    let Some(name) = name.as_str() else { return Ok(None) };
    let cls = f.path("method.type.name")?;
    Ok(Some(match cls.as_str() {
        Some(c) if !c.is_empty() => format!("{c}.{name}"),
        _ => name.to_string(),
    }))
}

fn frames_of<'c>(v: &Val<'c>) -> Result<Option<Array<'c>>> {
    let Val::Raw(Value::Obj(o)) = v else { return Ok(None) };
    Ok(o.get("frames")?.as_array())
}

fn call<'c>(name: &str, args: &[Expr], env: &Env<'_, 'c>) -> Result<Val<'c>> {
    let n_args = |n: usize| -> Result<()> {
        if args.len() == n { Ok(()) } else { Err(Error::Query(format!("{name}() takes {n} argument(s), got {}", args.len()))) }
    };
    let owned = |s: String| Val::Str(Cow::Owned(s));
    Ok(match name {
        "lower" | "upper" | "length" | "trim" => {
            n_args(1)?;
            let v = arg(args, 0, env)?;
            let Some(s) = v.as_str() else { return Ok(Val::Null) };
            match name {
                "lower" => owned(s.to_lowercase()),
                "upper" => owned(s.to_uppercase()),
                "trim" => owned(s.trim().to_string()),
                _ => Val::Int(s.chars().count() as i64),
            }
        }
        "contains" | "startswith" | "endswith" => {
            n_args(2)?;
            let (a, b) = (arg(args, 0, env)?, arg(args, 1, env)?);
            match (a.as_str(), b.as_str()) {
                (Some(a), Some(b)) => Val::Bool(match name {
                    "contains" => a.contains(b),
                    "startswith" => a.starts_with(b),
                    _ => a.ends_with(b),
                }),
                _ => Val::Null,
            }
        }
        "substr" | "substring" => {
            let v = arg(args, 0, env)?;
            let Some(s) = v.as_str() else { return Ok(Val::Null) };
            let start = arg(args, 1, env)?.as_i64().unwrap_or(1).max(1) as usize - 1;
            let len = if args.len() > 2 { arg(args, 2, env)?.as_i64().map(|l| l.max(0) as usize) } else { None };
            let it = s.chars().skip(start);
            owned(match len {
                Some(l) => it.take(l).collect(),
                None => it.collect(),
            })
        }
        "concat" => {
            let mut out = String::new();
            for i in 0..args.len() {
                let v = arg(args, i, env)?;
                if !v.is_null() {
                    out.push_str(&v.to_string());
                }
            }
            owned(out)
        }
        "coalesce" => {
            for i in 0..args.len() {
                let v = arg(args, i, env)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Val::Null
        }
        "abs" | "floor" | "ceil" | "round" | "sqrt" | "ln" | "log10" => {
            n_args(1)?;
            match arg(args, 0, env)? {
                Val::Int(i) if matches!(name, "abs" | "floor" | "ceil" | "round") => Val::Int(if name == "abs" { i.wrapping_abs() } else { i }),
                v => match v.as_f64() {
                    Some(f) => Val::Float(match name {
                        "abs" => f.abs(),
                        "floor" => f.floor(),
                        "ceil" => f.ceil(),
                        "round" => f.round(),
                        "sqrt" => f.sqrt(),
                        "ln" => f.ln(),
                        _ => f.log10(),
                    }),
                    None => Val::Null,
                },
            }
        }
        "bucket" => {
            n_args(2)?;
            let (v, w) = (arg(args, 0, env)?, arg(args, 1, env)?);
            match (&v, &w) {
                (Val::Int(x), Val::Int(w)) if *w > 0 => Val::Int(x.div_euclid(*w) * w),
                _ => match (v.as_f64(), w.as_f64()) {
                    (Some(x), Some(w)) if w > 0.0 => Val::Float((x / w).floor() * w),
                    _ => Val::Null,
                },
            }
        }
        "ms" | "us" | "sec" | "mb" | "kb" | "gb" => {
            n_args(1)?;
            let div = match name {
                "ms" => 1e6,
                "us" => 1e3,
                "sec" => 1e9,
                "kb" => 1024.0,
                "mb" => 1024.0 * 1024.0,
                _ => 1024.0 * 1024.0 * 1024.0,
            };
            arg(args, 0, env)?.as_f64().map_or(Val::Null, |f| Val::Float(f / div))
        }
        "ts" => {
            n_args(1)?;
            match arg(args, 0, env)?.as_str().and_then(parse_iso) {
                Some(ns) => Val::Int(ns),
                None => return Err(Error::Query("ts() expects an ISO-8601 UTC time such as '2026-11-15T08:20:00Z'".into())),
            }
        }
        "isotime" => {
            n_args(1)?;
            arg(args, 0, env)?.as_i64().map_or(Val::Null, |ns| owned(format_iso(ns)))
        }
        "threadname" => {
            n_args(1)?;
            let v = arg(args, 0, env)?;
            let Val::Raw(Value::Obj(o)) = &v else { return Ok(Val::Null) };
            let j = o.get("javaName")?;
            match j.as_str() {
                Some(s) => Val::Str(Cow::Owned(s.to_string())),
                None => match o.get("osName")?.as_str() {
                    Some(s) => Val::Str(Cow::Owned(s.to_string())),
                    None => Val::Null,
                },
            }
        }
        "depth" => {
            n_args(1)?;
            frames_of(&arg(args, 0, env)?)?.map_or(Val::Null, |a| Val::Int(a.len() as i64))
        }
        "topframe" | "frame" => {
            let i = if name == "frame" { arg(args, 1, env)?.as_i64().unwrap_or(0).max(0) as usize } else { 0 };
            let Some(frames) = frames_of(&arg(args, 0, env)?)? else { return Ok(Val::Null) };
            match frames.get(i) {
                Some(f) => match f?.as_obj() {
                    Some(o) => frame_text(&o)?.map_or(Val::Null, owned),
                    None => Val::Null,
                },
                None => Val::Null,
            }
        }
        "stack" => {
            let Some(frames) = frames_of(&arg(args, 0, env)?)? else { return Ok(Val::Null) };
            let limit = if args.len() > 1 { arg(args, 1, env)?.as_i64().map(|d| d.max(0) as usize) } else { None };
            let mut names = Vec::new();
            for f in frames.iter().take(limit.unwrap_or(usize::MAX)) {
                if let Some(o) = f?.as_obj() {
                    names.push(frame_text(&o)?.unwrap_or_else(|| "?".into()));
                }
            }
            names.reverse();
            owned(names.join(";"))
        }
        "hasframe" => {
            n_args(2)?;
            let pat = arg(args, 1, env)?;
            let (Some(frames), Some(pat)) = (frames_of(&arg(args, 0, env)?)?, pat.as_str().map(str::to_string)) else {
                return Ok(Val::Null);
            };
            for f in frames.iter() {
                if let Some(o) = f?.as_obj() {
                    if frame_text(&o)?.is_some_and(|t| t.contains(&pat)) {
                        return Ok(Val::Bool(true));
                    }
                }
            }
            Val::Bool(false)
        }
        other => return Err(Error::Query(format!("unknown function `{other}`"))),
    })
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `YYYY-MM-DD[THH:MM[:SS[.fraction]]][Z]` as UTC, to epoch nanoseconds.
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let (y, m, d): (i64, i64, i64) = (dp.next()?.parse().ok()?, dp.next()?.parse().ok()?, dp.next()?.parse().ok()?);
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ns = days_from_civil(y, m, d).checked_mul(86_400_000_000_000)?;
    if let Some(t) = time {
        let mut tp = t.split(':');
        let h: i64 = tp.next()?.parse().ok()?;
        let mi: i64 = tp.next().map_or(Some(0), |x| x.parse().ok())?;
        let (sec, frac) = match tp.next() {
            Some(x) => match x.split_once('.') {
                Some((a, f)) => {
                    let digits: String = f.chars().take(9).collect();
                    let scale = 10i64.pow(9 - digits.len() as u32);
                    (a.parse::<i64>().ok()?, digits.parse::<i64>().ok()? * scale)
                }
                None => (x.parse::<i64>().ok()?, 0),
            },
            None => (0, 0),
        };
        ns = ns.checked_add(((h * 60 + mi) * 60 + sec) * 1_000_000_000 + frac)?;
    }
    Some(ns)
}

pub(crate) fn format_iso(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let frac = ns.rem_euclid(1_000_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let t = secs.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z", t / 3600, t / 60 % 60, t % 60, frac)
}
