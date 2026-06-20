//! Vendor-neutral HWPC engine driven entirely by perf's `pmu-events` data.
//!
//! Nothing here is hand-coded or guessed:
//!  * event codes + metric formulas come from the vendored perf JSON
//!    (`collectors/snapshot/pmu-events/arch/...`),
//!  * the config **bit layout** comes from the kernel's
//!    `/sys/devices/<pmu>/format/*` files (authoritative, exactly what perf uses),
//!  * the CPU → model mapping comes from perf's `mapfile.csv`.
//!
//! If a referenced event is absent, a formula uses a construct we don't
//! implement, or a sysfs field is missing, the metric is reported as a **gap**
//! (`None`) — a counter is never fabricated.

use std::collections::HashMap;
use std::path::PathBuf;

use regex::Regex;
use serde_json::Value;

/// A perf event definition (the fields we encode), from the JSON.
#[derive(Clone, Debug, Default)]
pub struct EventDef {
    pub fields: Vec<(String, u64)>, // (sysfs-format field name, value): event, umask, cmask, ...
    pub unit: String,               // PMU, e.g. "cpu" / "cpu_core" / "armv8_pmuv3_0"
}

/// The vendored data for the detected CPU model.
pub struct PmuDb {
    pub model_dir: String,
    pub events: HashMap<String, EventDef>, // keyed by lowercased EventName (incl. ".submask")
    pub metrics: HashMap<String, String>,  // lowercased MetricName -> MetricExpr
}

// ---------------------------------------------------------------- data location
fn data_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UAPS_PMU_EVENTS") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    // dev: relative to this crate; install: relative to the exe (../lib/...).
    let mut cands = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../pmu-events/arch")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join("../pmu-events/arch"));
            cands.push(d.join("../../pmu-events/arch"));
        }
    }
    cands.into_iter().find(|p| p.is_dir())
}

// ---------------------------------------------------------------- CPU detection
struct CpuId {
    arch: &'static str,    // "x86" or "arm64"
    keys: Vec<String>,     // candidate mapfile match strings (with/without stepping)
}

fn detect_cpuid() -> Option<CpuId> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let field = |name: &str| {
        info.lines()
            .find(|l| l.split(':').next().map(|s| s.trim()) == Some(name))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
    };
    if let Some(vendor) = field("vendor_id") {
        // x86: perf's cpuid string is "vendor-FAMILY(dec)-MODEL(hex)[-STEPPING(hex)]".
        let fam: u32 = field("cpu family")?.parse().ok()?;
        let model: u32 = field("model")?.parse().ok()?;
        let step = field("stepping").and_then(|s| s.parse::<u32>().ok());
        // mapfile rows may or may not include stepping — try both forms.
        let mut keys = vec![format!("{vendor}-{fam}-{model:X}")];
        if let Some(s) = step {
            keys.insert(0, format!("{vendor}-{fam}-{model:X}-{s:X}"));
        }
        return Some(CpuId { arch: "x86", keys });
    }
    if let Some(imp) = field("CPU implementer") {
        // arm64: perf matches the 64-bit MIDR. Build it from cpuinfo fields per the
        // ARM MIDR layout: [31:24]=implementer [23:20]=variant [19:16]=arch
        // [15:4]=partnum [3:0]=revision.
        let h = |s: Option<String>| s.and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok());
        let imp = u64::from_str_radix(imp.trim().trim_start_matches("0x"), 16).ok()?;
        let var = h(field("CPU variant")).unwrap_or(0);
        let part = h(field("CPU part")).unwrap_or(0);
        let rev = field("CPU revision").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let arch = 0xf; // ARMv8
        let midr = (imp << 24) | (var << 20) | (arch << 16) | (part << 4) | rev;
        return Some(CpuId { arch: "arm64", keys: vec![format!("0x{midr:016x}")] });
    }
    None
}

