//! Lexer, parser and the query AST.

use crate::error::{Error, Result};
use crate::metadata::Unit;
use std::fmt;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Query(msg.into()))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Int(i64, Option<String>),
    Float(f64, Option<String>),
    Str(String),
    LParen,
    RParen,
    Comma,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LBracket,
    RBracket,
    Dot,
    Eof,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

fn lex(src: &str) -> Result<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let tok = match c {
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b'[' => {
                i += 1;
                Tok::LBracket
            }
            b']' => {
                i += 1;
                Tok::RBracket
            }
            b'.' if i > 0 && b[i - 1] == b']' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') => {
                i += 1;
                Tok::Dot
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'*' => {
                i += 1;
                Tok::Star
            }
            b'+' => {
                i += 1;
                Tok::Plus
            }
            b'-' => {
                i += 1;
                Tok::Minus
            }
            b'/' => {
                i += 1;
                Tok::Slash
            }
            b'=' => {
                i += 1;
                if b.get(i) == Some(&b'=') {
                    i += 1;
                }
                Tok::Eq
            }
            b'!' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::Ne
            }
            b'<' => {
                i += 1;
                match b.get(i) {
                    Some(b'=') => {
                        i += 1;
                        Tok::Le
                    }
                    Some(b'>') => {
                        i += 1;
                        Tok::Ne
                    }
                    _ => Tok::Lt,
                }
            }
            b'>' => {
                i += 1;
                if b.get(i) == Some(&b'=') {
                    i += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'\'' | b'"' => {
                let q = c;
                i += 1;
                let mut s = Vec::new();
                loop {
                    match b.get(i) {
                        None => return err(format!("unterminated string starting at {start}")),
                        Some(&x) if x == q => {
                            if b.get(i + 1) == Some(&q) {
                                s.push(q);
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(&x) => {
                            s.push(x);
                            i += 1;
                        }
                    }
                }
                Tok::Str(String::from_utf8_lossy(&s).into_owned())
            }
            b'0'..=b'9' => {
                let mut is_float = false;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
                if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    is_float = true;
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let num: String = src[start..i].chars().filter(|&c| c != '_').collect();
                let sfx_start = i;
                if b.get(i) == Some(&b'%') {
                    i += 1;
                } else {
                    while i < b.len() && b[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                }
                let sfx = (i > sfx_start).then(|| src[sfx_start..i].to_string());
                if is_float {
                    Tok::Float(num.parse().map_err(|_| Error::Query(format!("bad number `{num}`")))?, sfx)
                } else {
                    match num.parse::<i64>() {
                        Ok(v) => Tok::Int(v, sfx),
                        Err(_) => Tok::Float(num.parse().map_err(|_| Error::Query(format!("bad number `{num}`")))?, sfx),
                    }
                }
            }
            b'%' => {
                i += 1;
                Tok::Percent
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'$' | b'.')) {
                    i += 1;
                }
                Tok::Ident(src[start..i].to_string())
            }
            other => return err(format!("unexpected character `{}` at {start}", other as char)),
        };
        out.push(Token { tok, start, end: i });
    }
    out.push(Token { tok: Tok::Eof, start: src.len(), end: src.len() });
    Ok(out)
}

/// One step of a field path.
#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Name(String),
    Index(usize),
}

