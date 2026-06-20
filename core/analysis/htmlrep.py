"""Self-contained HTML report generator for the suite.

`perfsuite report --format html`         → the combined report
`perfsuite report --detail mpi --format html` → the MPI analysis (comm-matrix
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

E = _html.escape
CSS = """
body{font:14px/1.5 -apple-system,Segoe UI,Roboto,sans-serif;margin:0;color:#1a1a1a;background:#fafafa}
.wrap{max-width:1100px;margin:0 auto;padding:24px}
h1{font-size:22px;margin:0 0 4px} h2{font-size:17px;margin:28px 0 8px;border-bottom:2px solid #ddd;padding-bottom:4px}
.sub{color:#666;font-size:13px;margin-bottom:16px}
.badge{display:inline-block;background:#eef;border:1px solid #ccd;border-radius:4px;padding:1px 7px;margin-right:6px;font-size:12px}
.ins{background:#fff8e1;border-left:4px solid #f0b400;padding:8px 12px;margin:4px 0;border-radius:3px}
table{border-collapse:collapse;margin:6px 0 4px;font-size:13px}
th,td{padding:3px 10px;text-align:right;border-bottom:1px solid #eee} th{background:#f0f0f0;text-align:right}
td.l,th.l{text-align:left} tr:hover td{background:#f6f9ff}
.note{color:#777;font-size:12px;margin:2px 0 14px}
.cards{display:flex;flex-wrap:wrap;gap:10px;margin:8px 0}
.card{background:#fff;border:1px solid #e3e3e3;border-radius:6px;padding:8px 14px;min-width:120px}
.card .k{color:#777;font-size:12px} .card .v{font-size:18px;font-weight:600}
.hm{border-collapse:collapse;font-size:10px} .hm td{padding:0;border:none;width:14px;height:14px;text-align:center}
.hm th{padding:1px 3px;background:none;font-weight:400;color:#555;font-size:10px}
.bar{background:#4a90d9;height:13px;display:inline-block;border-radius:2px;vertical-align:middle}
"""


def _fmt(v):
    if isinstance(v, float):
        return ("%.3f" % v) if abs(v) < 1e6 else ("%.3e" % v)
    return str(v)


def _table(headers, rows, leftcols=(0,)):
    h = "".join('<th class="l">%s</th>' % E(c) if i in leftcols else "<th>%s</th>" % E(c)
                for i, c in enumerate(headers))
    body = []
    for r in rows:
        tds = "".join('<td class="l">%s</td>' % E(_fmt(c)) if i in leftcols else "<td>%s</td>" % E(_fmt(c))
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
    lx0, lx1, ly0, ly1 = math.log10(xmin), math.log10(xmax), math.log10(ymin), math.log10(ymax)

    def X(ai):
        return pad + (math.log10(max(ai, xmin)) - lx0) / (lx1 - lx0) * (W - 2 * pad)

    def Y(g):
        return H - pad - (math.log10(max(g, ymin)) - ly0) / (ly1 - ly0) * (H - 2 * pad)
    parts = ['<svg width="%d" height="%d" style="background:#fff;border:1px solid #e3e3e3;border-radius:6px">' % (W, H)]
    # axes
    parts.append('<line x1="%g" y1="%g" x2="%g" y2="%g" stroke="#bbb"/>' % (pad, H - pad, W - pad, H - pad))
    parts.append('<line x1="%g" y1="%g" x2="%g" y2="%g" stroke="#bbb"/>' % (pad, pad, pad, H - pad))
    parts.append('<text x="%g" y="%g" font-size="11" fill="#666">AI (FLOP/byte) →</text>' % (W / 2 - 40, H - 14))
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
def _comm_matrix(result_dir):
    """Return (labels, get(i,j)->bytes, bucketed?) from raw per-rank mpi_detail."""
    sent = {}
    for p in contract.prof_glob(result_dir):
        try:
            d = json.load(open(p))
        except Exception:
            continue
        md = d.get("mpi_detail")
        if md and md.get("sent"):
            sent[d.get("rank", 0)] = md["sent"]
    ranks = sorted(sent)
    if not ranks or not any(sum(v) for v in sent.values()):
        return None
    n = len(ranks)
    idx = {r: i for i, r in enumerate(ranks)}
    raw = [[0.0] * n for _ in range(n)]
    for r in ranks:
        v = sent[r]
        for j, b in enumerate(v):
            if j in idx:
                raw[idx[r]][idx[j]] = b
    # downsample to <=256 buckets so thousand-rank jobs stay legible
    B = 256
    if n <= B:
        return [str(r) for r in ranks], raw, False
    step = math.ceil(n / B)
    nb = math.ceil(n / step)
    agg = [[0.0] * nb for _ in range(nb)]
    for i in range(n):
        for j in range(n):
            agg[i // step][j // step] += raw[i][j]
    labels = ["%d-%d" % (ranks[k * step], ranks[min((k + 1) * step - 1, n - 1)]) for k in range(nb)]
    return labels, agg, True


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
    rt = (profile or {}).get("runtime_s", 0.0)

    def keep(seq):
        if threshold > 0 and rt > 0:
            return [f for f in seq if f["t_incl"] / rt * 100.0 >= threshold]
        return list(seq)
    comp = keep(sorted((f for f in fns if f["group"] in COMPUTE), key=lambda f: -f["t_excl"]))[:40]
    if comp:
        out.append("<h2>Compute (BLAS / LAPACK / FFTW)</h2>")
        out.append(_table(["group", "function", "count", "incl(s)", "excl(s)"],
                          [[f["group"], f["name"], "%.0f" % f["count"], "%.4f" % f["t_incl"], "%.4f" % f["t_excl"]]
                           for f in comp], leftcols=(0, 1)))
        out.append("<p class='note'>calls aggregated over input sizes; below %.3g%% of runtime hidden; "
                   "incl = routine+callees, excl = routine only.</p>" % threshold)
    mpi = keep(sorted((f for f in fns if f["group"] == "MPI"), key=lambda f: -f["t_incl"]))
    if mpi:
        out.append("<h2>MPI communication</h2>")
        out.append(_table(["function", "count", "incl(s)", "bytes", "GB/s"],
                          [[f["name"], "%.0f" % f["count"], "%.4f" % f["t_incl"], f["bytes"],
                            "%.2f" % (f["bytes"] / f["t_incl"] / 1e9 if f["t_incl"] > 0 else 0)] for f in mpi]))
    io = keep(sorted((f for f in fns if f["group"] == "IO"), key=lambda f: -f["t_incl"]))
    if io:
        out.append("<h2>I/O</h2>")
        out.append(_table(["call", "count", "incl(s)", "bytes"],
                          [[f["name"], "%.0f" % f["count"], "%.4f" % f["t_incl"], f["bytes"]] for f in io]))
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


def build(result_dir, manifest, snap, profile, suite, detail=None, threshold=0.1):
    cmd = " ".join(manifest.get("command", [])) if manifest else ""

    if detail == "mpi":
        body = ["<h1>MPI analysis</h1><div class='sub'>%s</div>" % E(cmd)]
        body.append("<h2>Communication matrix</h2>")
        body.append(_heatmap_html(result_dir))
        body.append("<h2>Message-size distribution</h2>")
        body.append(_size_histogram(result_dir) or "<p class='note'>(none)</p>")
        body.append("<h2>MPI calls</h2>")
        fns = (profile or {}).get("functions", [])
        mpi = sorted((f for f in fns if f["group"] == "MPI"), key=lambda f: -f["t_incl"])
        body.append(_table(["function", "count", "incl(s)", "bytes", "GB/s"],
                           [[f["name"], "%.0f" % f["count"], "%.4f" % f["t_incl"], f["bytes"],
                             "%.2f" % (f["bytes"] / f["t_incl"] / 1e9 if f["t_incl"] > 0 else 0)] for f in mpi]))
        return _page("MPI analysis", "".join(body))

    pk = roofline.peaks()
    m = _metrics(snap)
    body = ["<h1>Performance Suite report</h1><div class='sub'><span class='badge'>uaps</span>"
            "<span class='badge'>upat</span> %s</div>" % E(cmd)]
    if suite:
        body.append("<h2>Insights</h2>")
        body += ["<div class='ins'>%s</div>" % E(s) for s in suite]

    # UAPS snapshot cards
    if snap:
        body.append("<h2>UAPS — snapshot</h2><div class='cards'>")
        for k, lbl in [("elapsed_time", "elapsed"), ("cpu_core_pct", "core util"), ("ipc", "IPC"),
                       ("gflops", "GFLOP/s"), ("memory_bound", "mem-bound"), ("peak_rss", "peak RSS")]:
            if k in m:
                body.append("<div class='card'><div class='k'>%s</div><div class='v'>%s</div></div>"
                            % (E(lbl), E(m[k].get("display", _fmt(m[k].get("value"))))))
        body.append("</div>")
        # roofline figure
        if pk:
            body.append("<h2>Roofline</h2>")
            body.append(_roofline_svg(pk, _whole_program_point(snap)))
        # microarch / memory
        rows = [[lbl, m[k]["display"]] for k, lbl in
                [("topdown_retiring_pct", "retiring"), ("topdown_frontend_pct", "frontend-bound"),
                 ("topdown_backend_pct", "backend-bound"), ("cache_miss_rate", "cache-miss"),
                 ("llc_mpki", "LLC MPKI"), ("dram_bound_pct", "DRAM-bound"), ("numa_remote_pct", "NUMA remote")]
                if k in m]
        if rows:
            body.append("<h2>Microarchitecture &amp; memory</h2>")
            body.append(_table(["metric", "value"], rows, leftcols=(0,)))

    # APS-style top-5 MPI functions (bird's-eye)
    mpis = sorted((f for f in (profile or {}).get("functions", []) if f["group"] == "MPI"),
                  key=lambda f: -f["t_incl"])
    if mpis and (profile or {}).get("nranks", 1) >= 2:
        rt = (profile or {}).get("runtime_s", 0.0)
        mt = sum(f["t_incl"] for f in mpis)
        body.append("<h2>Top MPI functions</h2>")
        body.append("<p class='note'>MPI time %.4fs (%.1f%% of runtime, %d ranks)</p>"
                    % (mt, (mt / rt * 100 if rt else 0), profile.get("nranks", 1)))
        body.append(_table(["function", "time(s)", "%MPI", "calls", "imb%"],
                           [[f["name"], "%.4f" % f["t_incl"], "%.1f" % (f["t_incl"] / mt * 100 if mt else 0),
                             "%.0f" % f["count"], "%.0f" % f.get("imb_excl", 0)] for f in mpis[:5]]))

    # UPAT profile tables
    body.append("<h2 style='border-color:#88a'>UPAT — deep profile</h2>")
    body.append(_profile_tables(profile, threshold))
    # comm matrix figure if MPI ran
    cm = _comm_matrix(result_dir)
    if cm:
        body.append("<h2>MPI communication matrix</h2>")
        body.append(_heatmap_html(result_dir))
    return _page("Performance Suite report", "".join(body))