/// Resolve the model directory for this CPU via perf's mapfile.csv. None = a gap
/// (unknown CPU); we never guess a model.
pub fn detect_model_dir() -> Option<(PathBuf, String)> {
    let root = data_root()?;
    let cpu = detect_cpuid()?;
    let mapfile = root.join(cpu.arch).join("mapfile.csv");
    let body = std::fs::read_to_string(&mapfile).ok()?;
    for line in body.lines() {
        let mut it = line.splitn(4, ',');
        let (Some(re), _ver, Some(path), _ty) = (it.next(), it.next(), it.next(), it.next()) else {
            continue;
        };
        // mapfile column 0 is a POSIX ERE matched against the cpuid string.
        if let Ok(rx) = Regex::new(&format!("^(?:{})$", re.replace("[[:xdigit:]]", "[0-9A-Fa-f]"))) {
            if cpu.keys.iter().any(|k| rx.is_match(k)) {
                let dir = root.join(cpu.arch).join(path);
                if dir.is_dir() {
                    return Some((dir, path.to_string()));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- JSON loading
fn hex_or_dec(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u64::from_str_radix(h, 16).ok()
            } else {
                s.parse().ok()
            }
        }
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Load all event + metric JSON in a model directory.
pub fn load(dir: &std::path::Path) -> PmuDb {
    let mut events = HashMap::new();
    let mut metrics = HashMap::new();
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    for e in entries {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = std::fs::read_to_string(&p) else { continue };
        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&txt) else { continue };
        for m in items {
            if let Some(name) = m.get("MetricName").and_then(|v| v.as_str()) {
                if let Some(expr) = m.get("MetricExpr").and_then(|v| v.as_str()) {
                    metrics.insert(name.to_lowercase(), expr.to_string());
                }
            }
            if let Some(name) = m.get("EventName").and_then(|v| v.as_str()) {
                let mut fields = Vec::new();
                // map JSON keys → sysfs-format field names (the kernel owns the bits)
                for (jkey, fkey) in [("EventCode", "event"), ("UMask", "umask"),
                                     ("Cmask", "cmask"), ("Inv", "inv"), ("Edge", "edge")] {
                    if let Some(v) = m.get(jkey).and_then(hex_or_dec) {
                        if v != 0 || jkey == "EventCode" {
                            fields.push((fkey.to_string(), v));
                        }
                    }
                }
                let unit = m.get("Unit").and_then(|v| v.as_str()).unwrap_or("cpu").to_string();
                events.insert(name.to_lowercase(), EventDef { fields, unit });
            }
        }
    }
    PmuDb { model_dir: String::new(), events, metrics }
}

// ---------------------------------------------------------------- sysfs encode
/// Parse one `/sys/devices/<pmu>/format/<field>` spec, e.g. "config:0-7,32-35".
fn parse_format(spec: &str) -> Option<(String, Vec<(u8, u8)>)> {
    let (reg, ranges) = spec.trim().split_once(':')?;
    let mut out = Vec::new();
    for part in ranges.split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
            None => {
                let b: u8 = part.parse().ok()?;
                (b, b)
            }
        };
        out.push((lo, hi));
    }
    Some((reg.to_string(), out))
}

fn sysfs_format(pmu: &str) -> HashMap<String, Vec<(u8, u8)>> {
    let mut m = HashMap::new();
    let dir = format!("/sys/devices/{pmu}/format");
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let field = e.file_name().to_string_lossy().into_owned();
        if let Ok(spec) = std::fs::read_to_string(e.path()) {
            // we only place into "config"; ignore config1/config2 fields for now
            if let Some((reg, ranges)) = parse_format(spec.trim()) {
                if reg == "config" {
                    m.insert(field, ranges);
                }
            }
        }
    }
    m
}

/// Encode an event into perf_event_attr.config using the PMU's sysfs bit layout.
/// None = a gap (a field has no sysfs layout — we don't guess bit positions).
pub fn encode(ev: &EventDef, fmt: &HashMap<String, Vec<(u8, u8)>>) -> Option<u64> {
    let mut config = 0u64;
    for (field, mut val) in ev.fields.iter().cloned() {
        let ranges = fmt.get(&field)?; // missing field layout → gap, never guessed
        for &(lo, hi) in ranges {
            let w = (hi - lo + 1) as u32;
            let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            config |= (val & mask) << lo;
            val >>= w;
        }
    }
    Some(config)
}

