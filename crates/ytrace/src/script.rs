//! The ytrace script plane — DTrace-class predicates and aggregates, evaluated
//! in-process at the probe site.
//!
//! Design laws (see `docs/spec-ytrace.md` §Scripts):
//! 1. **Scripts see every firing, unsampled.** Sampling is a FILE-stream policy;
//!    a `@quantize` that only sees 1:50 of fast frames is a lying instrument.
//! 2. **One semantics.** The whole clause compiles to the IR or fails to parse
//!    at attach with a precise error. There is no tier in which the same text
//!    means something different.
//! 3. **Bounded by construction.** No loops, no user code, no allocation on the
//!    unmatched path; group count, ring size, and capture bytes are capped, and
//!    overflow is counted, never silent.
//! 4. **Anti-false-zero.** `fired / matched / schema_miss` are distinct: "the
//!    probe never fired", "the predicate never matched", and "the record did
//!    not look the way the script assumed" are three different findings.
//! 5. **Drains ride the socket, not the plane.** Aggregate snapshots never
//!    consume the JSONL byte budget — an instrument must not shorten the
//!    diagnostic window it exists to extend.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};

/// Hard bounds — the verifier's job here is done by construction, but the
/// numbers must exist and be reported when they bite.
pub const MAX_GROUPS: usize = 1024;
pub const MAX_AGGREGATES: usize = 8;
pub const MAX_RING: usize = 4096;
pub const DEFAULT_RING: usize = 32;
pub const MAX_SCRIPT_BYTES: usize = 4096;
pub const CAPTURE_BYTES: usize = 4096;
const OVERFLOW_KEY: &str = "__overflow__";

// ── IR ──────────────────────────────────────────────────────────────────────

/// A field path. Bare names are record-header fields (`duration_ms`, `category`,
/// `component`, `name`, `clock`, `pid`, `app`, `ts_ms`); payload fields are
/// addressed explicitly: `payload.rows`, `payload.host_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub text: String,
    pub payload: bool,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Path(Path),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AggSpec {
    Count,
    Sum(Expr),
    Min(Expr),
    Max(Expr),
    Avg(Expr),
    Quantize(Expr),
}

impl AggSpec {
    pub fn kind(&self) -> &'static str {
        match self {
            AggSpec::Count => "count",
            AggSpec::Sum(_) => "sum",
            AggSpec::Min(_) => "min",
            AggSpec::Max(_) => "max",
            AggSpec::Avg(_) => "avg",
            AggSpec::Quantize(_) => "quantize",
        }
    }
}

/// A compiled script: one probe, one predicate, one group-key set, N aggregates,
/// an optional ring of captured records.
#[derive(Debug, Clone)]
pub struct Script {
    pub id: String,
    pub source: String,
    pub category: String,
    pub name: String,
    pub predicate: Option<Expr>,
    pub aggs: Vec<AggSpec>,
    pub by: Vec<Path>,
    pub keep: Vec<Path>,
    pub ring: usize, // 0 = ring disabled
}

/// Header view of a record, borrowed — the hot path must not allocate to read.
#[derive(Debug, Clone, Copy)]
pub struct RecRef<'a> {
    pub ts_ms: u128,
    pub pid: u32,
    pub app: &'a str,
    pub app_version: &'a str,
    pub component: &'a str,
    pub category: &'a str,
    pub name: &'a str,
    pub clock: &'a str,
    pub duration_ms: Option<f64>,
}

// ── evaluated values ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum V {
    Num(f64),
    Str(String),
    Bool(bool),
    Missing,
}

impl V {
    fn to_json(&self) -> Value {
        match self {
            V::Num(n) => json!(n),
            V::Str(s) => json!(s),
            V::Bool(b) => json!(b),
            V::Missing => Value::Null,
        }
    }
}

fn header_value<'a>(r: &RecRef<'a>, field: &str) -> V {
    match field {
        "ts_ms" => V::Num(r.ts_ms as f64),
        "pid" => V::Num(r.pid as f64),
        "app" => V::Str(r.app.to_string()),
        "app_version" => V::Str(r.app_version.to_string()),
        "component" => V::Str(r.component.to_string()),
        "category" => V::Str(r.category.to_string()),
        "name" => V::Str(r.name.to_string()),
        "clock" => V::Str(r.clock.to_string()),
        "duration_ms" => match r.duration_ms {
            Some(d) => V::Num(d),
            None => V::Missing,
        },
        _ => V::Missing,
    }
}

fn payload_value(payload: &Value, segments: &[String]) -> V {
    let mut cur = payload;
    for seg in segments {
        match cur.get(seg.as_str()) {
            Some(v) => cur = v,
            None => return V::Missing,
        }
    }
    match cur {
        Value::Null => V::Missing,
        Value::Number(n) => n
            .as_f64()
            .map(V::Num)
            .unwrap_or(V::Missing),
        Value::String(s) => V::Str(s.clone()),
        Value::Bool(b) => V::Bool(*b),
        _ => V::Missing, // objects/arrays are not scalar values to a predicate
    }
}

