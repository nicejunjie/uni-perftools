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
/// Parse one JSON object into an event definition. `name` is its EventName (or,
/// for an ArchStdEvent reference, the referenced name); fields come from the
/// object, with any field absent here inherited from `base` (the ArchStdEvent
/// template). Mirrors perf jevents: an ArchStdEvent supplies defaults the entry
/// may override.
fn parse_event(m: &Value, base: Option<&Value>) -> Option<(String, EventDef)> {
    let name = m
        .get("EventName")
        .and_then(|v| v.as_str())
        .or_else(|| base.and_then(|b| b.get("EventName")).and_then(|v| v.as_str()))?;
    let mut fields = Vec::new();
    for (jkey, fkey) in [("EventCode", "event"), ("UMask", "umask"),
                         ("Cmask", "cmask"), ("Inv", "inv"), ("Edge", "edge")] {
        // entry wins over the ArchStdEvent template
        let v = m.get(jkey).and_then(hex_or_dec).or_else(|| base.and_then(|b| b.get(jkey)).and_then(hex_or_dec));
        if let Some(v) = v {
            if v != 0 || jkey == "EventCode" {
                fields.push((fkey.to_string(), v));
            }
        }
    }
    let unit = m
        .get("Unit")
        .or_else(|| base.and_then(|b| b.get("Unit")))
        .and_then(|v| v.as_str())
        .unwrap_or("cpu")
        .to_string();
    Some((name.to_lowercase(), EventDef { fields, unit }))
}

/// Architecture-shared ArchStdEvent templates (ARM keeps event encodings once in
/// `<arch>/common-and-microarch.json` and references them by name from each
/// model). Returns name → the full JSON object so models can inherit + override.
fn arch_std_events(dir: &std::path::Path) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    // ascend to the arch dir (…/arch/x86 or …/arch/arm64)
    let mut arch = dir;
    while let Some(parent) = arch.parent() {
        if matches!(arch.file_name().and_then(|s| s.to_str()), Some("x86" | "arm64")) {
            break;
        }
        arch = parent;
    }
    for f in ["common-and-microarch.json", "recommended.json"] {
        let Ok(txt) = std::fs::read_to_string(arch.join(f)) else { continue };
        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&txt) else { continue };
        for m in items {
            if let Some(name) = m.get("EventName").and_then(|v| v.as_str()) {
                out.insert(name.to_string(), m);
            }
        }
    }
    out
}

