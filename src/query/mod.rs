//! A small SQL-like query engine over recordings.
//!
//! ```no_run
//! # fn main() -> jfr_parser::Result<()> {
//! use jfr_parser::{JfrFile, ParallelOptions};
//!
//! let file = JfrFile::open("recording.jfr")?;
//! let result = file.query(
//!     "SELECT eventThread.javaName AS thread, count(*) AS n, p99(duration) AS p99_ns
//!      FROM jdk.JavaMonitorEnter
//!      WHERE duration > 10ms
//!      GROUP BY thread ORDER BY n DESC LIMIT 10",
//!     &ParallelOptions::default(),
//! )?;
//! println!("{result}");
//! # Ok(()) }
//! ```
//!
//! # Grammar
//!
//! ```text
//! SELECT <expr> [AS name] [, ...] | *
//! FROM   <event> [, <event> ...]        names may contain `*`; `GarbageCollection` means `jdk.GarbageCollection`
//! [WHERE <condition>]
//! [GROUP BY <expr> [, ...]]             may name a select alias
//! [HAVING <condition>]
//! [ORDER BY <expr> [ASC|DESC] [, ...]]  may name a select alias
//! [LIMIT <n>]
//! ```
//!
//! # Fields and units
//!
//! Fields are addressed by name. Dotted paths follow constant-pool
//! references and arrays: `eventThread.javaName`, `stackTrace.frames.0.lineNumber`
//! (or `stackTrace.frames[0].lineNumber`). A path through a null yields null.
//!
//! Values are normalized using the units declared in the recording:
//!
//! * durations compare as nanoseconds and `startTime` is nanoseconds since the Unix epoch,
//! * sizes stay in bytes, percentages are fractions (`0.5` is 50%).
//!
//! Numeric literals take unit suffixes: `ns us ms s min h d` for time,
//! `b kb mb gb tb` (powers of 1024) for sizes, and `%` for percentages, so
//! `duration > 10ms` and `heapUsed >= 512mb` mean what they say.
//!
//! The pseudo-columns `$event` (event name), `$chunk` (chunk index) and
//! `$size` (record size in bytes) are available in every query.
//!
//! # Aggregates
//!
//! `count(*)`, `count(x)`, `count(DISTINCT x)`, `sum`, `avg`, `min`, `max`,
//! `median`, `p50`, `p75`, `p90`, `p95`, `p99`, `p999` and
//! `percentile(x, q)`. Percentiles come from a mergeable log-bucketed sketch
//! with about 1% relative error, so they can be computed in parallel
//! without keeping every value. All other aggregates are exact.
//!
//! # Functions
//!
//! | Group | Functions |
//! |---|---|
//! | Strings | `lower`, `upper`, `trim`, `length`, `substr(s, from[, len])`, `concat(...)`, `contains(s, sub)`, `startswith`, `endswith` |
//! | Numbers | `abs`, `floor`, `ceil`, `round`, `sqrt`, `ln`, `log10`, `bucket(x, width)` |
//! | Conversion | `ms(x)`, `us(x)`, `sec(x)` from nanoseconds, `kb(x)`, `mb(x)`, `gb(x)` from bytes |
//! | Time | `ts('2026-11-15T08:20:00Z')` to epoch nanoseconds, `isotime(x)` back to text |
//! | Null handling | `coalesce(...)`, `IS [NOT] NULL` |
//! | Threads | `threadname(thread)` returns the Java name, or the OS name when there is none |
//! | Stacks | `topframe(st)`, `frame(st, i)`, `depth(st)`, `hasframe(st, 'text')`, `stack(st[, max])` |
//!
//! `stack` renders frames root-first and joined by `;`, the folded format
//! that flame graph tools read. `bucket(startTime, 1s)` gives time-series
//! buckets. Predicates support `AND OR NOT`, `IN (...)`, `BETWEEN a AND b`,
//! `LIKE` with `%` and `_`, and follow SQL three-valued logic.
//!
//! # Examples
//!
//! ```text
//! -- slowest 10 monitor waits per thread
//! SELECT threadname(eventThread) AS t, max(duration) AS worst
//! FROM jdk.JavaMonitorEnter GROUP BY t ORDER BY worst DESC LIMIT 10
//!
//! -- GC pause distribution
//! SELECT name, count(*), p50(sumOfPauses), p99(sumOfPauses)
//! FROM jdk.GarbageCollection GROUP BY name
//!
//! -- allocation rate per minute, in MB
//! SELECT isotime(bucket(startTime, 60s)) AS minute, sum(allocationSize) / 1mb AS mb
//! FROM jdk.ObjectAllocationInNewTLAB GROUP BY minute ORDER BY minute
//!
//! -- folded stacks for a flame graph
//! SELECT stack(stackTrace) AS folded, count(*) AS samples
//! FROM jdk.ExecutionSample GROUP BY folded
//! ```
//!
//! Queries run in parallel through [`JfrFile::par_units`] and honour the
//! [`ParallelOptions`] you pass. A name in `FROM` or a field that no chunk
//! defines is reported as an error instead of silently returning nothing.