/// Resolve a path against a record — payload paths walk the payload object,
/// bare paths are header fields.
fn path_value(p: &Path, r: &RecRef, payload: &Value) -> V {
    if p.payload {
        payload_value(payload, &p.segments)
    } else {
        header_value(r, p.segments.first().map(|s| s.as_str()).unwrap_or(""))
    }
}

/// Evaluate an expression. `schema_miss` is set when a path is missing or a
/// value has the wrong type — once per event, at the top level, not per path.
fn eval_expr(expr: &Expr, r: &RecRef, payload: &Value, schema_miss: &mut bool) -> V {
    match expr {
        Expr::Num(n) => V::Num(*n),
        Expr::Str(s) => V::Str(s.clone()),
        Expr::Bool(b) => V::Bool(*b),
        Expr::Path(p) => {
            if p.payload {
                payload_value(payload, &p.segments)
            } else {
                header_value(r, p.segments.first().map(|s| s.as_str()).unwrap_or(""))
            }
        }
        Expr::Arith(op, a, b) => {
            let va = eval_expr(a, r, payload, schema_miss);
            let vb = eval_expr(b, r, payload, schema_miss);
            match (va, vb) {
                (V::Num(x), V::Num(y)) => V::Num(match op {
                    ArithOp::Add => x + y,
                    ArithOp::Sub => x - y,
                    ArithOp::Mul => x * y,
                    ArithOp::Div => {
                        if y == 0.0 {
                            *schema_miss = true;
                            f64::NAN
                        } else {
                            x / y
                        }
                    }
                }),
                (V::Missing, _) | (_, V::Missing) => {
                    *schema_miss = true;
                    V::Missing
                }
                _ => {
                    *schema_miss = true;
                    V::Missing
                }
            }
        }
        Expr::Cmp(op, a, b) => {
            let va = eval_expr(a, r, payload, schema_miss);
            let vb = eval_expr(b, r, payload, schema_miss);
            let res = match (&va, &vb) {
                (V::Num(x), V::Num(y)) => match op {
                    CmpOp::Eq => x == y,
                    CmpOp::Ne => x != y,
                    CmpOp::Lt => x < y,
                    CmpOp::Le => x <= y,
                    CmpOp::Gt => x > y,
                    CmpOp::Ge => x >= y,
                },
                (V::Str(x), V::Str(y)) => match op {
                    CmpOp::Eq => x == y,
                    CmpOp::Ne => x != y,
                    CmpOp::Lt => x < y,
                    CmpOp::Le => x <= y,
                    CmpOp::Gt => x > y,
                    CmpOp::Ge => x >= y,
                },
                (V::Bool(x), V::Bool(y)) => match op {
                    CmpOp::Eq => x == y,
                    CmpOp::Ne => x != y,
                    _ => {
                        *schema_miss = true;
                        return V::Bool(false);
                    }
                },
                (V::Missing, _) | (_, V::Missing) => {
                    *schema_miss = true;
                    return V::Bool(false);
                }
                _ => {
                    *schema_miss = true;
                    return V::Bool(false);
                }
            };
            V::Bool(res)
        }
        Expr::And(a, b) => {
            let va = eval_expr(a, r, payload, schema_miss);
            if va == V::Bool(false) {
                return V::Bool(false); // short-circuit; b may reference absent fields legitimately
            }
            let vb = eval_expr(b, r, payload, schema_miss);
            match (va, vb) {
                (V::Bool(x), V::Bool(y)) => V::Bool(x && y),
                _ => {
                    *schema_miss = true;
                    V::Bool(false)
                }
            }
        }
        Expr::Or(a, b) => {
            let va = eval_expr(a, r, payload, schema_miss);
            if va == V::Bool(true) {
                return V::Bool(true);
            }
            let vb = eval_expr(b, r, payload, schema_miss);
            match (va, vb) {
                (V::Bool(x), V::Bool(y)) => V::Bool(x || y),
                _ => {
                    *schema_miss = true;
                    V::Bool(false)
                }
            }
        }
        Expr::Not(a) => match eval_expr(a, r, payload, schema_miss) {
            V::Bool(b) => V::Bool(!b),
            _ => {
                *schema_miss = true;
                V::Bool(false)
            }
        },
    }
}

// ── aggregate state ─────────────────────────────────────────────────────────

#[derive(Default)]
struct GroupAggs {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    n: u64, // numeric observations (avg denominator)
    quant: Option<Box<[u64; 64]>>,
    quant_min: f64,
    quant_max: f64,
}

impl GroupAggs {
    fn new(aggs: &[AggSpec]) -> Self {
        let quant = if aggs.iter().any(|a| matches!(a, AggSpec::Quantize(_))) {
            Some(Box::new([0u64; 64]))
        } else {
            None
        };
        GroupAggs {
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            n: 0,
            quant,
            quant_min: f64::INFINITY,
            quant_max: f64::NEG_INFINITY,
        }
    }