pub fn load(dir: &std::path::Path) -> PmuDb {
    let mut events = HashMap::new();
    let mut metrics = HashMap::new();
    let std_events = arch_std_events(dir);
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
            // Event: inline EventName, or an ArchStdEvent reference resolved
            // against the arch-shared templates (then locally overridable).
            let base = m.get("ArchStdEvent").and_then(|v| v.as_str()).and_then(|s| std_events.get(s));
            if m.get("EventName").is_some() || base.is_some() {
                if let Some((name, ev)) = parse_event(&m, base) {
                    events.insert(name, ev);
                }
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

/// Resolve an event name to its definition. JSON wins; otherwise fall back to a
/// kernel-provided event alias under `/sys/devices/<pmu>/events/<name>`. Intel's
/// `slots` and the `topdown-*` PERF_METRICS pseudo-events are not in the JSON —
/// the kernel exposes their encoding here, so this is still sourced, not guessed.
fn resolve_event(db: &PmuDb, name: &str) -> Option<EventDef> {
    if let Some(ev) = db.events.get(name) {
        return Some(ev.clone());
    }
    kernel_event(name)
}

/// Read a kernel event alias (`event=0x..,umask=0x..`) from any PMU that defines
/// one with this name, parsing it into the same field/value form as the JSON.
fn kernel_event(name: &str) -> Option<EventDef> {
    for dev in std::fs::read_dir("/sys/devices").into_iter().flatten().flatten() {
        let unit = dev.file_name().to_string_lossy().into_owned();
        let path = dev.path().join("events").join(name);
        let Ok(spec) = std::fs::read_to_string(&path) else { continue };
        let mut fields = Vec::new();
        let mut ok = true;
        for kv in spec.trim().split(',') {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let v = v.trim();
            let val = if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                u64::from_str_radix(h, 16).ok()
            } else {
                v.parse().ok()
            };
            match val {
                Some(val) => fields.push((k.trim().to_string(), val)),
                None => { ok = false; break } // e.g. "config1=..." with non-numeric → skip alias
            }
        }
        if ok && !fields.is_empty() {
            return Some(EventDef { fields, unit });
        }
    }
    None
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
    Sys(String), // #smt_on, #num_cpus, … (perf "system constants")
    Op(String),  // + - * / ( ) , and multi-char relops + ternary ? :
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
        } else if c == '#' {
            // perf system constant: #smt_on, #num_cpus, #core_wide, …
            i += 1;
            let start = i;
            while i < b.len() && {
                let ch = b[i] as char;
                ch.is_ascii_alphanumeric() || ch == '_'
            } {
                i += 1;
            }
            out.push(Tok::Sys(s[start..i].to_lowercase()));
        } else if c.is_ascii_alphabetic() || c == '_' || c == '\\' {
            // Identifier (event/metric/function). perf escapes characters that
            // are otherwise operators inside event names: `topdown\-retiring`,
            // `EVENT\,umask`, `cmask\=1`. Consume `\X` as the literal X so the
            // name matches the JSON/kernel spelling (minus the backslash).
            let mut name = String::new();
            while i < b.len() {
                let ch = b[i] as char;
                if ch == '\\' && i + 1 < b.len() {
                    name.push(b[i + 1] as char);
                    i += 2;
                } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
                    name.push(ch);
                    i += 1;
                } else {
                    break;
                }
            }
            if name.is_empty() {
                return None;
            }
            out.push(Tok::Ident(name.to_lowercase()));
        } else if c == '<' || c == '>' || c == '=' || c == '!' {
            // relational / equality; may be one or two chars (<, <=, ==, !=)
            if i + 1 < b.len() && b[i + 1] == b'=' {
                out.push(Tok::Op(format!("{c}=")));
                i += 2;
            } else if c == '<' || c == '>' {
                out.push(Tok::Op(c.to_string()));
                i += 1;
            } else {
                return None; // lone '=' or '!' → gap
            }
        } else if "+-*/(),?:".contains(c) {
            out.push(Tok::Op(c.to_string()));
            i += 1;
        } else {
            return None; // unsupported character (e.g. @ event modifier) → gap
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
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if o == op)
    }
    fn is_ident(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }
    /// Top of the grammar (lowest precedence): both ternary spellings perf uses —
    /// C-style `cond ? a : b` (newer Intel/AMD) and infix `a if cond else b`
    /// (older Intel TMA helpers).
    fn expr(&mut self) -> Option<f64> {
        let v = self.cmp()?;
        if self.is_op("?") {
            self.pos += 1;
            let then = self.expr()?;
            self.is_op(":").then(|| ())?;
            self.pos += 1;
            let els = self.expr()?;
            Some(if v != 0.0 { then } else { els })
        } else if self.is_ident("if") {
            // `value if cond else other` — value already parsed into `v`.
            self.pos += 1;
            let cond = self.expr()?;
            self.is_ident("else").then(|| ())?;
            self.pos += 1;
            let other = self.expr()?;
            Some(if cond != 0.0 { v } else { other })
        } else {
            Some(v)
        }
    }
    /// Relational/equality, yielding 1.0/0.0 (perf uses these inside `?:`/min/max).
    fn cmp(&mut self) -> Option<f64> {
        let l = self.add()?;
        if let Some(Tok::Op(o)) = self.peek() {
            if matches!(o.as_str(), "<" | ">" | "<=" | ">=" | "==" | "!=") {
                let o = o.clone();
                self.pos += 1;
                let r = self.add()?;
                let b = match o.as_str() {
                    "<" => l < r,
                    ">" => l > r,
                    "<=" => l <= r,
                    ">=" => l >= r,
                    "==" => l == r,
                    _ => l != r,
                };
                return Some(if b { 1.0 } else { 0.0 });
            }
        }
        Some(l)
    }
    fn add(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(Tok::Op(o)) = self.peek() {
            let o = o.clone();
            if o == "+" || o == "-" {
                self.pos += 1;
                let r = self.term()?;
                v = if o == "+" { v + r } else { v - r };
            } else {
                break;
            }
        }
        Some(v)
    }
    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        while let Some(Tok::Op(o)) = self.peek() {
            let o = o.clone();
            if o == "*" || o == "/" {
                self.pos += 1;
                let r = self.factor()?;
                v = if o == "*" { v * r } else if r != 0.0 { v / r } else { 0.0 };
            } else {
                break;
            }
        }
        Some(v)
    }
    fn factor(&mut self) -> Option<f64> {
        match self.peek()?.clone() {
            Tok::Num(n) => {
                self.pos += 1;
                Some(n)
            }
            Tok::Sys(name) => {
                self.pos += 1;
                sys_const(&name)
            }
            Tok::Op(ref o) if o == "(" => {
                self.pos += 1;
                let v = self.expr()?;
                self.is_op(")").then(|| ())?;
                self.pos += 1;
                Some(v)
            }
            Tok::Op(ref o) if o == "-" => {
                self.pos += 1;
                Some(-self.factor()?)
            }
            Tok::Ident(name) => {
                self.pos += 1;
                // function call?
                if self.is_op("(") {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.is_op(")") {
                        loop {
                            args.push(self.expr()?);
                            if self.is_op(",") {
                                self.pos += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    self.is_op(")").then(|| ())?;
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
        // perf's `if(c, t, e)` (a few metric files use the functional form)
        ("if", [c, t, e]) => Some(if *c != 0.0 { *t } else { *e }),
        _ => None, // unsupported function → gap
    }
}

/// perf "system constants" (`#name`). Resolve the few that are knowable from the
/// host without guessing; anything else is a gap (`None`).
fn sys_const(name: &str) -> Option<f64> {
    match name {
        "smt_on" => Some(if smt_on() { 1.0 } else { 0.0 }),
        "num_cpus" | "num_cpus_online" => Some(num_cpus_online() as f64),
        // We aggregate over the whole process, never perf's `--per-core` mode, so
        // core_wide is 0 — this selects the per-thread-slots branch of the older
        // Intel TMA helpers, which is the correct one for our counting mode.
        "core_wide" => Some(0.0),
        // Anything else (e.g. #num_packages-scaled) can't be reproduced
        // faithfully from a whole-process count → gap, not a wrong number.
        _ => None,
    }
}

/// SMT enabled? True when any core lists more than one thread sibling.
fn smt_on() -> bool {
    if let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/topology/thread_siblings_list") {
        return s.contains(',') || s.contains('-');
    }
    false
}

fn num_cpus_online() -> usize {
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .ok()
        .map(|s| {
            s.trim()
                .split(',')
                .map(|r| match r.split_once('-') {
                    Some((a, b)) => {
                        let (a, b) = (a.trim().parse::<usize>().unwrap_or(0), b.trim().parse::<usize>().unwrap_or(0));
                        b.saturating_sub(a) + 1
                    }
                    None => 1,
                })
                .sum()
        })
        .filter(|&n| n > 0)
        .unwrap_or(1)
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

/// Intel PERF_METRICS pseudo-events + the slots fixed counter. These are not in
/// the pmu-events JSON — the kernel exposes their encoding under
/// `/sys/devices/cpu*/events/`. Stable kernel ABI names, so structural
/// validation can count them "present" without the Intel hardware to read sysfs.
const PERF_METRICS_ALIASES: &[&str] = &[
    "slots",
    "topdown-retiring",
    "topdown-bad-spec",
    "topdown-fe-bound",
    "topdown-be-bound",
    // L2 (newer Intel)
    "topdown-heavy-ops",
    "topdown-br-mispredict",
    "topdown-fetch-lat",
    "topdown-mem-bound",
];

/// Whether a referenced event can be counted on the target: present in the
/// vendored JSON, or a known kernel-provided alias (resolved from sysfs at run
/// time on the real CPU). Used by structural validation, which has the JSON but
/// not the hardware's sysfs.
fn event_present(db: &PmuDb, name: &str) -> bool {
    db.events.contains_key(name) || PERF_METRICS_ALIASES.contains(&name)
}

/// Structural validation of one model's db against the canonical metrics, with no
/// hardware: for each CANON metric, does the model define a candidate, does its
/// formula parse + use only supported constructs, and is every referenced event
/// resolvable (JSON or kernel alias)? This is how the SPR/Grace/older-Intel/Zen
/// implementations are checked by construction on a single host.
#[derive(Debug, Clone)]
pub struct CanonStatus {
    pub key: &'static str,
    pub metric: Option<String>, // the candidate present in this model, if any
    pub resolved: bool,
    pub note: String,
}

pub fn validate_canon(db: &PmuDb) -> Vec<CanonStatus> {
    CANON
        .iter()
        .map(|&(key, _label, cands)| {
            let Some(mname) = cands.iter().copied().find(|c| db.metrics.contains_key(*c)) else {
                return CanonStatus { key, metric: None, resolved: false, note: "no candidate metric in model".into() };
            };
            // Collect referenced events while proving the formula uses only
            // constructs we implement (stub returns Some so only *constructs*,
            // not missing data, can fail the eval).
            let mut refs: Vec<String> = Vec::new();
            let ok = eval_metric(db, mname, &mut |e| {
                if !refs.contains(&e.to_string()) {
                    refs.push(e.to_string());
                }
                Some(1.0)
            })
            .is_some();
            if !ok {
                return CanonStatus { key, metric: Some(mname.into()), resolved: false, note: "formula uses an unsupported construct".into() };
            }
            if let Some(missing) = refs.iter().find(|e| !event_present(db, e)) {
                return CanonStatus { key, metric: Some(mname.into()), resolved: false, note: format!("missing event: {missing}") };
            }
            CanonStatus { key, metric: Some(mname.into()), resolved: true, note: format!("{} events", refs.len()) }
        })
        .collect()
}

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
                resolve_event(db, e).is_some_and(|ev| {
                    let f = fmts.entry(ev.unit.clone()).or_insert_with(|| sysfs_format(&ev.unit));
                    encode(&ev, f).is_some()
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
                            let ev = resolve_event(db, e)?;
                            let f = fmts.entry(ev.unit.clone()).or_insert_with(|| sysfs_format(&ev.unit));
                            Some(EventCfg { etype: pmu_type(&ev.unit), config: encode(&ev, f)? })
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

    /// Build a db whose only content is the named test formulas (no hardware).
    fn db_with(metrics: &[(&str, &str)]) -> PmuDb {
        PmuDb {
            model_dir: "test".into(),
            events: HashMap::new(),
            metrics: metrics.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn evaluator_ternary_and_comparison() {
        // ternary over a comparison, the two flavours perf uses.
        let db = db_with(&[("m", "(a > b) ? 10 : 20")]);
        let mut hi = |e: &str| match e { "a" => Some(3.0), "b" => Some(2.0), _ => None };
        assert_eq!(eval_metric(&db, "m", &mut hi), Some(10.0));
        let mut lo = |e: &str| match e { "a" => Some(1.0), "b" => Some(2.0), _ => None };
        assert_eq!(eval_metric(&db, "m", &mut lo), Some(20.0));
    }

    #[test]
    fn evaluator_if_function() {
        let db = db_with(&[("m", "if(c, 5, 7)")]);
        let mut z = |e: &str| if e == "c" { Some(0.0) } else { None };
        assert_eq!(eval_metric(&db, "m", &mut z), Some(7.0));
        let mut nz = |e: &str| if e == "c" { Some(1.0) } else { None };
        assert_eq!(eval_metric(&db, "m", &mut nz), Some(5.0));
    }

    #[test]
    fn evaluator_escaped_hyphen_event() {
        // Intel TMA writes `topdown\-retiring` — the backslash escapes the '-' so
        // it's part of the event name, not subtraction. The token must resolve to
        // the kernel-alias spelling "topdown-retiring".
        let db = db_with(&[("m", "topdown\\-retiring / 4")]);
        let mut c = |e: &str| if e == "topdown-retiring" { Some(8.0) } else { None };
        assert_eq!(eval_metric(&db, "m", &mut c), Some(2.0));
    }

    #[test]
    fn evaluator_system_constant_smt() {
        // #smt_on resolves to a real host fact (0 or 1) — not a gap.
        let db = db_with(&[("m", "#smt_on")]);
        let mut none = |_: &str| None;
        let v = eval_metric(&db, "m", &mut none).expect("smt_on resolves on Linux");
        assert!(v == 0.0 || v == 1.0, "got {v}");
        // An unknown #constant is an honest gap, not a fabricated number.
        let db2 = db_with(&[("m", "#bogus_constant")]);
        assert_eq!(eval_metric(&db2, "m", &mut none), None);
    }

    /// Enumerate every vendored model dir — any directory under arch/{x86,arm64}
    /// that directly holds *.json (some, like NVIDIA Grace `nvidia/t410`, nest).
    /// The name is the path relative to the arch dir (e.g. "amdzen5", "nvidia/t410").
    fn vendored_model_dirs() -> Vec<(String, std::path::PathBuf)> {
        fn has_json(d: &std::path::Path) -> bool {
            std::fs::read_dir(d)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        }
        fn walk(base: &std::path::Path, d: &std::path::Path, out: &mut Vec<(String, std::path::PathBuf)>) {
            if has_json(d) {
                let name = d.strip_prefix(base).unwrap_or(d).to_string_lossy().into_owned();
                out.push((name, d.to_path_buf()));
            }
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    walk(base, &e.path(), out);
                }
            }
        }
        let Some(root) = data_root() else { return Vec::new() };
        let mut out = Vec::new();
        for arch in ["x86", "arm64"] {
            let adir = root.join(arch);
            walk(&adir, &adir, &mut out);
        }
        out.sort();
        out
    }

    /// CI structural gate: across every vendored CPU model, top-down must EITHER
    /// fully resolve (candidate metric present, formula supported, all events
    /// resolvable) OR be an explicit gap — never a partial/guessed result. Plus
    /// the families we claim support for must actually resolve all 4 quadrants.
    #[test]
    fn structural_validation_across_vendored_models() {
        let dirs = vendored_model_dirs();
        assert!(!dirs.is_empty(), "vendored pmu-events tree not found");

        // Families we claim top-down support for → must resolve all 4 metrics.
        // (AMD: only Zen5+ ship the named metrics in perf's JSON; Zen1-4 and ARM
        // Grace have no such metric → documented gaps, asserted as gaps below.)
        let required: &[&str] = &[
            "amdzen4", "amdzen5", "amdzen6", // AMD Zen4+ ship the named metrics
            "haswellx", "broadwellx", "skylakex", "cascadelakex", "icelakex",
            "sapphirerapids", "emeraldrapids", "graniterapids", // Intel Xeon
            "nvidia/t410", // NVIDIA Grace (ARM Neoverse V2)
        ];
        // No top-down metric in perf's JSON for these → documented gaps, not guesses.
        let known_gaps: &[&str] = &["amdzen1", "amdzen2", "amdzen3", "arm"];

        eprintln!("\n{:<16} {:>9}  detail", "model", "topdown");
        let mut required_seen = std::collections::HashSet::new();
        for (name, dir) in &dirs {
            if name.is_empty() {
                continue; // the arch-root shared file, not a real model
            }
            let mut db = load(dir);
            db.model_dir = name.clone();
            let st = validate_canon(&db);
            let n_ok = st.iter().filter(|s| s.resolved).count();
            let detail: Vec<String> = st
                .iter()
                .map(|s| format!("{}={}", s.key.trim_start_matches("topdown_").trim_end_matches("_pct"), if s.resolved { "ok" } else { "gap" }))
                .collect();
            eprintln!("{:<16} {:>4}/{}    {}", name, n_ok, st.len(), detail.join(" "));

            // Invariant for EVERY model: each metric is all-or-nothing (a resolved
            // metric had its candidate + supported formula + all events present).
            for s in &st {
                if s.resolved {
                    assert!(s.metric.is_some(), "{name}/{}: resolved but no metric", s.key);
                }
            }

            if required.contains(&name.as_str()) {
                required_seen.insert(name.clone());
                let gaps: Vec<_> = st.iter().filter(|s| !s.resolved).collect();
                assert!(
                    gaps.is_empty(),
                    "{name}: claimed-supported family failed top-down: {:?}",
                    gaps.iter().map(|s| format!("{} ({})", s.key, s.note)).collect::<Vec<_>>()
                );
            }
            if known_gaps.contains(&name.as_str()) {
                assert!(
                    st.iter().all(|s| !s.resolved),
                    "{name}: documented as a top-down gap but something resolved — update the harness"
                );
            }
        }
        // Every claimed family must actually be present in the vendored tree.
        for r in required {
            assert!(required_seen.contains(*r), "claimed-supported model `{r}` missing from vendored tree");
        }
    }

    #[test]
    fn unsupported_at_modifier_is_a_gap() {
        // Event `@…@` modifiers aren't implemented → the metric must gap, never
        // silently drop the modifier and return a wrong count.
        let db = db_with(&[("m", "cpu@event\\=0x3c@ / 2")]);
        let mut c = |_: &str| Some(100.0);
        assert_eq!(eval_metric(&db, "m", &mut c), None);
    }
}
