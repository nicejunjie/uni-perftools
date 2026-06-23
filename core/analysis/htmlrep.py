"""Self-contained HTML report generator for the suite.

`upat report --format html`         → the combined report
`upat report --detail mpi --format html` → the MPI analysis (comm-matrix
                                                 heatmap + size histogram)

No external assets/JS: inline CSS, tables, an SVG roofline, and a CSS-coloured
heatmap. The communication matrix — unreadable as text past a few ranks — is
rendered as a heatmap figure and DOWN-SAMPLED into buckets above ~256 ranks, so
it stays legible for thousand-rank jobs (like Intel APS's HTML matrix).
"""
import os
import sys
import json
import math
import html as _html

_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "roofline"))
import contract   # noqa: E402
import roofline   # noqa: E402
import viewpoints  # noqa: E402  (shared environment_rows)

E = _html.escape
CSS = """
:root{--good:#2e7d32;--warn:#d98300;--bad:#cf2e2e;--ink:#1f2430;--mut:#6b7280;
 --line:#e6e8ec;--bg:#eef1f5;--card:#fff;--accent:#1565c0}
*{box-sizing:border-box}
body{font:14px/1.55 -apple-system,Segoe UI,Roboto,Helvetica,sans-serif;margin:0;color:var(--ink);background:var(--bg)}
.wrap{max-width:1080px;margin:0 auto;padding:0 22px 56px}
/* header band */
.hdr{background:linear-gradient(120deg,#0d3b66 0%,#1565c0 100%);color:#fff;margin:0 -22px 0;padding:24px 28px 22px}
.hdr h1{font-size:20px;margin:0;font-weight:600;letter-spacing:.2px}
.hdr .cmd{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;opacity:.9;margin-top:7px;word-break:break-all}
.hdr .meta{font-size:12.5px;opacity:.85;margin-top:8px}
.hdr .meta b{font-weight:600;opacity:1}
/* headline insight */
.headline{background:var(--card);border-left:5px solid var(--accent);padding:13px 16px;margin:20px 0 8px;
 border-radius:7px;box-shadow:0 1px 3px rgba(20,40,80,.07);font-size:14.5px}
.headline .lab{font-size:11px;text-transform:uppercase;letter-spacing:.6px;color:var(--mut);margin-bottom:3px}
/* hero metric tiles */
.heroes{display:flex;flex-wrap:wrap;gap:12px;margin:14px 0}
.hero{flex:1 1 150px;background:var(--card);border:1px solid var(--line);border-top:3px solid #cdd3db;
 border-radius:10px;padding:13px 15px;box-shadow:0 1px 2px rgba(20,40,80,.05)}
.hero .v{font-size:25px;font-weight:700;line-height:1.1;font-variant-numeric:tabular-nums}
.hero .v .u{font-size:13px;font-weight:500;color:var(--mut);margin-left:2px}
.hero .k{font-size:11.5px;color:var(--mut);margin-top:5px;text-transform:uppercase;letter-spacing:.4px}
.hero.good{border-top-color:var(--good)} .hero.good .v{color:var(--good)}
.hero.warn{border-top-color:var(--warn)} .hero.warn .v{color:var(--warn)}
.hero.bad{border-top-color:var(--bad)} .hero.bad .v{color:var(--bad)}
.hero .flag{float:right;font-size:14px;line-height:1}
/* section card */
.sec{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:6px 20px 18px;
 margin:16px 0;box-shadow:0 1px 2px rgba(20,40,80,.05)}
.sec.fig{text-align:center}
/* dashboard grid: two columns that pack section cards side by side */
.grid{display:grid;grid-template-columns:1.08fr 1fr;gap:16px;align-items:start;margin:16px 0}
.grid .col{display:flex;flex-direction:column;gap:16px;min-width:0}
.grid .sec{margin:0}
.span2{grid-column:1 / -1}
/* compact multi-column platform/software info */
.envgrid{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:4px 28px}
.envgrid h3{font-size:12px;color:#6b7280;text-transform:uppercase;letter-spacing:.5px;margin:10px 0 2px}
@media(max-width:760px){.grid{grid-template-columns:1fr}}
h1{font-size:20px;margin:0} h2{font-size:15px;margin:18px 0 10px;color:#374151;font-weight:600}
.sub{color:#666;font-size:13px;margin:14px 0}
.badge{display:inline-block;background:rgba(255,255,255,.18);border:1px solid rgba(255,255,255,.35);
 border-radius:4px;padding:1px 8px;margin-right:6px;font-size:11px}
.ins{background:#fff8e1;border-left:4px solid #f0b400;padding:8px 12px;margin:6px 0;border-radius:3px}
/* stacked pipeline-slots bar */
.stack{display:flex;height:32px;border-radius:7px;overflow:hidden;margin:10px 0 4px;box-shadow:inset 0 0 0 1px rgba(0,0,0,.05)}
.stack>span{display:flex;align-items:center;justify-content:center;color:#fff;font-size:11px;font-weight:600;
 white-space:nowrap;overflow:hidden;min-width:0}
.legend{display:flex;flex-wrap:wrap;gap:16px;font-size:12px;color:#4b5563;margin:8px 0 2px}
.legend i{display:inline-block;width:11px;height:11px;border-radius:3px;margin-right:6px;vertical-align:-1px}
/* metric list (label · dot · value) */
.mlist{width:100%;border-collapse:collapse;font-size:13.5px}
.mlist td{padding:6px 4px;border-bottom:1px solid #f1f2f4}
.mlist tr:last-child td{border-bottom:none}
.mlist td.v{text-align:right;font-variant-numeric:tabular-nums;font-weight:600}
.dot{display:inline-block;width:8px;height:8px;border-radius:50%;margin-right:9px;vertical-align:1px}
.dot.good{background:var(--good)} .dot.warn{background:var(--warn)} .dot.bad{background:var(--bad)} .dot.none{background:#cbd5e1}
/* generic tables (upat) */
table{border-collapse:collapse;margin:6px 0 4px;font-size:13px}
th,td{padding:4px 10px;text-align:right;border-bottom:1px solid #eef0f2} th{background:#f3f5f8;text-align:right;font-weight:600}
td.l,th.l{text-align:left} tr:hover td{background:#f6f9ff}
.note{color:#777;font-size:12px;margin:2px 0 14px}
.cards{display:flex;flex-wrap:wrap;gap:10px;margin:8px 0}
.card{background:#fff;border:1px solid #e3e3e3;border-radius:6px;padding:8px 14px;min-width:120px}
.card .k{color:#777;font-size:12px} .card .v{font-size:18px;font-weight:600}
.hm{border-collapse:collapse;font-size:10px} .hm td{padding:0;border:none;width:14px;height:14px;text-align:center}
.hm th{padding:1px 3px;background:none;font-weight:400;color:#555;font-size:10px}
.bar{background:#4a90d9;height:13px;display:inline-block;border-radius:2px;vertical-align:middle}
abbr.gloss{text-decoration:underline dotted #aaa;text-underline-offset:2px;cursor:help}
"""