// ---------------------------------------------------------------- evaluator
/// Evaluate a metric formula. `count(event_name)` returns the measured raw count
/// (or None if that event wasn't/couldn't be collected). Returns None on any
/// unresolved reference or unsupported construct — a reported gap, not a guess.
pub fn eval_metric(
    db: &PmuDb,
    name: &str,
    count: &mut dyn FnMut(&str) -> Option<f64>,
) -> Option<f64> {
    let expr = db.metrics.get(&name.to_lowercase())?;
    let mut depth = 0;
    eval_expr(expr, db, count, &mut depth)
}

fn eval_expr(
    expr: &str,
    db: &PmuDb,
    count: &mut dyn FnMut(&str) -> Option<f64>,
    depth: &mut u32,
) -> Option<f64> {
    *depth += 1;
    if *depth > 64 {
        return None; // metric-reference cycle / too deep → gap
    }
    let toks = tokenize(expr)?;
    let mut p = Parser { toks: &toks, pos: 0, db, count, depth };
    let v = p.expr()?;
    if p.pos != p.toks.len() {
        return None; // trailing junk we didn't understand → gap
    }
    Some(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char), // + - * / ( ) ,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || (c == '.' && i + 1 < b.len() && (b[i + 1] as char).is_ascii_digit()) {
            let start = i;
            if c == '0' && i + 1 < b.len() && (b[i + 1] | 0x20) == b'x' {
                i += 2;
                while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                out.push(Tok::Num(u64::from_str_radix(&s[start + 2..i], 16).ok()? as f64));
            } else {
                while i < b.len() && ((b[i] as char).is_ascii_digit() || b[i] == b'.' || (b[i] | 0x20) == b'e') {
                    i += 1;
                }
                out.push(Tok::Num(s[start..i].parse().ok()?));
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && {
                let ch = b[i] as char;
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'
            } {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].to_lowercase()));
        } else if "+-*/(),".contains(c) {
            out.push(Tok::Op(c));
            i += 1;
        } else {
            return None; // unsupported character (e.g. @, #, <, ?) → gap
        }
    }
    Some(out)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    db: &'a PmuDb,
    count: &'a mut dyn FnMut(&str) -> Option<f64>,
    depth: &'a mut u32,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(Tok::Op(c @ ('+' | '-'))) = self.peek() {
            let c = *c;
            self.pos += 1;
            let r = self.term()?;
            v = if c == '+' { v + r } else { v - r };
        }
        Some(v)
    }
    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        while let Some(Tok::Op(c @ ('*' | '/'))) = self.peek() {
            let c = *c;
            self.pos += 1;
            let r = self.factor()?;
            v = if c == '*' { v * r } else if r != 0.0 { v / r } else { 0.0 };
        }
        Some(v)
    }
    fn factor(&mut self) -> Option<f64> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.pos += 1;
                Some(n)
            }
            Tok::Op('(') => {
                self.pos += 1;
                let v = self.expr()?;
                matches!(self.peek(), Some(Tok::Op(')'))).then(|| ())?;
                self.pos += 1;
                Some(v)
            }
            Tok::Op('-') => {
                self.pos += 1;
                Some(-self.factor()?)
            }
            Tok::Ident(name) => {
                self.pos += 1;
                // function call?
                if matches!(self.peek(), Some(Tok::Op('('))) {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::Op(')'))) {
                        loop {
                            args.push(self.expr()?);
                            match self.peek() {
                                Some(Tok::Op(',')) => self.pos += 1,
                                _ => break,
                            }
                        }
                    }
                    matches!(self.peek(), Some(Tok::Op(')'))).then(|| ())?;
                    self.pos += 1;
                    return apply_fn(&name, &args);
                }
                // metric reference?
                if self.db.metrics.contains_key(&name) {
                    return eval_expr(&self.db.metrics[&name].clone(), self.db, self.count, self.depth);
                }
                // event reference (resolved against measured counts)
                (self.count)(&name)
            }
            _ => None,
        }
    }
}

fn apply_fn(name: &str, args: &[f64]) -> Option<f64> {
    match (name, args) {
        ("d_ratio", [a, b]) => Some(if *b != 0.0 { a / b } else { 0.0 }),
        ("min", [a, b]) => Some(a.min(*b)),
        ("max", [a, b]) => Some(a.max(*b)),
        _ => None, // unsupported function → gap
    }
}