    fn observe(&mut self, aggs: &[AggSpec], values: &[Option<f64>]) {
        self.count += 1;
        for (spec, val) in aggs.iter().zip(values.iter()) {
            let Some(v) = val else { continue };
            match spec {
                AggSpec::Count => {}
                AggSpec::Sum(_) | AggSpec::Avg(_) => {
                    self.sum += v;
                    self.n += 1;
                }
                AggSpec::Min(_) => {
                    if *v < self.min {
                        self.min = *v;
                    }
                }
                AggSpec::Max(_) => {
                    if *v > self.max {
                        self.max = *v;
                    }
                }
                AggSpec::Quantize(_) => {
                    if let Some(q) = self.quant.as_mut() {
                        let b = bucket_of(*v);
                        q[b] += 1;
                        if *v < self.quant_min {
                            self.quant_min = *v;
                        }
                        if *v > self.quant_max {
                            self.quant_max = *v;
                        }
                    }
                }
            }
        }
    }
}

/// log2 histogram bucket: value v>0 lands in floor(log2(v)) clamped to 0..=63.
fn bucket_of(v: f64) -> usize {
    if v <= 0.0 {
        return 0;
    }
    let b = v.log2().floor() as i32;
    b.clamp(0, 63) as usize
}

fn bucket_value(b: usize) -> f64 {
    // representative value: the lower edge — bucket b holds [2^b, 2^(b+1))
    2u64.pow(b as u32) as f64
}

fn percentile_from_quant(q: &[u64; 64], p: f64) -> Option<f64> {
    let total: u64 = q.iter().sum();
    if total == 0 {
        return None;
    }
    let target = ((total as f64) * p).ceil() as u64;
    let mut cum = 0u64;
    for (b, c) in q.iter().enumerate() {
        cum += c;
        if cum >= target {
            return Some(bucket_value(b));
        }
    }
    None
}

// ── script state ────────────────────────────────────────────────────────────

pub struct ScriptState {
    pub script: Script,
    fired: u64,
    matched: u64,
    schema_miss: u64,
    overflow_groups: u64,
    ring_dropped: u64,
    groups: HashMap<String, (Vec<V>, GroupAggs)>,
    ring: VecDeque<Value>,
}

impl ScriptState {
    pub fn new(script: Script) -> Self {
        let ring = if script.ring > 0 {
            VecDeque::with_capacity(script.ring.min(64))
        } else {
            VecDeque::new()
        };
        ScriptState {
            script,
            fired: 0,
            matched: 0,
            schema_miss: 0,
            overflow_groups: 0,
            ring_dropped: 0,
            groups: HashMap::new(),
            ring,
        }
    }

    /// One probe firing. This is the hot path: allocation happens only after
    /// the predicate matched.
    pub fn eval(&mut self, r: &RecRef, payload: &Value) {
        self.fired += 1;
        let mut miss = false;
        let hit = match &self.script.predicate {
            None => true,
            Some(p) => matches!(eval_expr(p, r, payload, &mut miss), V::Bool(true)),
        };
        if miss {
            self.schema_miss += 1;
        }
        if !hit {
            return;
        }
        self.matched += 1;

        // group key — canonical JSON; bounded by MAX_GROUPS
        let (key_str, group_vals) = if self.script.by.is_empty() {
            ("()".to_string(), Vec::new())
        } else {
            let vals: Vec<V> = self
                .script
                .by
                .iter()
                .map(|p| path_value(p, r, payload))
                .collect();
            let key = serde_json::to_string(
                &vals.iter().map(|v| v.to_json()).collect::<Vec<Value>>(),
            )
            .unwrap_or_else(|_| "()".to_string());
            (key, vals)
        };

        let entry = match self.groups.get_mut(&key_str) {
            Some(e) => e,
            None => {
                if self.groups.len() >= MAX_GROUPS {
                    self.overflow_groups += 1;
                    self.groups
                        .entry(OVERFLOW_KEY.to_string())
                        .or_insert_with(|| {
                            (
                                vec![V::Str(OVERFLOW_KEY.to_string())],
                                GroupAggs::new(&self.script.aggs),
                            )
                        })
                } else {
                    self.groups.insert(
                        key_str.clone(),
                        (group_vals, GroupAggs::new(&self.script.aggs)),
                    );
                    self.groups.get_mut(&key_str).unwrap()
                }
            }
        };

        // evaluate agg arguments once each
        let values: Vec<Option<f64>> = self
            .script
            .aggs
            .iter()
            .map(|spec| match spec {
                AggSpec::Count => None,
                _ => {
                    let expr = match spec {
                        AggSpec::Sum(e) | AggSpec::Min(e) | AggSpec::Max(e) | AggSpec::Avg(e)
                        | AggSpec::Quantize(e) => e,
                        AggSpec::Count => unreachable!(),
                    };
                    match eval_expr(expr, r, payload, &mut miss) {
                        V::Num(n) if n.is_finite() => Some(n),
                        V::Missing => None,
                        _ => None,
                    }
                }
            })
            .collect();
        if miss {
            self.schema_miss += 1;
        }
        entry.1.observe(&self.script.aggs, &values);

        // ring capture
        if self.script.ring > 0 {
            let rec = capture_record(r, payload, &self.script.keep);
            if self.ring.len() >= self.script.ring {
                self.ring.pop_front();
                self.ring_dropped += 1;
            }
            self.ring.push_back(rec);
        }
    }