/// Pseudo-columns available in every query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Virt {
    /// `$event`: the event type name.
    Event,
    /// `$chunk`: index of the chunk in the file.
    Chunk,
    /// `$size`: the event record size in bytes.
    Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64, Unit),
    Float(f64, Unit),
    Str(String),
    /// A field path; `root` indexes [`Query::roots`].
    Path { root: usize, rest: Vec<Seg> },
    Virt(Virt),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Func(String, Vec<Expr>),
    In { expr: Box<Expr>, list: Vec<Expr>, negated: bool },
    Between { expr: Box<Expr>, lo: Box<Expr>, hi: Box<Expr>, negated: bool },
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool },
    IsNull { expr: Box<Expr>, negated: bool },
    /// Result of aggregate `n` ([`Query::aggs`]).
    Agg(usize),
    /// Value of group-by expression `n`.
    Group(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
    /// Percentile; the quantile is stored on the spec.
    Percentile,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggSpec {
    pub kind: AggKind,
    /// `None` for `count(*)`.
    pub arg: Option<Expr>,
    pub quantile: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub expr: Expr,
    pub name: String,
}

/// A parsed query.
#[derive(Debug, Clone)]
pub struct Query {
    pub(crate) select: Vec<SelectItem>,
    pub(crate) select_all: bool,
    /// Event names or globs (`jdk.GC*`).
    pub(crate) from: Vec<String>,
    pub(crate) filter: Option<Expr>,
    pub(crate) group_by: Vec<Expr>,
    pub(crate) having: Option<Expr>,
    pub(crate) order_by: Vec<(Expr, bool)>,
    pub(crate) limit: Option<usize>,
    pub(crate) aggs: Vec<AggSpec>,
    /// Distinct top-level field names the query reads.
    pub(crate) roots: Vec<String>,
    /// For each root, whether any expression actually reads it
    /// (aliases that only name output columns are not fields).
    pub(crate) roots_used: Vec<bool>,
}

const KEYWORDS: &[&str] = &[
    "select", "from", "where", "group", "order", "by", "limit", "having", "as", "and", "or", "not", "in", "like",
    "is", "null", "between", "true", "false", "asc", "desc", "distinct",
];

fn is_kw(s: &str, kw: &str) -> bool {
    s.eq_ignore_ascii_case(kw)
}

fn unit_for_suffix(sfx: &str) -> Result<(f64, Unit)> {
    let s = sfx.to_ascii_lowercase();
    Ok(match s.as_str() {
        "ns" => (1.0, Unit::TimespanNanos),
        "us" => (1e3, Unit::TimespanNanos),
        "ms" => (1e6, Unit::TimespanNanos),
        "s" | "sec" => (1e9, Unit::TimespanNanos),
        "min" => (60e9, Unit::TimespanNanos),
        "h" => (3600e9, Unit::TimespanNanos),
        "d" => (86400e9, Unit::TimespanNanos),
        "b" => (1.0, Unit::Bytes),
        "kb" | "k" => (1024.0, Unit::Bytes),
        "mb" | "m" => (1024.0 * 1024.0, Unit::Bytes),
        "gb" | "g" => (1024.0 * 1024.0 * 1024.0, Unit::Bytes),
        "tb" => (1024.0f64.powi(4), Unit::Bytes),
        "%" => (0.01, Unit::Percentage),
        _ => return err(format!("unknown unit suffix `{sfx}`")),
    })
}

struct Parser<'s> {
    src: &'s str,
    toks: Vec<Token>,
    pos: usize,
    q: Query,
    agg_depth: u32,
}

impl Query {
    /// Parses a query string.
    ///
    /// ```text
    /// SELECT <expr> [AS name], ... | *
    /// FROM <event>[, <event>...]          (names may contain * globs)
    /// [WHERE <condition>]
    /// [GROUP BY <expr>, ...]
    /// [HAVING <condition>]
    /// [ORDER BY <expr> [ASC|DESC], ...]
    /// [LIMIT <n>]
    /// ```
    pub fn parse(src: &str) -> Result<Query> {
        let toks = lex(src)?;
        let mut p = Parser {
            src,
            toks,
            pos: 0,
            q: Query {
                select: Vec::new(),
                select_all: false,
                from: Vec::new(),
                filter: None,
                group_by: Vec::new(),
                having: None,
                order_by: Vec::new(),
                limit: None,
                aggs: Vec::new(),
                roots: Vec::new(),
                roots_used: Vec::new(),
            },
            agg_depth: 0,
        };
        p.query()?;
        p.q.finish()?;
        Ok(p.q)
    }

    /// True when the query aggregates (has aggregate functions or GROUP BY).
    pub(crate) fn is_aggregate(&self) -> bool {
        !self.aggs.is_empty() || !self.group_by.is_empty()
    }