mod eval;
mod parse;
mod val;

pub use parse::Query;
pub use val::{Cell, Val};

use parse::{AggSpec, Expr, SelectItem, Virt};

use crate::chunk::Chunk;
use crate::error::{Error, Result};
use crate::event::{Event, Events};
use crate::file::{JfrFile, ParallelOptions};
use crate::layout::Projection;
use crate::metadata::Unit;
use crate::value::Value;
use eval::{Env, eval, glob_match};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use val::AggState;

/// Counters describing a query run.
#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    /// Events of the selected types that were examined.
    pub events_scanned: u64,
    /// Events that passed the `WHERE` clause.
    pub events_matched: u64,
    /// Number of chunks in the file.
    pub chunks: usize,
    /// Chunks that could not be read and were skipped.
    pub chunks_skipped: usize,
}

/// The rows produced by a query.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Output column names, in `SELECT` order.
    pub columns: Vec<String>,
    /// Result rows; each row has one cell per column.
    pub rows: Vec<Vec<Cell>>,
    /// Execution counters.
    pub stats: QueryStats,
}

impl QueryResult {
    /// Renders as CSV with RFC 4180 quoting.
    pub fn to_csv(&self) -> String {
        fn field(s: &str, out: &mut String) {
            if s.contains([',', '"', '\n', '\r']) {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
        let mut out = String::new();
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            field(c, &mut out);
        }
        out.push('\n');
        for r in &self.rows {
            for (i, c) in r.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !c.is_null() {
                    field(&c.to_string(), &mut out);
                }
            }
            out.push('\n');
        }
        out
    }

    /// Renders as a JSON array of objects.
    pub fn to_json(&self) -> String {
        fn esc(s: &str, out: &mut String) {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        let mut out = String::from("[");
        for (ri, r) in self.rows.iter().enumerate() {
            if ri > 0 {
                out.push(',');
            }
            out.push('{');
            for (i, (c, v)) in self.columns.iter().zip(r).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                esc(c, &mut out);
                out.push(':');
                match v {
                    Val::Null => out.push_str("null"),
                    Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    Val::Int(x) => out.push_str(&x.to_string()),
                    Val::Float(x) if x.is_finite() => out.push_str(&x.to_string()),
                    Val::Float(_) => out.push_str("null"),
                    other => esc(&other.to_string(), &mut out),
                }
            }
            out.push('}');
        }
        out.push(']');
        out
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cells: Vec<Vec<String>> =
            self.rows.iter().map(|r| r.iter().map(|c| c.to_string().replace('\n', "\\n")).collect()).collect();
        let mut w: Vec<usize> = self.columns.iter().map(|c| c.chars().count()).collect();
        for r in &cells {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(c.chars().count().min(120));
            }
        }
        let numeric: Vec<bool> = (0..self.columns.len())
            .map(|i| self.rows.iter().all(|r| matches!(r[i], Val::Int(_) | Val::Float(_) | Val::Null)))
            .collect();
        let line = |f: &mut fmt::Formatter<'_>, items: &[String], header: bool| -> fmt::Result {
            for (i, c) in items.iter().enumerate() {
                let c: String = if c.chars().count() > 120 { c.chars().take(119).chain(['…']).collect() } else { c.clone() };
                if i > 0 {
                    f.write_str("  ")?;
                }
                if numeric[i] && !header {
                    write!(f, "{c:>w$}", w = w[i])?;
                } else if i + 1 == items.len() {
                    f.write_str(&c)?;
                } else {
                    write!(f, "{c:<w$}", w = w[i])?;
                }
            }
            writeln!(f)
        };
        line(f, &self.columns, true)?;
        let rule: Vec<String> = w.iter().map(|&n| "-".repeat(n)).collect();
        line(f, &rule, true)?;
        for r in &cells {
            line(f, r, false)?;
        }
        write!(f, "({} row{})", self.rows.len(), if self.rows.len() == 1 { "" } else { "s" })
    }
}