class _Raw(str):
    """A cell whose contents are already safe HTML and must not be re-escaped."""


def _term(label):
    """Wrap a label in an <abbr> tooltip when the shared glossary defines it (Intel
    APS-style mouse-over); otherwise return the plain label. Result is safe HTML."""
    d = viewpoints.define(label)
    if d:
        return _Raw("<abbr class='gloss' title=\"%s\">%s</abbr>" % (E(d), E(label)))
    return label


def _fmt(v):
    if isinstance(v, float):
        return ("%.3f" % v) if abs(v) < 1e6 else ("%.3e" % v)
    return str(v)


def _cell(c):
    """Escape a table cell unless it is already-safe HTML (_Raw)."""
    return str(c) if isinstance(c, _Raw) else E(_fmt(c))


def _table(headers, rows, leftcols=(0,)):
    h = "".join('<th class="l">%s</th>' % E(c) if i in leftcols else "<th>%s</th>" % E(c)
                for i, c in enumerate(headers))
    body = []
    for r in rows:
        tds = "".join('<td class="l">%s</td>' % _cell(c) if i in leftcols else "<td>%s</td>" % _cell(c)
                      for i, c in enumerate(r))
        body.append("<tr>%s</tr>" % tds)
    return "<table><tr>%s</tr>%s</table>" % (h, "".join(body))


def _metrics(snap):
    return {m["key"]: m for m in (snap or {}).get("metrics", [])}