    fn finish(&mut self) -> Result<()> {
        let aliases: Vec<(String, Expr)> = self.select.iter().map(|s| (s.name.clone(), s.expr.clone())).collect();
        let roots = self.roots.clone();
        let sub = |e: &Expr| subst_aliases(e, &roots, &aliases);
        self.group_by = self.group_by.iter().map(&sub).collect();
        self.having = self.having.as_ref().map(&sub);
        self.order_by = self.order_by.iter().map(|(e, d)| (sub(e), *d)).collect();

        if self.is_aggregate() {
            if self.select_all {
                return err("SELECT * cannot be combined with aggregation");
            }
            let groups = self.group_by.clone();
            for s in &mut self.select {
                s.expr = bind_post(&s.expr, &groups, &roots)?;
            }
            if let Some(h) = &self.having {
                self.having = Some(bind_post(h, &groups, &roots)?);
            }
            for (e, _) in &mut self.order_by {
                *e = bind_post(e, &groups, &roots)?;
            }
        } else if self.having.is_some() {
            return err("HAVING requires GROUP BY or an aggregate");
        }

        let mut used = vec![false; self.roots.len()];
        let mut mark = |i: usize| used[i] = true;
        for s in &self.select {
            visit_roots(&s.expr, &mut mark);
        }
        for e in self.filter.iter().chain(self.having.iter()).chain(self.group_by.iter()) {
            visit_roots(e, &mut mark);
        }
        for (e, _) in &self.order_by {
            visit_roots(e, &mut mark);
        }
        for a in &self.aggs {
            if let Some(e) = &a.arg {
                visit_roots(e, &mut mark);
            }
        }
        self.roots_used = used;
        Ok(())
    }
}

fn subst_aliases(e: &Expr, roots: &[String], aliases: &[(String, Expr)]) -> Expr {
    let s = |x: &Expr| subst_aliases(x, roots, aliases);
    let sb = |x: &Expr| Box::new(subst_aliases(x, roots, aliases));
    match e {
        Expr::Path { root, rest } if rest.is_empty() => {
            match aliases.iter().find(|(n, _)| *n == roots[*root]) {
                Some((_, target)) => target.clone(),
                None => e.clone(),
            }
        }
        Expr::Not(x) => Expr::Not(sb(x)),
        Expr::Neg(x) => Expr::Neg(sb(x)),
        Expr::Bin(op, l, r) => Expr::Bin(*op, sb(l), sb(r)),
        Expr::Func(n, a) => Expr::Func(n.clone(), a.iter().map(s).collect()),
        Expr::In { expr, list, negated } => Expr::In { expr: sb(expr), list: list.iter().map(s).collect(), negated: *negated },
        Expr::Between { expr, lo, hi, negated } => Expr::Between { expr: sb(expr), lo: sb(lo), hi: sb(hi), negated: *negated },
        Expr::Like { expr, pattern, negated } => Expr::Like { expr: sb(expr), pattern: sb(pattern), negated: *negated },
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: sb(expr), negated: *negated },
        other => other.clone(),
    }
}

fn visit_roots(e: &Expr, f: &mut dyn FnMut(usize)) {
    match e {
        Expr::Path { root, .. } => f(*root),
        Expr::Not(x) | Expr::Neg(x) => visit_roots(x, f),
        Expr::Bin(_, l, r) => {
            visit_roots(l, f);
            visit_roots(r, f);
        }
        Expr::Func(_, a) => a.iter().for_each(|x| visit_roots(x, f)),
        Expr::In { expr, list, .. } => {
            visit_roots(expr, f);
            list.iter().for_each(|x| visit_roots(x, f));
        }
        Expr::Between { expr, lo, hi, .. } => {
            visit_roots(expr, f);
            visit_roots(lo, f);
            visit_roots(hi, f);
        }
        Expr::Like { expr, pattern, .. } => {
            visit_roots(expr, f);
            visit_roots(pattern, f);
        }
        Expr::IsNull { expr, .. } => visit_roots(expr, f),
        _ => {}
    }
}

/// Rewrites an expression for evaluation after aggregation: group-by
/// expressions become `Group(i)`, everything else may only combine those and
/// aggregate results.
fn bind_post(e: &Expr, groups: &[Expr], roots: &[String]) -> Result<Expr> {
    if let Some(i) = groups.iter().position(|g| g == e) {
        return Ok(Expr::Group(i));
    }
    let b = |x: &Expr| bind_post(x, groups, roots).map(Box::new);
    Ok(match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(..) | Expr::Float(..) | Expr::Str(_) => e.clone(),
        Expr::Agg(_) | Expr::Group(_) => e.clone(),
        Expr::Path { root, rest } => {
            let mut name = roots[*root].clone();
            for s in rest {
                match s {
                    Seg::Name(n) => name.push_str(&format!(".{n}")),
                    Seg::Index(i) => name.push_str(&format!("[{i}]")),
                }
            }
            return err(format!("`{name}` must appear in GROUP BY or be used inside an aggregate function"));
        }
        Expr::Virt(v) => {
            let n = match v { Virt::Event => "$event", Virt::Chunk => "$chunk", Virt::Size => "$size" };
            return err(format!("`{n}` must appear in GROUP BY or be used inside an aggregate function"));
        }
        Expr::Not(x) => Expr::Not(b(x)?),
        Expr::Neg(x) => Expr::Neg(b(x)?),
        Expr::Bin(op, l, r) => Expr::Bin(*op, b(l)?, b(r)?),
        Expr::Func(n, a) => Expr::Func(n.clone(), a.iter().map(|x| bind_post(x, groups, roots)).collect::<Result<_>>()?),
        Expr::In { expr, list, negated } => Expr::In {
            expr: b(expr)?,
            list: list.iter().map(|x| bind_post(x, groups, roots)).collect::<Result<_>>()?,
            negated: *negated,
        },
        Expr::Between { expr, lo, hi, negated } => {
            Expr::Between { expr: b(expr)?, lo: b(lo)?, hi: b(hi)?, negated: *negated }
        }
        Expr::Like { expr, pattern, negated } => Expr::Like { expr: b(expr)?, pattern: b(pattern)?, negated: *negated },
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: b(expr)?, negated: *negated },
    })
}