    /// Snapshot for drain. Zero the state afterwards via [`ScriptState::reset`]
    /// (under the same lock, so a rate view is atomic against emitters).
    pub fn drain(&self) -> Value {
        let mut groups_json = Vec::with_capacity(self.groups.len());
        // newest last; sort overflow to the end for readability
        let mut keys: Vec<_> = self.groups.keys().cloned().collect();
        keys.sort();
        keys.sort_by_key(|k| k.contains(OVERFLOW_KEY));
        for k in &keys {
            let (vals, g) = &self.groups[k];
            let mut o = Map::new();
            o.insert(
                "key".into(),
                if self.script.by.is_empty() {
                    Value::Null
                } else {
                    Value::Array(vals.iter().map(|v| v.to_json()).collect())
                },
            );
            o.insert("count".into(), json!(g.count));
            if self.script.aggs.iter().any(|a| matches!(a, AggSpec::Sum(_) | AggSpec::Avg(_))) {
                o.insert("sum".into(), json!(g.sum));
                o.insert("avg".into(), if g.n > 0 { json!(g.sum / g.n as f64) } else { Value::Null });
            }
            if self.script.aggs.iter().any(|a| matches!(a, AggSpec::Min(_))) {
                o.insert("min".into(), json!(if g.min.is_finite() { g.min } else { 0.0 }));
            }
            if self.script.aggs.iter().any(|a| matches!(a, AggSpec::Max(_))) {
                o.insert("max".into(), json!(if g.max.is_finite() { g.max } else { 0.0 }));
            }
            if let Some(q) = &g.quant {
                o.insert("quantize".into(), json!({
                    "min": if g.quant_min.is_finite() { g.quant_min } else { 0.0 },
                    "max": if g.quant_max.is_finite() { g.quant_max } else { 0.0 },
                    "p50": percentile_from_quant(q, 0.50),
                    "p95": percentile_from_quant(q, 0.95),
                    "p99": percentile_from_quant(q, 0.99),
                    "buckets": q.iter().enumerate()
                        .filter(|(_, c)| **c > 0)
                        .map(|(b, c)| json!({">=": bucket_value(b), "<": if b < 63 { bucket_value(b+1) } else { f64::INFINITY }, "count": c}))
                        .collect::<Vec<_>>(),
                }));
            }
            groups_json.push(Value::Object(o));
        }
        let out = json!({
            "id": self.script.id,
            "script": self.script.source,
            "probe": format!("{}/{}", self.script.category, self.script.name),
            "stats": {
                "fired": self.fired,
                "matched": self.matched,
                "schema_miss": self.schema_miss,
                "overflow_groups": self.overflow_groups,
                "ring_dropped": self.ring_dropped,
                "groups": self.groups.len(),
            },
            "groups": groups_json,
            "ring": if self.script.ring > 0 { Value::Array(self.ring.iter().cloned().collect()) } else { Value::Null },
        });
        out
    }

    pub fn reset(&mut self) {
        self.fired = 0;
        self.matched = 0;
        self.schema_miss = 0;
        self.overflow_groups = 0;
        self.ring_dropped = 0;
        self.groups.clear();
        self.ring.clear();
    }

    pub fn stats_line(&self) -> String {
        format!(
            "fired={} matched={} schema_miss={} groups={}{}",
            self.fired,
            self.matched,
            self.schema_miss,
            self.groups.len(),
            if self.script.ring > 0 {
                format!(" ring={}/{}", self.ring.len(), self.script.ring)
            } else {
                String::new()
            }
        )
    }
}

/// tiny helper so the group-key computation can hand back both forms
// (removed — the group key is computed inline, plain and legible)

/// Build the ring-capture record. Header basics are always included; `keep`
/// paths add selected fields. Captures larger than `CAPTURE_BYTES` are
/// truncated to a marker — never silently, the marker is visible.
fn capture_record(r: &RecRef, payload: &Value, keep: &[Path]) -> Value {
    let mut o = Map::new();
    o.insert("ts_ms".into(), json!(r.ts_ms));
    o.insert("component".into(), json!(r.component));
    o.insert("category".into(), json!(r.category));
    o.insert("name".into(), json!(r.name));
    if let Some(d) = r.duration_ms {
        o.insert("duration_ms".into(), json!(d));
    }
    for p in keep {
        if p.payload && p.segments.is_empty() {
            o.insert("payload".into(), payload.clone());
        } else if p.payload {
            let v = payload_value(payload, &p.segments);
            o.insert(p.text.clone(), v.to_json());
        } else {
            let v = header_value(r, p.segments.first().map(|s| s.as_str()).unwrap_or(""));
            o.insert(p.text.clone(), v.to_json());
        }
    }
    let mut v = Value::Object(o);
    if let Ok(s) = serde_json::to_string(&v) {
        if s.len() > CAPTURE_BYTES {
            if let Value::Object(map) = &mut v {
                map.insert("payload".into(), json!({"_truncated": true, "bytes": s.len()}));
            }
        }
    }
    v
}