# ---------------------------------------------------------------- roofline SVG
def _roofline_svg(pk, point):
    if not pk:
        return ""
    W, H, pad = 520, 320, 44
    dp, sp, bw = roofline.peak_compute(pk, "dp"), roofline.peak_compute(pk, "sp"), pk.get("peak_bw_gbs") or 1
    xmin, xmax, ymin, ymax = 0.1, 1000.0, 1.0, (sp or dp) * 1.3
    # Grow the window to whole decades that contain the measured point — otherwise a
    # memory-bound point left of the default window (e.g. STREAM at AI≈0.06) clamps to
    # the left edge and looks detached from the bandwidth roof instead of sitting on it.
    if point:
        if point.get("ai", 0) > 0:
            xmin = min(xmin, 10 ** math.floor(math.log10(point["ai"])))
            xmax = max(xmax, 10 ** math.ceil(math.log10(point["ai"])))
        if point.get("gflops", 0) > 0:
            ymin = min(ymin, 10 ** math.floor(math.log10(point["gflops"])))
    lx0, lx1, ly0, ly1 = math.log10(xmin), math.log10(xmax), math.log10(ymin), math.log10(ymax)

    def X(ai):
        return pad + (math.log10(max(ai, xmin)) - lx0) / (lx1 - lx0) * (W - 2 * pad)

    def Y(g):
        return H - pad - (math.log10(max(g, ymin)) - ly0) / (ly1 - ly0) * (H - 2 * pad)
    parts = ['<svg viewBox="0 0 %d %d" width="%d" height="%d" preserveAspectRatio="xMidYMid meet" '
             'style="max-width:100%%;height:auto;background:#fff;border:1px solid #e3e3e3;border-radius:6px">'
             % (W, H, W, H)]
    # axes
    parts.append('<line x1="%g" y1="%g" x2="%g" y2="%g" stroke="#bbb"/>' % (pad, H - pad, W - pad, H - pad))
    parts.append('<line x1="%g" y1="%g" x2="%g" y2="%g" stroke="#bbb"/>' % (pad, pad, pad, H - pad))
    parts.append('<text x="%g" y="%g" font-size="11" fill="#666">AI (FLOP/byte) →'
                 '<title>%s</title></text>' % (W / 2 - 40, H - 14, E(viewpoints.define("AI") or "")))
    parts.append('<text x="14" y="%g" font-size="11" fill="#666" transform="rotate(-90 14 %g)">GFLOP/s →</text>' % (H / 2 + 30, H / 2 + 30))
    # ceilings (memory diagonal y=bw*ai, then flat at peak) for DP and SP
    for peak, col, lbl in ((dp, "#c0392b", "FP64"), (sp, "#2980b9", "FP32")):
        if not peak:
            continue
        ridge = peak / bw
        parts.append('<polyline points="%g,%g %g,%g %g,%g" fill="none" stroke="%s" stroke-width="2"/>' % (
            X(xmin), Y(bw * xmin), X(ridge), Y(peak), X(xmax), Y(peak), col))
        parts.append('<text x="%g" y="%g" font-size="11" fill="%s">%s %.0f GF/s</text>' % (
            W - pad - 86, Y(peak) - 4, col, lbl, peak))
    # the point
    if point:
        px, py = X(point["ai"]), Y(point["gflops"])
        parts.append('<circle cx="%g" cy="%g" r="5" fill="#27ae60" stroke="#145a32"/>' % (px, py))
        parts.append('<text x="%g" y="%g" font-size="11" fill="#145a32">%s</text>' % (
            px + 8, py - 6, E(point["label"])))
    parts.append("</svg>")
    return "".join(parts)


# ---------------------------------------------------------------- comm heatmap
def _sent_pairs(arr):
    """{peer: bytes} from a rank's 'sent', accepting the sparse [[peer,bytes],...]
    format and the legacy dense [bytes,...] array (peer = index)."""
    arr = arr or []
    if arr and isinstance(arr[0], (list, tuple)):
        return {int(p): b for p, b in arr if b}
    return {i: b for i, b in enumerate(arr) if b}


def _rank_of_path(p):
    """Rank id from a 'prof.<rank>.json' path (matches contract.prof_name)."""
    try:
        return int(os.path.basename(p).split(".")[1])
    except (IndexError, ValueError):
        return None