// ---------------------------------------------------------------- public detect
/// Detect + load the pmu-events db for this host, or None if the CPU is unknown.
pub fn detect() -> Option<PmuDb> {
    let (dir, model) = detect_model_dir()?;
    let mut db = load(&dir);
    db.model_dir = model;
    Some(db)
}

pub fn cpu_format(pmu: &str) -> HashMap<String, Vec<(u8, u8)>> {
    sysfs_format(pmu)
}

// ---------------------------------------------------------------- collector
use anyhow::Result;
use crate::pmu::{EventCfg, ThreadGroups, TYPE_RAW};
use uaps_core::{Collector, Metric, MetricValue, Target};

/// Canonical snapshot metrics → candidate perf metric names (first present wins).
/// AMD names come from amdzen pipeline.json; Intel `tma_*` from the TMA tree.
const CANON: &[(&str, &str, &[&str])] = &[
    ("topdown_retiring_pct", "Retiring", &["retiring", "tma_retiring"]),
    ("topdown_frontend_pct", "Frontend bound", &["frontend_bound", "tma_frontend_bound"]),
    ("topdown_backend_pct", "Backend bound", &["backend_bound", "tma_backend_bound"]),
    ("topdown_badspec_pct", "Bad speculation", &["bad_speculation", "tma_bad_speculation"]),
];