// ── parser ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),   // also dotted paths: payload.rows
    Num(f64),
    Str(String),
    Arrow,  // ->
    Comma,
    LParen,
    RParen,
    At,     // @
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or, Not,
    Plus, Minus, Star, Slash,
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '-' if i + 1 < b.len() && b[i + 1] == b'>' => {
                out.push((Tok::Arrow, i));
                i += 2;
            }
            '&' if i + 1 < b.len() && b[i + 1] == b'&' => {
                out.push((Tok::And, i));
                i += 2;
            }
            '|' if i + 1 < b.len() && b[i + 1] == b'|' => {
                out.push((Tok::Or, i));
                i += 2;
            }
            '=' if i + 1 < b.len() && b[i + 1] == b'=' => {
                out.push((Tok::Eq, i));
                i += 2;
            }
            '!' if i + 1 < b.len() && b[i + 1] == b'=' => {
                out.push((Tok::Ne, i));
                i += 2;
            }
            '<' if i + 1 < b.len() && b[i + 1] == b'=' => {
                out.push((Tok::Le, i));
                i += 2;
            }
            '>' if i + 1 < b.len() && b[i + 1] == b'=' => {
                out.push((Tok::Ge, i));
                i += 2;
            }
            '<' => {
                out.push((Tok::Lt, i));
                i += 1;
            }
            '>' => {
                out.push((Tok::Gt, i));
                i += 1;
            }
            '!' => {
                out.push((Tok::Not, i));
                i += 1;
            }
            '+' => {
                out.push((Tok::Plus, i));
                i += 1;
            }
            '-' => {
                out.push((Tok::Minus, i));
                i += 1;
            }
            '*' => {
                out.push((Tok::Star, i));
                i += 1;
            }
            '/' => {
                out.push((Tok::Slash, i));
                i += 1;
            }
            ',' => {
                out.push((Tok::Comma, i));
                i += 1;
            }
            '(' => {
                out.push((Tok::LParen, i));
                i += 1;
            }
            ')' => {
                out.push((Tok::RParen, i));
                i += 1;
            }
            '@' => {
                out.push((Tok::At, i));
                i += 1;
            }
            '"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                while i < b.len() && b[i] != b'"' {
                    s.push(b[i] as char);
                    i += 1;
                }
                if i >= b.len() {
                    return Err(format!("unterminated string literal at byte {start}"));
                }
                i += 1;
                out.push((Tok::Str(s), start));
            }
            '0'..='9' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                let txt = &src[start..i];
                let n: f64 = txt
                    .parse()
                    .map_err(|_| format!("bad number `{txt}` at byte {start}"))?;
                out.push((Tok::Num(n), start));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < b.len() {
                    let ch = b[i] as char;
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push((Tok::Ident(src[start..i].to_string()), start));
            }
            other => return Err(format!("unexpected byte `{other}` at byte {i}")),
        }
    }
    Ok(out)
}

/// Validate and build a path from dotted text. `payload.x.y` walks the payload;
/// bare names are header fields. Consistency for both predicates and keep items.
fn make_path(s: String, at: usize) -> Result<Path, String> {
    let payload = s == "payload" || s.starts_with("payload.");
    let segments: Vec<String> = s.split('.').map(|x| x.to_string()).collect();
    if segments.iter().any(|x| x.is_empty()) {
        return Err(format!("bad path `{s}` at byte {at}"));
    }
    Ok(Path {
        text: s,
        payload,
        segments: if payload { segments[1..].to_vec() } else { segments },
    })
}