impl<'s> Parser<'s> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if is_kw(s, kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.unexpected(&format!("`{}`", kw.to_ascii_uppercase()))
        }
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<()> {
        if self.eat(&t) { Ok(()) } else { self.unexpected(what) }
    }

    fn unexpected<T>(&self, wanted: &str) -> Result<T> {
        let t = &self.toks[self.pos];
        let found = if t.tok == Tok::Eof { "end of query".to_string() } else { format!("`{}`", &self.src[t.start..t.end]) };
        err(format!("expected {wanted} but found {found} at position {}", t.start))
    }

    fn root(&mut self, name: &str) -> usize {
        match self.q.roots.iter().position(|r| r == name) {
            Some(i) => i,
            None => {
                self.q.roots.push(name.to_string());
                self.q.roots.len() - 1
            }
        }
    }

    fn query(&mut self) -> Result<()> {
        self.expect_kw("select")?;
        if self.peek() == &Tok::Star {
            self.bump();
            self.q.select_all = true;
        } else {
            loop {
                let start = self.toks[self.pos].start;
                let expr = self.expr()?;
                let end = self.toks[self.pos.saturating_sub(1)].end;
                let mut name = self.src[start..end].trim().to_string();
                if self.eat_kw("as") {
                    match self.bump() {
                        Tok::Ident(a) => name = a,
                        Tok::Str(a) => name = a,
                        _ => return self.unexpected("an alias"),
                    }
                }
                self.q.select.push(SelectItem { expr, name });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect_kw("from")?;
        loop {
            let mut name = String::new();
            let mut last_end = None;
            loop {
                let t = self.toks[self.pos].clone();
                let part = match &t.tok {
                    Tok::Ident(s) if !(name.is_empty() && KEYWORDS.contains(&s.to_ascii_lowercase().as_str())) => s.clone(),
                    Tok::Star => "*".to_string(),
                    _ => break,
                };
                if last_end.is_some_and(|e| e != t.start) {
                    break;
                }
                name.push_str(&part);
                last_end = Some(t.end);
                self.bump();
            }
            if name.is_empty() {
                return self.unexpected("an event name");
            }
            self.q.from.push(name);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.eat_kw("where") {
            self.q.filter = Some(self.expr()?);
        }
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                self.q.group_by.push(e);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        if self.eat_kw("having") {
            self.q.having = Some(self.expr()?);
        }
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                let desc = if self.eat_kw("desc") {
                    true
                } else {
                    self.eat_kw("asc");
                    false
                };
                self.q.order_by.push((e, desc));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        if self.eat_kw("limit") {
            match self.bump() {
                Tok::Int(n, None) if n >= 0 => self.q.limit = Some(n as usize),
                _ => return self.unexpected("a non-negative integer after LIMIT"),
            }
        }
        if self.peek() != &Tok::Eof {
            return self.unexpected("end of query");
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut l = self.and_expr()?;
        while self.eat_kw("or") {
            let r = self.and_expr()?;
            l = Expr::Bin(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut l = self.not_expr()?;
        while self.eat_kw("and") {
            let r = self.not_expr()?;
            l = Expr::Bin(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            return Ok(Expr::Not(Box::new(self.not_expr()?)));
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let l = self.add_expr()?;
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let r = self.add_expr()?;
            return Ok(Expr::Bin(op, Box::new(l), Box::new(r)));
        }
        if self.at_kw("is") {
            self.bump();
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            return Ok(Expr::IsNull { expr: Box::new(l), negated });
        }
        let negated = if self.at_kw("not")
            && matches!(&self.toks[self.pos + 1].tok, Tok::Ident(s) if is_kw(s, "in") || is_kw(s, "like") || is_kw(s, "between"))
        {
            self.bump();
            true
        } else {
            false
        };
        if self.eat_kw("in") {
            self.expect(Tok::LParen, "`(`")?;
            let mut list = Vec::new();
            loop {
                list.push(self.expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "`)`")?;
            return Ok(Expr::In { expr: Box::new(l), list, negated });
        }
        if self.eat_kw("like") {
            let p = self.add_expr()?;
            return Ok(Expr::Like { expr: Box::new(l), pattern: Box::new(p), negated });
        }
        if self.eat_kw("between") {
            let lo = self.add_expr()?;
            self.expect_kw("and")?;
            let hi = self.add_expr()?;
            return Ok(Expr::Between { expr: Box::new(l), lo: Box::new(lo), hi: Box::new(hi), negated });
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut l = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.mul_expr()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.unary()?;
            l = Expr::Bin(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            return Ok(match self.unary()? {
                Expr::Int(v, u) => Expr::Int(-v, u),
                Expr::Float(v, u) => Expr::Float(-v, u),
                e => Expr::Neg(Box::new(e)),
            });
        }
        self.primary()
    }

    fn number(&mut self, v: f64, int: Option<i64>, sfx: Option<String>) -> Result<Expr> {
        match sfx {
            None => Ok(match int {
                Some(i) => Expr::Int(i, Unit::None),
                None => Expr::Float(v, Unit::None),
            }),
            Some(s) => {
                let (mult, unit) = unit_for_suffix(&s)?;
                if unit == Unit::Percentage {
                    return Ok(Expr::Float(v * mult, unit));
                }
                let scaled = v * mult;
                if scaled.fract() == 0.0 && scaled.abs() < 9.0e18 {
                    Ok(Expr::Int(scaled as i64, unit))
                } else {
                    Ok(Expr::Float(scaled, unit))
                }
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let t = self.toks[self.pos].clone();
        match t.tok {
            Tok::Int(v, sfx) => {
                self.bump();
                self.number(v as f64, Some(v), sfx)
            }
            Tok::Float(v, sfx) => {
                self.bump();
                self.number(v, None, sfx)
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(e)
            }
            Tok::Ident(name) => {
                self.bump();
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    "null" => return Ok(Expr::Null),
                    "true" => return Ok(Expr::Bool(true)),
                    "false" => return Ok(Expr::Bool(false)),
                    _ => {}
                }
                if self.peek() == &Tok::LParen {
                    return self.call(&name);
                }
                if KEYWORDS.contains(&lower.as_str()) {
                    self.pos -= 1;
                    return self.unexpected("an expression");
                }
                match lower.as_str() {
                    "$event" => return Ok(Expr::Virt(Virt::Event)),
                    "$chunk" => return Ok(Expr::Virt(Virt::Chunk)),
                    "$size" => return Ok(Expr::Virt(Virt::Size)),
                    _ => {}
                }
                self.path(&name)
            }
            _ => self.unexpected("an expression"),
        }
    }

    fn path(&mut self, name: &str) -> Result<Expr> {
        let mut parts = name.split('.');
        let first = parts.next().unwrap_or("");
        if first.is_empty() {
            return err(format!("invalid field path `{name}`"));
        }
        let mut rest = Vec::new();
        for p in parts {
            if p.is_empty() {
                return err(format!("invalid field path `{name}`"));
            }
            rest.push(match p.parse::<usize>() {
                Ok(i) => Seg::Index(i),
                Err(_) => Seg::Name(p.to_string()),
            });
        }
        while self.peek() == &Tok::LBracket {
            self.bump();
            match self.bump() {
                Tok::Int(i, None) if i >= 0 => rest.push(Seg::Index(i as usize)),
                _ => return self.unexpected("an array index"),
            }
            self.expect(Tok::RBracket, "`]`")?;
            if self.eat(&Tok::Dot) {
                match self.bump() {
                    Tok::Ident(more) => {
                        for p in more.split('.') {
                            rest.push(match p.parse::<usize>() {
                                Ok(i) => Seg::Index(i),
                                Err(_) => Seg::Name(p.to_string()),
                            });
                        }
                    }
                    _ => return self.unexpected("a field name"),
                }
            }
        }
        let root = self.root(first);
        Ok(Expr::Path { root, rest })
    }

    fn call(&mut self, name: &str) -> Result<Expr> {
        self.expect(Tok::LParen, "`(`")?;
        let lower = name.to_ascii_lowercase();
        let agg = match lower.as_str() {
            "count" => Some(AggKind::Count),
            "sum" => Some(AggKind::Sum),
            "avg" | "mean" => Some(AggKind::Avg),
            "min" => Some(AggKind::Min),
            "max" => Some(AggKind::Max),
            "median" | "p50" | "p75" | "p90" | "p95" | "p99" | "p999" | "percentile" => Some(AggKind::Percentile),
            _ => None,
        };
        let Some(mut kind) = agg else {
            let mut args = Vec::new();
            if self.peek() != &Tok::RParen {
                loop {
                    args.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(Tok::RParen, "`)`")?;
            return Ok(Expr::Func(lower, args));
        };
        if self.agg_depth > 0 {
            return err("aggregate functions cannot be nested");
        }
        self.agg_depth += 1;
        let start = self.toks[self.pos].start;
        let mut quantile = match lower.as_str() {
            "median" | "p50" => 0.5,
            "p75" => 0.75,
            "p90" => 0.9,
            "p95" => 0.95,
            "p99" => 0.99,
            "p999" => 0.999,
            _ => 0.0,
        };
        let mut arg = None;
        if self.peek() == &Tok::Star {
            if kind != AggKind::Count {
                return err(format!("`{name}(*)` is not valid"));
            }
            self.bump();
        } else {
            if kind == AggKind::Count && self.eat_kw("distinct") {
                kind = AggKind::CountDistinct;
            }
            arg = Some(self.expr()?);
            if kind == AggKind::Percentile && lower == "percentile" {
                self.expect(Tok::Comma, "`,` and a quantile")?;
                let q = match self.bump() {
                    Tok::Float(v, None) => v,
                    Tok::Int(v, None) => v as f64,
                    _ => return self.unexpected("a quantile between 0 and 1"),
                };
                if !(0.0..=1.0).contains(&q) {
                    return err("percentile quantile must be between 0 and 1");
                }
                quantile = q;
            }
        }
        let end = self.toks[self.pos].start;
        self.expect(Tok::RParen, "`)`")?;
        self.agg_depth -= 1;
        let text = format!("{lower}({})", self.src[start..end].trim());
        let spec = AggSpec { kind, arg, quantile, text };
        let idx = match self.q.aggs.iter().position(|a| *a == spec) {
            Some(i) => i,
            None => {
                self.q.aggs.push(spec);
                self.q.aggs.len() - 1
            }
        };
        Ok(Expr::Agg(idx))
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Null => f.write_str("null"),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Int(v, _) => write!(f, "{v}"),
            Expr::Float(v, _) => write!(f, "{v}"),
            Expr::Str(s) => write!(f, "'{s}'"),
            Expr::Path { root, rest } => {
                write!(f, "#{root}")?;
                for s in rest {
                    match s {
                        Seg::Name(n) => write!(f, ".{n}")?,
                        Seg::Index(i) => write!(f, "[{i}]")?,
                    }
                }
                Ok(())
            }
            Expr::Virt(v) => write!(f, "{v:?}"),
            Expr::Not(e) => write!(f, "NOT {e}"),
            Expr::Neg(e) => write!(f, "-{e}"),
            Expr::Bin(op, l, r) => write!(f, "({l} {op:?} {r})"),
            Expr::Func(n, a) => {
                write!(f, "{n}(")?;
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{x}")?;
                }
                f.write_str(")")
            }
            Expr::In { expr, list, negated } => write!(f, "{expr} {}IN {list:?}", if *negated { "NOT " } else { "" }),
            Expr::Between { expr, lo, hi, negated } => {
                write!(f, "{expr} {}BETWEEN {lo} AND {hi}", if *negated { "NOT " } else { "" })
            }
            Expr::Like { expr, pattern, negated } => write!(f, "{expr} {}LIKE {pattern}", if *negated { "NOT " } else { "" }),
            Expr::IsNull { expr, negated } => write!(f, "{expr} IS {}NULL", if *negated { "NOT " } else { "" }),
            Expr::Agg(i) => write!(f, "agg#{i}"),
            Expr::Group(i) => write!(f, "group#{i}"),
        }
    }
}