fn pmu_type(unit: &str) -> u32 {
    let u = if unit.is_empty() { "cpu" } else { unit };
    std::fs::read_to_string(format!("/sys/devices/{u}/type"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(TYPE_RAW)
}

/// HWPC collector driven by the pmu-events db: resolves the canonical metrics for
/// this CPU, counts exactly the events their formulas reference, evaluates them.
/// Unsupported metric / missing event / unknown CPU → that metric is simply absent.
struct Chosen {
    key: &'static str,
    label: &'static str,
    metric: String,
    group: usize, // index into `groups_ev`: every event this metric needs is here
}

pub struct HwpcCollector {
    db: Option<PmuDb>,
    chosen: Vec<Chosen>,
    groups_ev: Vec<Vec<String>>, // event names per perf group (read_sums order)
    groups: ThreadGroups,
}

impl HwpcCollector {
    pub fn new() -> Self {
        let db = detect();
        let mut chosen = Vec::new();
        let mut groups_ev: Vec<Vec<String>> = Vec::new();
        if let Some(db) = &db {
            let mut fmts: HashMap<String, HashMap<String, Vec<(u8, u8)>>> = HashMap::new();
            let mut encodes = |e: &str| -> bool {
                db.events.get(e).is_some_and(|ev| {
                    let f = fmts.entry(ev.unit.clone()).or_insert_with(|| sysfs_format(&ev.unit));
                    encode(ev, f).is_some()
                })
            };
            for &(key, label, cands) in CANON {
                let Some(mname) = cands.iter().copied().find(|c| db.metrics.contains_key(*c)) else {
                    continue;
                };
                // discover the events the formula references (also proves it's supported)
                let mut refs: Vec<String> = Vec::new();
                let ok = eval_metric(db, mname, &mut |e| {
                    if !refs.contains(&e.to_string()) {
                        refs.push(e.to_string());
                    }
                    Some(1.0)
                })
                .is_some();
                if !ok || !refs.iter().all(|e| encodes(e)) {
                    continue; // unsupported construct or unencodable event → gap
                }
                // metric-aware packing: place ALL of this metric's events in ONE
                // perf group (≤5) so its numerator/denominator are co-scheduled.
                if refs.len() > 5 {
                    continue; // can't co-schedule → would be inaccurate; gap, not guess
                }
                let gi = groups_ev.iter().position(|g| {
                    let extra = refs.iter().filter(|e| !g.contains(e)).count();
                    g.len() + extra <= 5
                });
                let gi = match gi {
                    Some(i) => i,
                    None => {
                        groups_ev.push(Vec::new());
                        groups_ev.len() - 1
                    }
                };
                for e in &refs {
                    if !groups_ev[gi].contains(e) {
                        groups_ev[gi].push(e.clone());
                    }
                }
                chosen.push(Chosen { key, label, metric: mname.to_string(), group: gi });
            }
        }
        // encode each group's events
        let groups_spec: Vec<Vec<EventCfg>> = if let Some(db) = &db {
            let mut fmts: HashMap<String, HashMap<String, Vec<(u8, u8)>>> = HashMap::new();
            groups_ev
                .iter()
                .map(|g| {
                    g.iter()
                        .filter_map(|e| {
                            let ev = db.events.get(e)?;
                            let f = fmts.entry(ev.unit.clone()).or_insert_with(|| sysfs_format(&ev.unit));
                            Some(EventCfg { etype: pmu_type(&ev.unit), config: encode(ev, f)? })
                        })
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };
        HwpcCollector { db, chosen, groups_ev, groups: ThreadGroups::new(groups_spec) }
    }

    /// True when the db resolved ≥1 metric (caller drops the hand-coded
    /// TopdownCollector in favour of this perf-data-driven one).
    pub fn active(&self) -> bool {
        !self.chosen.is_empty()
    }
}

impl Collector for HwpcCollector {
    fn name(&self) -> &'static str {
        "hwpc(pmu-events)"
    }
    fn start(&mut self, target: &Target) -> Result<()> {
        self.groups.start(target.pid);
        Ok(())
    }
    fn sample(&mut self) -> Result<()> {
        self.groups.scan();
        Ok(())
    }
    fn finish(&mut self) -> Result<Vec<Metric>> {
        let Some(db) = &self.db else { return Ok(Vec::new()) };
        // read_sums is flat in group order; rebuild per-group event→count maps so
        // each metric reads its co-scheduled counts.
        let sums = self.groups.read_sums();
        let mut per_group: Vec<HashMap<String, f64>> = Vec::new();
        let mut off = 0;
        for g in &self.groups_ev {
            let mut m = HashMap::new();
            for name in g {
                if let Some(Some(v)) = sums.get(off) {
                    m.insert(name.clone(), *v);
                }
                off += 1;
            }
            per_group.push(m);
        }
        let mut out = Vec::new();
        for c in &self.chosen {
            let counts = &per_group[c.group];
            let mut read = |e: &str| counts.get(e).copied();
            if let Some(v) = eval_metric(db, &c.metric, &mut read) {
                out.push(Metric {
                    key: c.key,
                    label: c.label.into(),
                    value: MetricValue::Percent(v * 100.0), // top-down formulas are fractions
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_this_host_model() {
        // On any CPU perf knows, detection resolves to a model dir.
        if let Some((_, model)) = detect_model_dir() {
            assert!(!model.is_empty());
            eprintln!("detected model dir: {model}");
        } else {
            eprintln!("CPU not in vendored mapfile (gap) — skipping");
        }
    }

    #[test]
    fn encoder_matches_known_amd_fp_event() {
        // fp_ret_sse_avx_ops.all = EventCode 0x03, UMask 0x0f → config 0x0f03,
        // exactly the value raw_pmu.rs hard-codes. Cross-checks the encoder.
        let Some((dir, model)) = detect_model_dir() else { return };
        if !model.starts_with("amdzen") {
            return; // this cross-check is AMD-specific
        }
        let db = load(&dir);
        let fmt = sysfs_format("cpu");
        if fmt.is_empty() {
            return; // no sysfs (containerized) — skip
        }
        let ev = db.events.get("fp_ret_sse_avx_ops.all").expect("event present in JSON");
        let cfg = encode(ev, &fmt).expect("encodes");
        assert_eq!(cfg, 0x0f03, "encoder must reproduce the known config");
    }

    #[test]
    fn evaluator_resolves_amd_topdown_retiring() {
        // retiring = d_ratio(ex_ret_ops, 8 * ls_not_halted_cyc). Feed synthetic
        // counts and check the math + event-ref + metric-ref resolution.
        let Some((dir, model)) = detect_model_dir() else { return };
        if !model.starts_with("amdzen") {
            return;
        }
        let mut db = load(&dir);
        db.model_dir = model;
        let mut counts = |ev: &str| -> Option<f64> {
            match ev {
                "ex_ret_ops" => Some(2000.0),
                "ls_not_halted_cyc" => Some(1000.0),
                _ => None,
            }
        };
        let r = eval_metric(&db, "retiring", &mut counts).expect("retiring resolves");
        // 2000 / (8 * 1000) = 0.25
        assert!((r - 0.25).abs() < 1e-9, "got {r}");
    }
}