struct P {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }
    fn next(&mut self) -> Option<(Tok, usize)> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, t: Tok) -> Result<(), String> {
        match self.next() {
            Some((got, _at)) if got == t => Ok(()),
            Some((got, at)) => Err(format!(
                "expected {t:?} but found {got:?} at byte {at}"
            )),
            None => Err(format!("expected {t:?} but script ended")),
        }
    }
    fn eat_ident(&mut self, kw: &str) -> bool {
        if let Some(Tok::Ident(s)) = self.peek() {
            if s == kw {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn path(&mut self) -> Result<Path, String> {
        match self.next() {
            Some((Tok::Ident(s), at)) => make_path(s, at),
            Some((got, at)) => Err(format!("expected a field path, found {got:?} at byte {at}")),
            None => Err("expected a field path but script ended".into()),
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.not()?;
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            let right = self.not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.cmp()
    }
    fn cmp(&mut self) -> Result<Expr, String> {
        let left = self.sum()?;
        let op = match self.peek() {
            Some(&Tok::Eq) => Some(CmpOp::Eq),
            Some(&Tok::Ne) => Some(CmpOp::Ne),
            Some(&Tok::Lt) => Some(CmpOp::Lt),
            Some(&Tok::Le) => Some(CmpOp::Le),
            Some(&Tok::Gt) => Some(CmpOp::Gt),
            Some(&Tok::Ge) => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.pos += 1;
            let right = self.sum()?;
            return Ok(Expr::Cmp(op, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }
    fn sum(&mut self) -> Result<Expr, String> {
        let mut left = self.term()?;
        loop {
            match self.peek() {
                Some(&Tok::Plus) => {
                    self.pos += 1;
                    let r = self.term()?;
                    left = Expr::Arith(ArithOp::Add, Box::new(left), Box::new(r));
                }
                Some(&Tok::Minus) => {
                    self.pos += 1;
                    let r = self.term()?;
                    left = Expr::Arith(ArithOp::Sub, Box::new(left), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(left)
    }
    fn term(&mut self) -> Result<Expr, String> {
        let mut left = self.unit()?;
        loop {
            match self.peek() {
                Some(&Tok::Star) => {
                    self.pos += 1;
                    let r = self.unit()?;
                    left = Expr::Arith(ArithOp::Mul, Box::new(left), Box::new(r));
                }
                Some(&Tok::Slash) => {
                    self.pos += 1;
                    let r = self.unit()?;
                    left = Expr::Arith(ArithOp::Div, Box::new(left), Box::new(r));
                }
                _ => break,
            }
        }
        Ok(left)
    }
    fn unit(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some((Tok::Num(n), _)) => Ok(Expr::Num(n)),
            Some((Tok::Str(s), _)) => Ok(Expr::Str(s)),
            Some((Tok::Ident(s), _)) => {
                if s == "true" {
                    Ok(Expr::Bool(true))
                } else if s == "false" {
                    Ok(Expr::Bool(false))
                } else {
                    Ok(Expr::Path(make_path(s, 0)?))
                }
            }
            Some((Tok::LParen, _)) => {
                let e = self.or()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Some((got, at)) => Err(format!("unexpected {got:?} at byte {at}")),
            None => Err("expression ended unexpectedly".into()),
        }
    }
}

/// Compile a clause. Grammar (one screen — that is the whole language):
///
/// ```text
/// script := PROBE ["where" or] ["->" agg ("," agg)*] ["by" path ("," path)*]
///           ["keep" keepitem ("," keepitem)*]
/// agg    := "@" ("count" | ("sum"|"min"|"max"|"avg"|"quantize") sum_expr)
/// keepitem := "ring" NUMBER | path
/// ```
///
/// PROBE is `category/name`. Bare paths are header fields; payload fields are
/// `payload.x.y`. Non-goals: loops, variables, user functions, string
/// transforms, joins across probes, side effects.
pub fn parse(src: &str, id: Option<String>) -> Result<Script, String> {
    if src.len() > MAX_SCRIPT_BYTES {
        return Err(format!("script exceeds {MAX_SCRIPT_BYTES} bytes"));
    }
    let src = src.trim();
    // probe: first whitespace-delimited chunk, exactly one '/'
    let probe_end = src
        .find(char::is_whitespace)
        .unwrap_or(src.len());
    let probe = &src[..probe_end];
    let (category, name) = probe
        .split_once('/')
        .filter(|(c, n)| !c.is_empty() && !n.is_empty())
        .ok_or_else(|| format!("probe must be `category/name`, got `{probe}`"))?;
    let (category, name) = (category.to_string(), name.to_string());
    let rest = &src[probe_end..];
    let toks = lex(rest)?;
    let mut p = P { toks, pos: 0 };

    let mut predicate = None;
    let mut aggs = Vec::new();
    let mut by = Vec::new();
    let mut keep = Vec::new();
    let mut ring: usize = 0;

    if p.eat_ident("where") {
        predicate = Some(p.or()?);
    }
    if p.peek() == Some(&Tok::Arrow) {
        p.pos += 1;
        loop {
            p.expect(Tok::At)?;
            let kind = match p.next() {
                Some((Tok::Ident(k), _)) => k,
                Some((got, at)) => return Err(format!("expected aggregate name after `@`, found {got:?} at byte {at}")),
                None => return Err("expected aggregate after `->`".into()),
            };
            match kind.as_str() {
                "count" => aggs.push(AggSpec::Count),
                "sum" => aggs.push(AggSpec::Sum(p.sum()?)),
                "min" => aggs.push(AggSpec::Min(p.sum()?)),
                "max" => aggs.push(AggSpec::Max(p.sum()?)),
                "avg" => aggs.push(AggSpec::Avg(p.sum()?)),
                "quantize" => aggs.push(AggSpec::Quantize(p.sum()?)),
                other => {
                    return Err(format!(
                        "unknown aggregate `@{other}` (known: count sum min max avg quantize)"
                    ))
                }
            }
            if p.peek() == Some(&Tok::Comma) {
                p.pos += 1;
                continue;
            }
            break;
        }
    }
    if aggs.len() > MAX_AGGREGATES {
        return Err(format!("at most {MAX_AGGREGATES} aggregates per script"));
    }
    if p.eat_ident("by") {
        loop {
            by.push(p.path()?);
            if p.peek() == Some(&Tok::Comma) {
                p.pos += 1;
                continue;
            }
            break;
        }
    }
    if p.eat_ident("keep") {
        loop {
            keep.push(p.path()?);
            if p.peek() == Some(&Tok::Comma) {
                p.pos += 1;
                continue;
            }
            break;
        }
    }
    // `ring N` is a bare clause word (no comma): `keep payload.host_id ring 32`
    if p.eat_ident("ring") {
        match p.next() {
            Some((Tok::Num(n), _)) => {
                ring = n as usize;
                if ring > MAX_RING {
                    return Err(format!("ring cap is {MAX_RING}"));
                }
            }
            Some((got, at)) => return Err(format!("ring needs a number, found {got:?} at byte {at}")),
            None => return Err("ring needs a number".into()),
        }
    }
    if let Some((got, at)) = p.next() {
        return Err(format!("trailing {got:?} at byte {at} — the grammar ends after where/->/by/keep"));
    }
    if ring == 0 && !keep.is_empty() {
        // keep without ring has nothing to keep into; default a small ring
        ring = DEFAULT_RING;
    }
    let id = id.unwrap_or_else(|| default_id(probe, &aggs));
    Ok(Script {
        id,
        source: src.to_string(),
        category: category.to_string(),
        name: name.to_string(),
        predicate,
        aggs,
        by,
        keep,
        ring,
    })
}

fn default_id(probe: &str, aggs: &[AggSpec]) -> String {
    if aggs.is_empty() {
        format!("{}@watch", probe)
    } else {
        format!("{}@{}", probe, aggs[0].kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(duration_ms: Option<f64>) -> (RecRef<'static>, Value) {
        // 'static is safe here: every &str below is a literal
        let r = RecRef {
            ts_ms: 1_000,
            pid: 42,
            app: "testapp",
            app_version: "0.0.0",
            component: "ui",
            category: "render",
            name: "gui",
            clock: "cpu",
            duration_ms,
        };
        (r, json!({"rows": 54, "host_id": "a"}))
    }

    #[test]
    fn parses_the_full_clause() {
        let s = parse(
            "render/gui where duration_ms > 16 -> @quantize(duration_ms / payload.rows * 1000), @count by payload.host_id keep payload, duration_ms ring 32",
            None,
        )
        .unwrap();
        assert_eq!(s.category, "render");
        assert_eq!(s.name, "gui");
        assert!(s.predicate.is_some());
        assert_eq!(s.aggs.len(), 2);
        assert_eq!(s.aggs[0].kind(), "quantize");
        assert_eq!(s.aggs[1].kind(), "count");
        assert_eq!(s.by.len(), 1);
        assert!(s.by[0].payload);
        assert_eq!(s.keep.len(), 2);
        assert_eq!(s.ring, 32);
        assert_eq!(s.id, "render/gui@quantize");
    }

    #[test]
    fn parse_errors_are_precise() {
        let e = parse("render/gui -> @nosuch", None).unwrap_err();
        assert!(e.contains("unknown aggregate `@nosuch`"), "{e}");
        let e = parse("render/gui where duration_ms > ", None).unwrap_err();
        assert!(!e.is_empty());
        let e = parse("rendergui", None).unwrap_err();
        assert!(e.contains("category/name"), "{e}");
        let e = parse("render/gui -> @count extra_junk", None).unwrap_err();
        assert!(e.contains("trailing"), "{e}");
        let e = parse("render/gui \"unterminated", None).unwrap_err();
        assert!(e.contains("unterminated string"), "{e}");
    }

    #[test]
    fn precedence_and_parens() {
        // && binds tighter than ||
        let s = parse("x/y where a == 1 || b == 2 && c == 3", None).unwrap();
        // matches: a==1, or (b==2 and c==3)
        let (r, payload) = rec(None);
        let mut miss = false;
        let hit = matches!(
            eval_expr(s.predicate.as_ref().unwrap(), &r, &payload, &mut miss),
            V::Bool(true)
        );
        // a/b/c all missing → false, schema_miss
        assert!(!hit);
        assert!(miss);
    }

    #[test]
    fn missing_field_is_schema_miss_not_silence() {
        let s = parse("render/gui where payload.nope == 1", None).unwrap();
        let mut st = ScriptState::new(s);
        let (r, payload) = rec(Some(10.0));
        st.eval(&r, &payload);
        let d = st.drain();
        assert_eq!(d["stats"]["fired"], 1);
        assert_eq!(d["stats"]["matched"], 0);
        assert_eq!(d["stats"]["schema_miss"], 1, "a wrong field is a finding, not silence");
    }

    #[test]
    fn quantize_buckets_and_percentiles() {
        let s = parse("render/gui where duration_ms > 16 -> @quantize(duration_ms)", None).unwrap();
        let mut st = ScriptState::new(s);
        for ms in [17.0, 20.0, 33.0, 100.0, 17.0] {
            let (r, payload) = rec(Some(ms));
            st.eval(&r, &payload);
        }
        let d = st.drain();
        assert_eq!(d["stats"]["matched"], 5);
        let q = &d["groups"][0]["quantize"];
        assert_eq!(q["min"], 17.0);
        assert_eq!(q["max"], 100.0);
        // 17,17,20 in bucket >=16<32; 33 in >=32<64; 100 in >=64<128
        assert_eq!(q["p50"], 16.0);
        assert_eq!(q["p99"], 64.0);
        assert_eq!(q["buckets"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn sampling_does_not_starve_scripts() {
        // a noisy probe (floor 8ms, 1:50) — scripts still see every firing
        let s = parse("render/gui -> @count", None).unwrap();
        let mut st = ScriptState::new(s);
        // simulates what Provider does: eval every firing, sample only the file stream
        for ms in (0..100).map(|i| i as f64) {
            let (r, payload) = rec(Some(ms));
            st.eval(&r, &payload);
        }
        let d = st.drain();
        assert_eq!(d["stats"]["fired"], 100);
        assert_eq!(d["stats"]["matched"], 100, "scripts see unsampled firings");
    }

    #[test]
    fn by_grouping_and_overflow_cap() {
        let s = parse("render/gui -> @count by payload.host_id", None).unwrap();
        let mut st = ScriptState::new(s);
        for i in 0..(MAX_GROUPS + 50) {
            let payload = json!({"host_id": format!("h{i}")});
            let (r, _) = rec(Some(1.0));
            st.eval(&r, &payload);
        }
        let d = st.drain();
        // MAX_GROUPS real groups, plus the one overflow bucket
        assert_eq!(d["stats"]["groups"], MAX_GROUPS + 1, "group count is capped (+1 overflow bucket)");
        assert_eq!(d["stats"]["overflow_groups"], 50, "overflow is counted, not silent");
        let overflow = d["groups"].as_array().unwrap().last().unwrap();
        assert_eq!(overflow["key"][0], OVERFLOW_KEY);
        assert_eq!(overflow["count"], 50);
    }

    #[test]
    fn ring_eviction_and_keep_filtering() {
        let s = parse("render/gui -> @count keep payload.host_id, duration_ms ring 2", None).unwrap();
        let mut st = ScriptState::new(s);
        for i in 0..3 {
            let payload = json!({"host_id": format!("h{i}"), "big": vec![0u8; 100]});
            let (r, _) = rec(Some(i as f64));
            st.eval(&r, &payload);
        }
        let d = st.drain();
        assert_eq!(d["stats"]["ring_dropped"], 1, "oldest evicted, counted");
        let ring = d["ring"].as_array().unwrap();
        assert_eq!(ring.len(), 2);
        // newest last; keep only selected fields
        assert!(ring[1].get("payload").is_none(), "unselected payload keys are filtered out");
        assert_eq!(ring[1]["payload.host_id"], "h2");
        assert_eq!(ring[1]["duration_ms"], 2.0);
        assert_eq!(ring[1]["category"], "render");
    }

    #[test]
    fn capture_over_budget_is_truncated_visibly() {
        let s = parse("render/gui -> @count keep payload ring 4", None).unwrap();
        let mut st = ScriptState::new(s);
        let payload = json!({"blob": "x".repeat(20_000)});
        let (r, _) = rec(Some(1.0));
        st.eval(&r, &payload);
        let d = st.drain();
        let ring = d["ring"].as_array().unwrap();
        assert_eq!(ring[0]["payload"]["_truncated"], true, "truncation is a marker, not silence");
    }

    #[test]
    fn drain_reset_zeroes() {
        let s = parse("render/gui -> @count", None).unwrap();
        let mut st = ScriptState::new(s);
        let (r, payload) = rec(Some(1.0));
        st.eval(&r, &payload);
        st.eval(&r, &payload);
        assert_eq!(st.drain()["stats"]["matched"], 2);
        st.reset();
        let d = st.drain();
        assert_eq!(d["stats"]["fired"], 0);
        assert_eq!(d["stats"]["matched"], 0);
    }

    #[test]
    fn arithmetic_in_agg_args_the_us_per_row_case() {
        // the render slope case from the fleet's measurements: µs per row
        let s = parse("render/gui -> @quantize(duration_ms / payload.rows * 1000)", None).unwrap();
        let mut st = ScriptState::new(s);
        let (r, payload) = rec(Some(2.7)); // 2.7ms / 54 rows ≈ 50µs
        st.eval(&r, &payload);
        let d = st.drain();
        let q = &d["groups"][0]["quantize"];
        assert_eq!(q["buckets"].as_array().unwrap().len(), 1);
        assert_eq!(q["buckets"][0][">="], 32.0, "2.7/54*1000 ≈ 50 lands in the 32..64 bucket");
    }

    #[test]
    fn string_predicates_and_short_circuit() {
        let s = parse("render/gui where component == \"ui\" && name != \"x\"", None).unwrap();
        let mut st = ScriptState::new(s);
        let (r, payload) = rec(Some(1.0));
        st.eval(&r, &payload);
        assert_eq!(st.drain()["stats"]["matched"], 1);
    }
}