impl JfrFile {
    /// Parses and runs a query. See the [module docs](crate::query).
    pub fn query(&self, text: &str, opts: &ParallelOptions) -> Result<QueryResult> {
        Query::parse(text)?.run(self, opts)
    }
}

struct Group {
    key: Vec<Cell>,
    states: Vec<AggState>,
}

#[derive(Default)]
struct GroupTable {
    index: HashMap<u64, Vec<u32>>,
    groups: Vec<Group>,
}

impl GroupTable {
    fn find_or_insert(&mut self, hash: u64, key: &[Val<'_>], aggs: &[AggSpec]) -> usize {
        let bucket = self.index.entry(hash).or_default();
        let found = bucket.iter().copied().find(|&gi| {
            let g = &self.groups[gi as usize];
            g.key.len() == key.len() && g.key.iter().zip(key).all(|(a, b)| a.group_eq(b))
        });
        let gi = match found {
            Some(gi) => gi,
            None => {
                let gi = self.groups.len() as u32;
                self.groups.push(Group {
                    key: key.iter().map(|k| k.clone().into_owned()).collect(),
                    states: aggs.iter().map(AggState::new).collect(),
                });
                bucket.push(gi);
                gi
            }
        };
        gi as usize
    }

    fn merge(&mut self, other: GroupTable, aggs: &[AggSpec]) {
        for g in other.groups {
            let mut h = 0u64;
            for k in &g.key {
                k.hash_into(&mut h);
            }
            let gi = self.find_or_insert(h, &g.key, aggs);
            for ((d, s), spec) in self.groups[gi].states.iter_mut().zip(g.states).zip(aggs) {
                d.merge(spec, s);
            }
        }
    }
}

struct Row {
    cells: Vec<Cell>,
    sort: Vec<Cell>,
}

enum Body {
    Rows(Vec<Row>),
    Groups(GroupTable),
}

struct Partial {
    body: Body,
    scanned: u64,
    matched: u64,
}

struct EventPlan {
    proj: Projection,
    units: Vec<Unit>,
}

fn compare_sort(a: &[Cell], b: &[Cell], desc: &[bool]) -> Ordering {
    for ((x, y), d) in a.iter().zip(b).zip(desc) {
        let o = x.total_cmp(y);
        let o = if *d { o.reverse() } else { o };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

impl Query {
    /// Runs the query over `file`.
    pub fn run(&self, file: &JfrFile, opts: &ParallelOptions) -> Result<QueryResult> {
        let q = self.prepare(file)?;
        let desc: Vec<bool> = q.order_by.iter().map(|(_, d)| *d).collect();

        let res = file.par_units(
            opts,
            |chunk, start, end| q.run_unit(chunk, start, end, &desc),
            |a, b| q.merge_partials(a, b, &desc),
        )?;
        let mut stats = QueryStats {
            chunks: file.chunks().len(),
            chunks_skipped: res.skipped.len(),
            ..Default::default()
        };
        let Some(p) = res.value else {
            return Ok(QueryResult { columns: q.columns(), rows: Vec::new(), stats });
        };
        stats.events_scanned = p.scanned;
        stats.events_matched = p.matched;

        let mut rows = match p.body {
            Body::Rows(r) => r,
            Body::Groups(t) => q.finish_groups(t)?,
        };
        if !q.order_by.is_empty() {
            rows.sort_by(|a, b| compare_sort(&a.sort, &b.sort, &desc));
        }
        if let Some(l) = q.limit {
            rows.truncate(l);
        }
        Ok(QueryResult { columns: q.columns(), rows: rows.into_iter().map(|r| r.cells).collect(), stats })
    }

    fn columns(&self) -> Vec<String> {
        self.select.iter().map(|s| s.name.clone()).collect()
    }

    /// Resolves `SELECT *`, validates event names and field names against the
    /// file's schemas.
    fn prepare(&self, file: &JfrFile) -> Result<Query> {
        let schemas = file.schemas()?;
        for pat in &self.from {
            let hit = schemas.iter().any(|s| s.meta.events().any(|t| glob_match(pat, &t.name)));
            if !hit {
                return Err(Error::NoSuchType(pat.clone()));
            }
        }
        let matches = |name: &str| self.from.iter().any(|p| glob_match(p, name));
        let mut q = self.clone();
        if q.select_all {
            let mut names: Vec<String> = Vec::new();
            for s in &schemas {
                for t in s.meta.events().filter(|t| matches(&t.name)) {
                    for f in &t.fields {
                        if !names.iter().any(|n| **n == *f.name) {
                            names.push(f.name.to_string());
                        }
                    }
                }
            }
            q.select.clear();
            q.select.push(SelectItem { expr: Expr::Virt(Virt::Event), name: "event".into() });
            for n in names {
                let root = match q.roots.iter().position(|r| *r == n) {
                    Some(i) => i,
                    None => {
                        q.roots.push(n.clone());
                        q.roots.len() - 1
                    }
                };
                q.select.push(SelectItem { expr: Expr::Path { root, rest: Vec::new() }, name: n });
            }
            q.select_all = false;
        }
        q.roots_used.resize(q.roots.len(), true);
        for (r, _) in q.roots.iter().zip(&q.roots_used).filter(|(_, used)| **used) {
            let known = schemas.iter().any(|s| {
                s.meta.events().filter(|t| matches(&t.name)).any(|t| t.field_index(r).is_some())
            });
            if !known {
                let from = self.from.join(", ");
                return Err(Error::NoSuchField { type_name: from, field: r.clone() });
            }
        }
        Ok(q)
    }

    fn run_unit<'c>(&self, chunk: &'c Chunk<'c>, start: usize, end: usize, desc: &[bool]) -> Result<Partial> {
        let meta = &chunk.schema().meta;
        let wanted: Vec<bool> =
            meta.types.iter().map(|t| t.is_event && self.from.iter().any(|p| glob_match(p, &t.name))).collect();
        let mut part = Partial {
            body: if self.is_aggregate() { Body::Groups(GroupTable::default()) } else { Body::Rows(Vec::new()) },
            scanned: 0,
            matched: 0,
        };
        if !wanted.iter().any(|&w| w) {
            return Ok(part);
        }
        let mut plans: Vec<Option<EventPlan>> = (0..meta.types.len()).map(|_| None).collect();
        let mut slots: Vec<Value<'c>> = vec![Value::Null; self.roots.len()];
        let mut cur_type = u32::MAX;
        let mut key_buf: Vec<Val<'c>> = Vec::with_capacity(self.group_by.len());
        let prune_at = self.limit.filter(|_| !self.order_by.is_empty()).map(|l| l.saturating_mul(4).max(256));
        let mut single_group: Option<usize> = None;

        for ev in Events::new(chunk, start, end, Some(wanted)) {
            let ev = ev?;
            part.scanned += 1;
            let t = ev.type_idx();
            if plans[t as usize].is_none() {
                plans[t as usize] = Some(self.plan_for(chunk, &ev));
            }
            let Some(plan) = plans[t as usize].as_ref() else { continue };
            if cur_type != t {
                slots.iter_mut().for_each(|s| *s = Value::Null);
                cur_type = t;
            }
            if !plan.proj.steps.is_empty() {
                ev.read_projection(&plan.proj, &mut slots)?;
            }
            let env = Env { chunk: Some(chunk), event: Some(&ev), slots: &slots, units: &plan.units, groups: &[], aggs: &[] };
            if let Some(f) = &self.filter {
                if !matches!(eval(f, &env)?, Val::Bool(true)) {
                    continue;
                }
            }
            part.matched += 1;
            match &mut part.body {
                Body::Groups(table) => {
                    let group = if self.group_by.is_empty() {
                        let gi = *single_group.get_or_insert_with(|| table.find_or_insert(0, &[], &self.aggs));
                        &mut table.groups[gi]
                    } else {
                        key_buf.clear();
                        let mut h = 0u64;
                        for g in &self.group_by {
                            let v = eval(g, &env)?;
                            v.hash_into(&mut h);
                            key_buf.push(v);
                        }
                        let gi = table.find_or_insert(h, &key_buf, &self.aggs);
                        &mut table.groups[gi]
                    };
                    for (st, spec) in group.states.iter_mut().zip(&self.aggs) {
                        match &spec.arg {
                            Some(e) => st.update(spec, Some(&eval(e, &env)?)),
                            None => st.update(spec, None),
                        }
                    }
                }
                Body::Rows(rows) => {
                    if self.order_by.is_empty() {
                        if let Some(l) = self.limit {
                            if rows.len() >= l {
                                continue;
                            }
                        }
                    }
                    let mut cells = Vec::with_capacity(self.select.len());
                    for s in &self.select {
                        cells.push(eval(&s.expr, &env)?.into_owned());
                    }
                    let mut sort = Vec::with_capacity(self.order_by.len());
                    for (e, _) in &self.order_by {
                        sort.push(eval(e, &env)?.into_owned());
                    }
                    rows.push(Row { cells, sort });
                    if let (Some(at), Some(limit)) = (prune_at, self.limit) {
                        if rows.len() >= at {
                            rows.sort_by(|a, b| compare_sort(&a.sort, &b.sort, desc));
                            rows.truncate(limit);
                        }
                    }
                }
            }
        }
        Ok(part)
    }

    fn plan_for<'c>(&self, chunk: &'c Chunk<'c>, ev: &Event<'c>) -> EventPlan {
        let td = ev.type_desc();
        let idxs: Vec<u32> = self
            .roots
            .iter()
            .zip(&self.roots_used)
            .map(|(r, &used)| if used { td.field_index(r).map_or(u32::MAX, |i| i as u32) } else { u32::MAX })
            .collect();
        let units = idxs
            .iter()
            .map(|&i| if i == u32::MAX { Unit::None } else { td.fields[i as usize].unit })
            .collect();
        EventPlan { proj: Projection::new(&chunk.schema().layout, ev.type_idx(), &idxs), units }
    }

    fn merge_partials(&self, mut a: Partial, b: Partial, desc: &[bool]) -> Partial {
        a.scanned += b.scanned;
        a.matched += b.matched;
        match (&mut a.body, b.body) {
            (Body::Rows(x), Body::Rows(y)) => {
                x.extend(y);
                if self.order_by.is_empty() {
                    if let Some(l) = self.limit {
                        x.truncate(l);
                    }
                } else if let Some(l) = self.limit {
                    if x.len() > l.saturating_mul(2).max(256) {
                        x.sort_by(|p, q| compare_sort(&p.sort, &q.sort, desc));
                        x.truncate(l);
                    }
                }
            }
            (Body::Groups(x), Body::Groups(y)) => x.merge(y, &self.aggs),
            _ => {}
        }
        a
    }

    fn finish_groups(&self, table: GroupTable) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(table.groups.len());
        let mut groups = table.groups;
        if groups.is_empty() && self.group_by.is_empty() {
            groups.push(Group { key: Vec::new(), states: self.aggs.iter().map(AggState::new).collect() });
        }
        for g in groups {
            let aggs: Vec<Cell> = g.states.iter().zip(&self.aggs).map(|(s, spec)| s.finish(spec)).collect();
            let env = Env { chunk: None, event: None, slots: &[], units: &[], groups: &g.key, aggs: &aggs };
            if let Some(h) = &self.having {
                if !matches!(eval(h, &env)?, Val::Bool(true)) {
                    continue;
                }
            }
            let mut cells = Vec::with_capacity(self.select.len());
            for s in &self.select {
                cells.push(eval(&s.expr, &env)?.into_owned());
            }
            let mut sort = Vec::with_capacity(self.order_by.len());
            for (e, _) in &self.order_by {
                sort.push(eval(e, &env)?.into_owned());
            }
            rows.push(Row { cells, sort });
        }
        Ok(rows)
    }
}