def _comm_matrix(result_dir, B=256):
    """Return (labels, mat[bi][bj]->bytes, bucketed?) from raw per-rank mpi_detail.

    Two-pass streaming so even a genuine all-to-all stays O(B^2 + N), never O(N^2):
      pass 1 fixes the <=B x B bucket geometry from the rank ids (read from the
             filenames — no file I/O),
      pass 2 reads each rank file ONCE, folds its sparse [peer,bytes] pairs into the
             bounded grid, and discards it (only one file is held at a time).
    Back-compatible with the legacy dense 'sent' array via _sent_pairs."""
    ranked = sorted((r, p) for p in contract.prof_glob(result_dir)
                    for r in (_rank_of_path(p),) if r is not None)
    if not ranked:
        return None
    ranks = [r for r, _ in ranked]
    n = len(ranks)
    idx = {r: i for i, r in enumerate(ranks)}
    # downsample to <=B buckets so thousand-rank jobs stay legible
    if n <= B:
        nb, step, labels = n, 1, [str(r) for r in ranks]
    else:
        step = math.ceil(n / B)
        nb = math.ceil(n / step)
        labels = ["%d-%d" % (ranks[k * step], ranks[min((k + 1) * step - 1, n - 1)])
                  for k in range(nb)]
    mat = [[0.0] * nb for _ in range(nb)]
    any_edge = False
    for r, p in ranked:                       # pass 2: stream one file at a time
        try:
            d = json.load(open(p))
        except Exception:
            continue
        m = _sent_pairs((d.get("mpi_detail") or {}).get("sent"))
        row = mat[idx[r] // step]
        for peer, b in m.items():
            j = idx.get(peer)
            if j is not None:
                row[j // step] += b
                any_edge = True
        del d, m                              # free before the next rank
    if not any_edge:
        return None
    return labels, mat, n > B


def _heatmap_html(result_dir):
    cm = _comm_matrix(result_dir)
    if not cm:
        return "<p class='note'>(no point-to-point communication recorded)</p>"
    labels, mat, bucketed = cm
    n = len(labels)
    mx = max((mat[i][j] for i in range(n) for j in range(n)), default=0) or 1
    show = n <= 32

    def color(v):
        if v <= 0:
            return "#f7f7f7"
        t = math.log1p(v) / math.log1p(mx)
        return "rgb(255,%d,%d)" % (int(235 * (1 - t)), int(235 * (1 - t)))
    head = "<th></th>" + "".join("<th>%s</th>" % E(l) for l in labels)
    rows = []
    for i in range(n):
        cells = []
        for j in range(n):
            v = mat[i][j]
            txt = ("%.0f" % (v / 1e6)) if (show and v > 0) else ""
            cells.append('<td style="background:%s" title="%s→%s: %.1f MB">%s</td>'
                         % (color(v), E(labels[i]), E(labels[j]), v / 1e6, txt))
        rows.append("<tr><th>%s</th>%s</tr>" % (E(labels[i]), "".join(cells)))
    note = ("down-sampled to %d×%d buckets (>%d ranks)" % (n, n, 256)) if bucketed \
        else "cell = MB sent from row-rank to col-rank (hover for value)"
    return ("<table class='hm'><tr>%s</tr>%s</table><p class='note'>%s; darker = more data.</p>"
            % (head, "".join(rows), note))


# ---------------------------------------------------------------- size histo
def _size_histogram(result_dir):
    BINS = ["<=64B", "<=256B", "<=1KiB", "<=4KiB", "<=16KiB", "<=64KiB",
            "<=256KiB", "<=1MiB", "<=4MiB", "<=16MiB", "<=64MiB", ">64MiB"]
    tot = [0] * len(BINS)
    for p in contract.prof_glob(result_dir):
        try:
            md = json.load(open(p)).get("mpi_detail") or {}
        except Exception:
            continue
        for i, v in enumerate(md.get("bins", [])):
            tot[i] += v
    n = sum(tot)
    if not n:
        return ""
    mx = max(tot) or 1
    rows = []
    for lab, c in zip(BINS, tot):
        if c:
            w = int(260 * c / mx)
            rows.append("<tr><td class='l'>%s</td><td>%d</td><td>%.1f%%</td>"
                        "<td class='l'><span class='bar' style='width:%dpx'></span></td></tr>"
                        % (E(lab), c, 100.0 * c / n, w))
    return ("<table><tr><th class='l'>size</th><th>count</th><th>%</th>"
            "<th class='l'></th></tr>" + "".join(rows) + "</table>")


# ---------------------------------------------------------------- profile tables
COMPUTE = {"BLAS", "LAPACK", "PBLAS", "ScaLAPACK", "CBLAS", "LAPACKe", "FFTW"}


def _profile_tables(profile, threshold=0.1):
    out = []
    fns = (profile or {}).get("functions", [])
    rt = (profile or {}).get("cpu_time_s") or (profile or {}).get("runtime_s", 0.0)  # CPU-time basis

    def keep(seq):
        if threshold > 0 and rt > 0:
            return [f for f in seq if f.get("t_incl", 0) / rt * 100.0 >= threshold]
        return list(seq)
    def tpct(f):                              # inclusive time as % of wall runtime
        return "%.1f%%" % (100.0 * f.get("t_incl", 0) / rt) if rt > 0 else "—"
    comp = keep(sorted((f for f in fns if f.get("group", "") in COMPUTE), key=lambda f: -f.get("t_incl", 0)))[:40]
    if comp:
        out.append("<h2>Compute (BLAS / LAPACK / FFTW)</h2>")
        out.append(_table(["time%", "group", "function", "count", "incl(s)", "excl(s)"],
                          [[tpct(f), f.get("group", ""), f.get("name", ""), "%.0f" % f.get("count", 0),
                            "%.4f" % f.get("t_incl", 0), "%.4f" % f.get("t_excl", 0)]
                           for f in comp], leftcols=(1, 2)))
        out.append("<p class='note'>time%% = inclusive time / total CPU time (thread-seconds); "
                   "calls aggregated over input sizes; below %.3g%% hidden; incl = routine+callees, "
                   "excl = routine only.</p>" % threshold)
    mpi = keep(sorted((f for f in fns if f.get("group", "") == "MPI"), key=lambda f: -f.get("t_incl", 0)))
    if mpi:
        out.append("<h2>MPI communication</h2>")
        out.append(_table(["time%", "function", "count", "incl(s)", "bytes", "GB/s"],
                          [[tpct(f), f.get("name", ""), "%.0f" % f.get("count", 0), "%.4f" % f.get("t_incl", 0), f.get("bytes", 0),
                            "%.2f" % (f.get("bytes", 0) / f["t_incl"] / 1e9 if f.get("t_incl", 0) > 0 else 0)] for f in mpi],
                          leftcols=(1,)))
    io = keep(sorted((f for f in fns if f.get("group", "") == "IO"), key=lambda f: -f.get("t_incl", 0)))
    if io:
        out.append("<h2>I/O</h2>")
        out.append(_table(["time%", "call", "count", "incl(s)", "bytes"],
                          [[tpct(f), f.get("name", ""), "%.0f" % f.get("count", 0), "%.4f" % f.get("t_incl", 0), f.get("bytes", 0)]
                           for f in io], leftcols=(1,)))
    return "".join(out)


# ---------------------------------------------------------------- pages
def _page(title, body):
    return ("<!doctype html><html><head><meta charset='utf-8'><title>%s</title><style>%s</style></head>"
            "<body><div class='wrap'>%s</div></body></html>" % (E(title), CSS, body))


def _whole_program_point(snap):
    m = _metrics(snap)
    g = m.get("gflops", {}).get("value")
    fills = m.get("mem_fills_dram", {}).get("value")
    el = m.get("elapsed_time", {}).get("value")
    if g and fills and el:
        b = fills * 64.0
        if b > 0:
            return {"label": "whole-program", "ai": g * 1e9 * el / b, "gflops": g}
    return None


# --------------------------------------------------------- APS-style widgets
# top-down pipeline-slot segment colours (retiring is "good", stalls warm/hot).
_SLOT_COLORS = {"retiring": "#2e7d32", "frontend-bound": "#e0a800",
                "backend-bound": "#e8743b", "bad speculation": "#9aa0a6",
                "SMT contention": "#7fb3d5"}


def _band(x, warn, bad, hi=True):
    """Severity for a value: hi=True → larger is worse; hi=False → smaller is worse."""
    if x is None:
        return ""
    if hi:
        return "bad" if x >= bad else "warn" if x >= warn else "good"
    return "bad" if x <= bad else "warn" if x <= warn else "good"


def _status(key, v):
    """good/warn/bad for the metrics where a threshold is meaningful; '' otherwise.
    Deliberately conservative — only flag genuinely actionable signals (so an
    intentionally serial run isn't painted red for low core utilisation)."""
    rules = {
        "memory_bound": (20, 40, True), "dram_bound_pct": (35, 65, True),
        "cache_miss_rate": (10, 30, True), "branch_mispredict_rate": (3, 6, True),
        "numa_remote_pct": (10, 25, True), "mpi_imbalance_pct": (15, 30, True),
        "ipc": (1.0, 0.5, False), "fp_eff": (25, 10, False),
    }
    r = rules.get(key)
    return _band(v, *r) if r else ""


def _hero(label, value, unit="", status="", tip=None):
    flag = "⚠" if status in ("warn", "bad") else ""
    k = ("<abbr class='gloss' title=\"%s\">%s</abbr>" % (E(tip), E(label))) if tip else E(label)
    u = "<span class='u'>%s</span>" % E(unit) if unit else ""
    return ("<div class='hero %s'><span class='flag'>%s</span>"
            "<div class='v'>%s%s</div><div class='k'>%s</div></div>"
            % (status, flag, E(value), u, k))


def _slot_bar(segments):
    """A stacked horizontal bar of (label, pct) pipeline slots + a colour legend."""
    seg = [(l, p) for l, p in segments if p and p > 0]
    if not seg:
        return ""
    bar = "".join(
        "<span style='width:%.3f%%;background:%s' title=\"%s — %.1f%%\">%s</span>"
        % (p, _SLOT_COLORS.get(l, "#888"), E(l), p, ("%.0f%%" % p if p >= 8 else ""))
        for l, p in seg)
    leg = "".join("<span><i style='background:%s'></i>%s — %.1f%%</span>"
                  % (_SLOT_COLORS.get(l, "#888"), E(l), p) for l, p in seg)
    return "<div class='stack'>%s</div><div class='legend'>%s</div>" % (bar, leg)


def _mlist(rows):
    """Metric list: (label, display, status) → label·dot·right-aligned value table.
    Labels get glossary tooltips automatically."""
    trs = "".join("<tr><td><span class='dot %s'></span>%s</td><td class='v'>%s</td></tr>"
                  % (st or "none", _cell(_term(lab)), E(val)) for lab, val, st in rows)
    return "<table class='mlist'>%s</table>" % trs


def _hdr_band(title, badge, cmd, env):
    """The coloured header band: tool title, command, and a one-line run summary."""
    bits = []
    if env.get("application"):
        bits.append("<b>%s</b>" % E(env["application"]))
    if env.get("host"):
        bits.append(E(env["host"]))
    if env.get("ranks"):
        bits.append(E(env["ranks"] + (" rank" if env["ranks"] == "1" else " ranks")))
    if env.get("threads/rank") and env["threads/rank"] != "1":
        bits.append(E(env["threads/rank"] + " thr/rank"))
    if env.get("elapsed"):
        bits.append(E("elapsed " + env["elapsed"]))
    if env.get("date"):
        bits.append(E(env["date"]))
    meta = " &nbsp;·&nbsp; ".join(bits)
    return ("<div class='hdr'><h1>%s <span class='badge'>%s</span></h1>"
            "<div class='cmd'>%s</div><div class='meta'>%s</div></div>"
            % (E(title), E(badge), E(cmd), meta))


def build(result_dir, manifest, snap, profile, suite, detail=None, threshold=0.1, collector="upat"):
    cmd = " ".join(manifest.get("command", [])) if manifest else ""
    # Single cost tier per report — never combined (see report.render).
    do_uaps = collector == "uaps"              # snapshot: roofline + microarch cards
    do_upat = collector == "upat"              # deep profile: tables + comm matrix

    if detail == "mpi":
        body = ["<h1>MPI analysis</h1><div class='sub'>%s</div>" % E(cmd)]
        body.append("<h2>Communication matrix</h2>")
        body.append(_heatmap_html(result_dir))
        body.append("<h2>Message-size distribution</h2>")
        body.append(_size_histogram(result_dir) or "<p class='note'>(none)</p>")
        body.append("<h2>MPI calls</h2>")
        fns = (profile or {}).get("functions", [])
        rt = (profile or {}).get("cpu_time_s") or (profile or {}).get("runtime_s", 0.0)
        mpi = sorted((f for f in fns if f.get("group", "") == "MPI"), key=lambda f: -f.get("t_incl", 0))
        body.append(_table(["time%", "function", "count", "incl(s)", "bytes", "GB/s"],
                           [["%.1f%%" % (100.0 * f.get("t_incl", 0) / rt) if rt > 0 else "—",
                             f.get("name", ""), "%.0f" % f.get("count", 0), "%.4f" % f.get("t_incl", 0), f.get("bytes", 0),
                             "%.2f" % (f.get("bytes", 0) / f["t_incl"] / 1e9 if f.get("t_incl", 0) > 0 else 0)]
                            for f in mpi], leftcols=(1,)))
        return _page("MPI analysis", "".join(body))

    pk = roofline.peaks()
    m = _metrics(snap)
    h1 = ("UAPS — Universal Application Performance Snapshot" if do_uaps
          else "UPAT — Universal Performance Analysis Tool")
    env_sections = viewpoints.environment_rows(result_dir, manifest, snap, profile, collector)
    env = {k: v for _, items in env_sections for k, v in items}

    # Coloured header band + the headline bottleneck (first insight) up top.
    body = [_hdr_band(h1, "uaps" if do_uaps else "upat", cmd, env)]
    if suite:
        body.append("<div class='headline'><div class='lab'>Headline</div>%s</div>" % E(suite[0]))

    def disp(k):
        return m[k].get("display", _fmt(m[k].get("value"))) if k in m else None

    def val(k):
        return m.get(k, {}).get("value")

    if do_uaps and snap:
        # --- hero metric tiles (big numbers, severity-coloured where actionable) ---
        fp_eff = None
        if val("gflops") and pk and pk.get("peak_gflops"):
            fp_eff = val("gflops") / pk["peak_gflops"] * 100.0
        heroes = []
        if disp("elapsed_time"):
            heroes.append(_hero("elapsed", disp("elapsed_time")))
        if val("gflops") is not None:
            heroes.append(_hero("FP throughput", "%.1f" % val("gflops"), "GFLOP/s",
                                _status("fp_eff", fp_eff), viewpoints.define("GFLOP/s")))
        if disp("cpu_freq_ghz"):
            heroes.append(_hero("avg frequency", "%.2f" % val("cpu_freq_ghz"), "GHz"))
        if disp("ipc"):
            heroes.append(_hero("IPC", "%.2f" % val("ipc"), "", _status("ipc", val("ipc")),
                                viewpoints.define("IPC")))
        if disp("cpu_core_pct"):
            heroes.append(_hero("core utilization", disp("cpu_core_pct"), "",
                                "", viewpoints.define("core util")))
        if val("memory_bound") is not None:
            heroes.append(_hero("memory bound", "%.1f" % val("memory_bound"), "%",
                                _status("memory_bound", val("memory_bound")),
                                viewpoints.define("memory-bound (slots)")))
        if fp_eff is not None:
            heroes.append(_hero("vectorization", "%.0f" % fp_eff, "% of peak",
                                _status("fp_eff", fp_eff), viewpoints.define("FP efficiency")))
        if heroes:
            body.append("<div class='heroes'>%s</div>" % "".join(heroes))

        # Analysis cards laid out in a two-column dashboard grid (uses the
        # horizontal space instead of one tall vertical stack). Left column gets
        # the metric lists; the roofline figure anchors the right column.
        left, right = [], []

        # --- pipeline slots as a stacked bar (the APS signature visual) → left ---
        four = {l: val(k) for l, k in
                [("retiring", "topdown_retiring_pct"), ("frontend-bound", "topdown_frontend_pct"),
                 ("backend-bound", "topdown_backend_pct"), ("bad speculation", "topdown_badspec_pct")]}
        if all(v is not None for v in four.values()):
            segs = list(four.items())
            if val("smt_active"):
                segs.append(("SMT contention", max(0.0, 100.0 - sum(four.values()))))
            bar = _slot_bar(segs)
            if bar:
                left.append("<div class='sec'><h2>Pipeline slots "
                            "<abbr class='gloss' title=\"%s\">(top-down)</abbr></h2>%s</div>"
                            % (E("how each CPU issue slot was spent: useful work vs. the kind of stall"), bar))

        # --- memory access (metric list with severity dots + tooltips) → left ---
        mem = []
        for k, lbl, skey in [
                ("dram_bandwidth_gbs", "DRAM bandwidth", None),
                ("cache_miss_rate", "cache-miss rate", "cache_miss_rate"),
                ("llc_mpki", "last-level cache misses", None),
                ("dtlb_mpki", "data-TLB misses", None),
                ("itlb_mpki", "instruction-TLB misses", None),
                ("dram_bound_pct", "DRAM-bound", "dram_bound_pct"),
                ("numa_remote_pct", "NUMA remote access", "numa_remote_pct"),
                ("branch_mispredict_rate", "branch mispredict", "branch_mispredict_rate")]:
            if disp(k) is not None:
                mem.append((lbl, disp(k), _status(skey, val(k)) if skey else "none"))
        if mem:
            left.append("<div class='sec'><h2>Memory &amp; microarchitecture</h2>%s</div>" % _mlist(mem))

        # --- roofline figure → right (anchors the column, scales to fit) ---
        if pk:
            right.append("<div class='sec fig'><h2 style='text-align:left'>Roofline</h2>%s</div>"
                         % _roofline_svg(pk, _whole_program_point(snap)))

        # --- MPI bird's-eye (snapshot shim), if this was an MPI run → right ---
        if val("mpi_time") is not None:
            mrows = [(lbl, disp(k), _status("mpi_imbalance_pct", val(k)) if k == "mpi_imbalance_pct" else "none")
                     for k, lbl in [("mpi_time", "MPI time"), ("mpi_time_pct", "MPI % of runtime"),
                                    ("mpi_imbalance_pct", "MPI imbalance"), ("mpi_ranks", "ranks")]
                     if disp(k) is not None]
            # top calls by time (the snapshot's mpi_top1..5 — label carries the % share)
            tops = [(x.get("label", "").strip(), x.get("display", ""))
                    for key in ("mpi_top1", "mpi_top2", "mpi_top3", "mpi_top4", "mpi_top5")
                    for x in (snap or {}).get("metrics", []) if x.get("key") == key]
            if mrows:
                sec = "<div class='sec'><h2>MPI</h2>%s" % _mlist(mrows)
                if tops:
                    sec += ("<h2 style='font-size:13px;margin:14px 0 4px'>Top calls (time)</h2>"
                            + _table(["MPI call", "time"], [[c, t] for c, t in tops], leftcols=(0,)))
                right.append(sec + "</div>")

        if left and right:
            body.append("<div class='grid'><div class='col'>%s</div><div class='col'>%s</div></div>"
                        % ("".join(left), "".join(right)))
        else:
            body.extend(left + right)

    # APS-style top-5 MPI functions (bird's-eye, upat tier)
    mpis = sorted((f for f in (profile or {}).get("functions", []) if f.get("group", "") == "MPI"),
                  key=lambda f: -f.get("t_incl", 0))
    if do_upat and mpis and (profile or {}).get("nranks", 1) >= 2:
        rt = (profile or {}).get("runtime_s", 0.0)
        mt = sum(f.get("t_incl", 0) for f in mpis)
        body.append("<div class='sec'><h2>Top MPI functions</h2>")
        body.append("<p class='note'>MPI time %.4fs (%.1f%% of runtime, %d ranks)</p>"
                    % (mt, (mt / rt * 100 if rt else 0), profile.get("nranks", 1)))
        body.append(_table(["function", "time(s)", "%MPI", "calls", "imb%"],
                           [[f.get("name", ""), "%.4f" % f.get("t_incl", 0), "%.1f" % (f.get("t_incl", 0) / mt * 100 if mt else 0),
                             "%.0f" % f.get("count", 0), "%.0f" % f.get("imb_excl", 0)] for f in mpis[:5]]))
        body.append("</div>")

    # UPAT profile tables
    if do_upat:
        body.append("<div class='sec'><h2>Deep profile</h2>")
        body.append(_profile_tables(profile, threshold))
        if _comm_matrix(result_dir):           # comm matrix figure if MPI ran
            body.append("<h2>MPI communication matrix</h2>")
            body.append(_heatmap_html(result_dir))
        body.append("</div>")

    # extra insights (beyond the headline) + the full environment, at the bottom
    if len(suite) > 1:
        body.append("<div class='sec'><h2>Insights</h2>%s</div>"
                    % "".join("<div class='ins'>%s</div>" % E(s) for s in suite[1:]))
    body.append("<div class='sec'><h2>System &amp; software</h2><div class='envgrid'>")
    for title, items in env_sections:
        if items:
            body.append("<div><h3>%s</h3>%s</div>"
                        % (E(title), _table(["", ""], [[_term(k), v] for k, v in items], leftcols=(0, 1))))
    body.append("</div></div>")
    return _page("UAPS snapshot" if do_uaps else "UPAT profile", "".join(body))
