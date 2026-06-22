#!/usr/bin/env python3
"""upat-report - postprocess raw upat per-rank JSON into a report.

The profiled run writes one upat.<rank>.json per process (no analysis).
This tool reduces across ranks and prints a COMPUTE table (BLAS/LAPACK/...) and a
separate MPI table (with communication volume / GB/s), plus load imbalance.

Usage:
  upat-report upat.*.json
  upat-report out/                       # a directory of json files
  upat-report --imbalance world --format csv upat.*.json
"""
import sys, os, re, json, glob, argparse, subprocess, collections, heapq

# One consistent table rule everywhere: a solid box-drawing line (matching the
# core/analysis sections), 78 wide, 2-space indent — never ASCII dashes, never a
# mix of widths. Use RULE for every table divider.
RULEW = 78
RULE = "  " + "─" * RULEW

COMPUTE_GROUPS = ["BLAS", "LAPACK", "PBLAS", "ScaLAPACK", "CBLAS", "LAPACKe", "FFTW"]

# --detail FACILITY -> the function groups it covers (post-recording analysis)
DETAIL_GROUPS = {"blas": ["BLAS", "CBLAS"],
                 "lapack": ["LAPACK", "LAPACKe", "ScaLAPACK", "PBLAS"],
                 "fftw": ["FFTW"], "mpi": ["MPI"], "io": ["IO"]}


def base_name(name):
    """Strip a trailing [shape] so one function aggregates across input sizes."""
    i = name.find("[")
    return name[:i] if i != -1 else name


def foot(pairs, width=78):
    """A CrayPAT-style legend explaining a table's columns/acronyms, preceded by
    a divider separating it from the table body."""
    out = [RULE, "  legend:"]
    for k, v in pairs:
        out.append("    %-12s %s" % (k, v))
    return "\n".join(out)


SAMP_LEGEND = [
    ("Samp%", "percent of samples here (~ percent of wall time)"),
    ("Samp", "sample count (wall time ~ Samp / sampling-Hz)"),
    ("Imb.Samp", "max-rank samples minus average across ranks (recoverable)"),
    ("Imb.Samp%", "Imb.Samp / max = (max-avg)/max"),
    ("groups", "USER=your code, ETC=other/system; MPI/BLAS/LAPACK/FFTW/IO as named"),
]


# Imbalance is a cross-rank metric — for a single-rank run it is always 0, so the
# Imb.Samp / Imb.Samp% / count[imb] columns just print "0.0%" on every row and add
# nothing. These helpers drop them when nranks <= 1 (set `imb=False`).
def samp_legend(imb):
    return SAMP_LEGEND if imb else [x for x in SAMP_LEGEND if not x[0].startswith("Imb")]


def samp_head(imb, last="Function"):
    return ("   Samp%      Samp  Imb.Samp  Imb.Samp%  " if imb else "   Samp%      Samp  ") + last


def samp_cols(samp, total, imbs, imbp, imb):
    """Leading numeric columns of a sampling row, with/without imbalance. A
    balanced row legitimately reads 0.0% (measured, not missing)."""
    p = 100.0 * samp / total if total else 0.0
    return ("%6.1f%% %9d %9.1f %8.1f%%" % (p, samp, imbs, imbp)) if imb \
        else ("%6.1f%% %9d" % (p, samp))

# ---------------------------------------------------------------- sampling ----
_etype_cache = {}

def elf_etype(path):
    """2 = ET_EXEC (absolute vaddr), 3 = ET_DYN (PIE/.so, vaddr = pc-base)."""
    if path in _etype_cache:
        return _etype_cache[path]
    et = 3
    try:
        with open(path, "rb") as f:
            hdr = f.read(18)
        if hdr[:4] == b"\x7fELF":
            et = hdr[16] | (hdr[17] << 8)
    except OSError:
        et = 0
    _etype_cache[path] = et
    return et


def map_lookup(maps, pc):
    for m in maps:
        if m["start"] <= pc < m["end"]:
            return m
    return None


def file_vaddr(m, pc):
    # ET_EXEC keeps absolute link addresses; ET_DYN is relocated by its base.
    return pc if elf_etype(m["path"]) == 2 else pc - m["start"] + m["off"]


def addr2line(path, addrs):
    """{addr -> (func, 'file:line')} via addr2line; fallback to module+offset."""
    out = {}
    base = os.path.basename(path)
    if not os.path.exists(path):
        return {a: ("%s+0x%x" % (base, a), "?") for a in addrs}
    addrs = sorted(addrs)
    try:
        p = subprocess.run(["addr2line", "-f", "-e", path] + ["0x%x" % a for a in addrs],
                           capture_output=True, text=True, timeout=120)
        lines = p.stdout.splitlines()
    except (OSError, subprocess.SubprocessError):
        lines = []
    for i, a in enumerate(addrs):
        fn = lines[2 * i] if 2 * i < len(lines) else "??"
        fl = lines[2 * i + 1] if 2 * i + 1 < len(lines) else "?"
        if fn == "??" or not fn:
            fn = "%s+0x%x" % (base, a)
        fl = fl.split()[0] if fl else ""        # drop " (discriminator N)"
        if not fl or fl.startswith("?"):
            fl = base
        else:
            path_part, _, ln = fl.rpartition(":")
            fl = "%s:%s" % (os.path.basename(path_part) if path_part else base, ln)
        out[a] = (fn, fl)
    return out


# Idle/wait functions: OpenMP worker spin, barriers, futex/lock waits, poll/sleep.
# These are "the program waiting", not useful work — split out from the ETC catch-all
# so a breakdown reads "idle 70%" (actionable) instead of "other 70%" (meaningless).
_WAIT_RE = re.compile(
    r"do_spin|futex|gomp_barrier|gomp_team_barrier|barrier_wait|gomp_.*wait|"
    r"epoll_wait|__poll|ppoll|pselect|sched_yield|nanosleep|usleep|"
    r"cond_wait|cond_timedwait|sem_wait|sem_timedwait|__lll_lock_wait")


# CrayPAT-style function groups. Classify each sample by its module/symbol.
def group_of(path, func, app_base):
    b = os.path.basename(path).lower() if path else ""
    if app_base and b == app_base.lower():
        return "USER"
    if func.startswith(("MPI_", "PMPI_", "mpi_")) or any(k in b for k in (
            "libmpi", "libmpich", "libopen-pal", "libopen-rte", "libopen-orte",
            "libpmix", "libfabric", "libucp", "libucs", "libuct")):
        return "MPI"
    if any(k in b for k in ("libopenblas", "libblas", "libmkl", "libsci",
                            "libnvpl_blas", "libblis", "libatlas", "libessl")):
        return "BLAS"
    if any(k in b for k in ("liblapack", "libnvpl_lapack", "libscalapack")):
        return "LAPACK"
    if "libfftw" in b:
        return "FFTW"
    if _WAIT_RE.search(func):
        return "WAIT"
    if "libupat" in b:
        return "ETC"        # our own overhead
    return "ETC"


# dominant-group priority: a sample is charged to the highest-priority group on its
# stack — so real work outranks idle/wait, and time blocked in MPI counts as MPI
# (not the libc poll it blocks in). WAIT outranks only ETC: a purely-idle thread
# (gomp spin, no user/lib frames) lands in WAIT, but any real frame above it wins.
GROUP_PRIO = {"MPI": 6, "BLAS": 5, "LAPACK": 5, "FFTW": 5, "IO": 4, "USER": 3,
              "WAIT": 1, "ETC": 0}

# Universal process/thread entry + runtime frames that sit at ~100% inclusive on
# every stack and are never actionable — plus two libcuda symbols that the nearest-
# symbol fallback misresolves to on NVIDIA-driver hosts (no CUDA is actually used).
# Dropped from the INCLUSIVE table only (they're never a leaf/self hotspot).
ENTRY_FRAMES = {
    "tramp", "start_thread", "clone", "clone3", "__clone", "_start",
    "__libc_start_main", "__libc_start_call_main", "call_init", "__GI___clone",
    "libprof_sample_init", "cuEGLApiInit", "cuVDPAUCtxCreate",
    # the MPI async-progress thread's poll loop — a dedicated thread that spins in
    # the library, not the app's call tree (shows ~constant inclusive %).
    "progress_engine", "opal_progress", "event_base_loop", "epoll_dispatch",
    "epoll_wait", "ompi_sync_wait_mt",
}


class Samp:
    """Symbolized sampling data with leaf (self) and, if stacks were captured,
    inclusive attribution."""
    def __init__(self):
        self.hz = 0
        self.total = 0
        self.nranks = 0
        self.stacks = False
        self.leaf = []        # per-rank Counter[(group, func, file:line)]  (self)
        self.dom = []         # per-rank Counter[group]                     (dominant)
        self.incf = []        # per-rank Counter[func]                      (inclusive)
        self.folded = collections.Counter()  # "a;b;c" -> samples


def symbolize_samples(ranks):
    s = Samp()
    s.nranks = len(ranks)
    app_base = os.path.basename(ranks[0].get("application", "")) if ranks else ""
    addrs = collections.defaultdict(set)
    raw = []   # per rank: list of (count, [(path,addr), ...]) leaf-first
    for r in ranks:
        sm = r.get("sampling")
        if not sm:
            raw.append(None); continue
        s.hz = sm.get("hz", s.hz)
        maps = sm.get("maps", [])

        def frames_of(pcs):
            fr = []
            for pc in pcs:
                m = map_lookup(maps, pc)
                if m:
                    a = file_vaddr(m, pc); addrs[m["path"]].add(a); fr.append((m["path"], a))
                else:
                    fr.append((None, pc))
            return fr

        items = []
        if sm.get("stacks"):
            s.stacks = True
            for st in sm["stacks"]:
                items.append((st["n"], frames_of(st["pc"])))
        else:
            for pc, cnt in sm.get("samples", []):
                items.append((cnt, frames_of([pc])))
        raw.append(items)

    sym = {}
    for path, aset in addrs.items():
        for a, v in addr2line(path, aset).items():
            sym[(path, a)] = v

    def info(fr):
        path, a = fr
        fn, fl = sym.get((path, a), ("0x%x" % a, "?")) if path else ("0x%x" % a, "?")
        return group_of(path, fn, app_base), fn, fl

    for items in raw:
        if items is None:
            s.leaf.append(None); s.dom.append(None); s.incf.append(None); continue
        leaf, dom, incf = collections.Counter(), collections.Counter(), collections.Counter()
        for cnt, frames in items:
            g0, f0, l0 = info(frames[0])
            leaf[(g0, f0, l0)] += cnt
            s.total += cnt
            if s.stacks:
                best, seenf, names = "ETC", set(), []
                for fr in frames:
                    g, fn, _ = info(fr)
                    if GROUP_PRIO.get(g, 0) > GROUP_PRIO.get(best, 0):
                        best = g
                    if fn not in seenf and fn not in ENTRY_FRAMES:
                        incf[(g, fn)] += cnt; seenf.add(fn)
                    names.append(fn)
                dom[best] += cnt
                s.folded[";".join(reversed(names))] += cnt
        s.leaf.append(leaf); s.dom.append(dom); s.incf.append(incf)
    return s


def symbolize_roofline(ranks, samp):
    """Per-function FLOP and DRAM-byte attribution from the roofline_sampling
    block (the characterize pass), reusing the same PC->symbol pipeline.

    The sampler emits N weighted *channels* (role=fp|mem, period, scale). Each
    PC sample contributes `period × scale` to its function: flops for an fp
    channel (scale = flops/op — 1 for an AMD op-proxy, or 1/2/4/8 for Intel's
    width-split FP_ARITH umasks), bytes for a mem channel (scale = bytes/sample,
    i.e. a cache line). Multiple fp channels are summed per function, so the
    width-weighted Intel FLOP count and the single-event AMD count come out on
    the same footing. Returns per-function {group, file, flops, bytes, fp_samp,
    self}, or None when no rank carries roofline data. Attribution is by sampled
    PC, so it works for any function (library / user / system)."""
    if not any(r.get("roofline_sampling") for r in ranks):
        return None
    app_base = os.path.basename(ranks[0].get("application", "")) if ranks else ""
    addrs = collections.defaultdict(set)
    per_rank = []   # list of [(role, period, scale, [(path,a,cnt)...]), ...] per rank
    for r in ranks:
        rf = r.get("roofline_sampling")
        if not rf:
            per_rank.append(None); continue
        maps = rf.get("maps", [])

        def collect(lst):
            out = []
            for pc, cnt in lst:
                m = map_lookup(maps, pc)
                if m:
                    a = file_vaddr(m, pc); addrs[m["path"]].add(a); out.append((m["path"], a, cnt))
                else:
                    out.append((None, pc, cnt))
            return out
        chans = []
        for ch in rf.get("channels", []):
            chans.append((ch.get("role", "fp"), ch.get("period", 0) or 0,
                          float(ch.get("scale", 0) or 0), collect(ch.get("samples", []))))
        per_rank.append(chans)

    sym = {}
    for path, aset in addrs.items():
        for a, v in addr2line(path, aset).items():
            sym[(path, a)] = v

    def info(path, a):
        fn, fl = sym.get((path, a), ("0x%x" % a, "?")) if path else ("0x%x" % a, "?")
        return group_of(path, fn, app_base), fn, fl

    funcs = {}

    def ent(path, a):
        g, fn, fl = info(path, a)
        return fn, funcs.setdefault(fn, {"group": g, "file": fl,
                                         "flops": 0.0, "bytes": 0.0, "fp_samp": 0, "self": 0})
    for chans in per_rank:
        if not chans:
            continue
        for role, period, scale, samples in chans:
            w = period * scale
            for path, a, cnt in samples:
                _, e = ent(path, a)
                if role == "mem":
                    e["bytes"] += cnt * w
                else:
                    e["flops"] += cnt * w
                    e["fp_samp"] += cnt
    # exclusive (self) time samples per function, from the time sampler's leaf data
    if samp and samp.leaf:
        for c in samp.leaf:
            if not c:
                continue
            for (g, fn, fl), cnt in c.items():
                e = funcs.setdefault(fn, {"group": g, "file": fl,
                                          "flops": 0.0, "bytes": 0.0, "fp_samp": 0, "self": 0})
                e["self"] += cnt
    return {"hz": samp.hz if samp else 0, "functions": funcs}


def load(paths):
    files = []
    for p in paths:
        if os.path.isdir(p):
            files += sorted(glob.glob(os.path.join(p, "*.json")))
        else:
            files += sorted(glob.glob(p)) if any(c in p for c in "*?[") else [p]
    ranks = []
    for fn in files:
        with open(fn) as f:
            ranks.append(json.load(f))
    if not ranks:
        sys.exit("upat-report: no input files matched")
    return ranks


def cpu_ref(ranks):
    """Per-rank-average total CPU time (summed thread utime+stime) — the denominator
    for time%. Using CPU time (not wall) keeps a function's share bounded 0-100% and
    comparable to Samp%: t_incl is summed across threads, so dividing by wall would
    overstate parallel calls and could exceed 100%. Falls back to wall runtime for
    older captures that lack cpu_time_s."""
    tot = sum(r.get("cpu_time_s", 0.0) for r in ranks)
    if tot > 0:
        return tot / len(ranks)
    return max((r.get("runtime_s", 0.0) for r in ranks), default=0.0)


def reduce_rows(ranks, imbalance, keep_shapes=False):
    nranks = len(ranks)
    # key -> per-rank accumulation. By default the [shape] suffix is stripped so
    # one function aggregates across input sizes; --detail keeps shapes.
    agg = {}
    for r in ranks:
        # sum within this rank first (so shape-stripped entries collapse to one
        # per-rank value before the cross-rank imbalance lists are built)
        per = {}
        for fn in r["functions"]:
            name = fn["function"] if keep_shapes else base_name(fn["function"])
            key = (fn["group"], name)
            d = per.setdefault(key, [0.0, 0.0, 0.0, 0])
            d[0] += fn["count"]; d[1] += fn["t_incl"]; d[2] += fn["t_excl"]; d[3] += fn["bytes"]
        for key, (c, inc, exc, by) in per.items():
            a = agg.setdefault(key, {"group": key[0], "name": key[1],
                                     "counts": [], "incl": [], "excl": [], "bytes": 0})
            a["counts"].append(c)
            a["incl"].append(inc)
            a["excl"].append(exc)
            a["bytes"] += by
    rows = []
    for a in agg.values():
        active = len(a["counts"])
        denom = nranks if imbalance == "world" else active
        # in world mode, ranks that never called it contribute 0 to min/avg
        cmin = 0 if (imbalance == "world" and active < nranks) else min(a["counts"])
        emin = 0.0 if (imbalance == "world" and active < nranks) else min(a["excl"])
        imin = 0.0 if (imbalance == "world" and active < nranks) else min(a["incl"])
        rows.append({
            "group": a["group"], "name": a["name"],
            "count": sum(a["counts"]) / denom,
            "t_incl": sum(a["incl"]) / denom,
            "t_excl": sum(a["excl"]) / denom,
            "bytes": a["bytes"],
            "imb_count": pct(cmin, max(a["counts"]), sum(a["counts"]) / denom),
            "imb_excl": pct(emin, max(a["excl"]), sum(a["excl"]) / denom),
            "active": active, "nranks": nranks,
        })
    return rows


def pct(mn, mx, avg):
    # Suite-wide imbalance = (max-avg)/max: the recoverable fraction of the slow
    # rank (CrayPAT Imb%, bounded 0-100). Matches cp_imb() and core/contract.
    return (mx - avg) / mx * 100.0 if mx > 0 else 0.0


def imb_s(p):
    return ">=999%" if p >= 999 else "%.1f%%" % p


# Tracing-table call-count column: "count[imb]" (count + imbalance%) for multi-rank
# runs, plain "count" for a single rank where imbalance is always 0.
def cnt_head(imb, w=9):
    return ("%*s %5s" % (w, "count", "[imb]")) if imb else ("%*s" % (w, "count"))


def cnt_cell(count, imb_count, imb, w=9):
    return ("%*.0f %5s" % (w, count, imb_s(imb_count))) if imb else ("%*.0f" % (w, count))


def _thr_filter(rows, runtime, thr):
    """Keep rows whose inclusive time is >= thr% of the run; return (kept, hidden)."""
    if thr <= 0 or runtime <= 0:
        return rows, 0
    keep = [r for r in rows if r["t_incl"] / runtime * 100.0 >= thr]
    return keep, len(rows) - len(keep)


def _hidden_note(hidden, thr):
    return ("   ... %d more below %.3g%% of CPU time (report --threshold 0 to show all)"
            % (hidden, thr)) if hidden else None


def fmt_compute(rows, sortkey, top, runtime=0.0, thr=0.0, imb=True):
    rows = [r for r in rows if r["group"] in COMPUTE_GROUPS]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    rows, hidden = _thr_filter(rows, runtime, thr)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    out = ["", "  Compute (BLAS / LAPACK / PBLAS / ScaLAPACK / CBLAS / LAPACKe / FFTW)",
           RULE,
           "  %6s %-8s %-21s %s %10s %10s"
           % ("time%", "group", "function", cnt_head(imb), "incl(s)", "excl(s)"),
           RULE]
    for r in rows:
        tp = 100.0 * r["t_incl"] / runtime if runtime > 0 else 0.0
        out.append("  %5.1f%% %-8s %-21s %s %10.4f %10.4f" % (
            tp, r["group"], r["name"][:21], cnt_cell(r["count"], r["imb_count"], imb),
            r["t_incl"], r["t_excl"]))
    n = _hidden_note(hidden, thr)
    if n:
        out.append(n)
    out.append(foot([
        ("time%", "inclusive time as % of total CPU time (thread-seconds) — scan to find what matters"),
        ("group", "library family: BLAS/CBLAS, LAPACK/LAPACKe, P/ScaLAPACK, FFTW"),
        ("count" + ("[imb]" if imb else ""),
         "total calls over ranks" + (" [Imb% = (max-avg)/max load imbalance]" if imb else "")),
        ("incl(s)", "inclusive time: this routine + everything it calls"),
        ("excl(s)", "exclusive time: this routine only (callees excluded)"),
        ("note", "calls aggregated over input sizes; per-shape: report --detail blas|lapack|fftw")]))
    return "\n".join(out)


def fmt_mpi(rows, sortkey, top, runtime=0.0, thr=0.0, imb=True):
    rows = [r for r in rows if r["group"] == "MPI"]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    rows, hidden = _thr_filter(rows, runtime, thr)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    total_bytes = sum(r["bytes"] for r in rows)
    out = ["", "  MPI (communication)",
           RULE,
           "  %6s %-14s %s %5s %9s %11s %7s"
           % ("time%", "function", cnt_head(imb, 8), "r/R", "incl(s)", "bytes", "GB/s"),
           RULE]
    for r in rows:
        gbs = r["bytes"] / r["t_incl"] / 1e9 if r["t_incl"] > 0 else 0.0
        tp = 100.0 * r["t_incl"] / runtime if runtime > 0 else 0.0
        out.append("  %5.1f%% %-14s %s %3d/%-3d %9.4f %11d %7.2f" % (
            tp, r["name"][:14], cnt_cell(r["count"], r["imb_count"], imb, 8),
            r["active"], r["nranks"], r["t_incl"], r["bytes"], gbs))
    out.append(RULE)
    out.append("   total communication volume: %.3f GB  (sum over ranks)" % (total_bytes / 1e9))
    n = _hidden_note(hidden, thr)
    if n:
        out.append(n)
    out.append(foot([
        ("time%", "inclusive MPI time as % of total CPU time"),
        ("count" + ("[imb]" if imb else ""),
         "total calls over ranks" + (" [Imb% = (max-avg)/max]" if imb else "")),
        ("r/R", "ranks that called it / total ranks"),
        ("incl(s)", "inclusive wall time spent in the call"),
        ("bytes", "message bytes moved (summed over ranks)"),
        ("GB/s", "bytes / inclusive-time"),
        ("note", "comm matrix + size histogram: report --detail mpi")]))
    return "\n".join(out)


MPI_BINS = ["<=64B", "<=256B", "<=1KiB", "<=4KiB", "<=16KiB", "<=64KiB",
            "<=256KiB", "<=1MiB", "<=4MiB", "<=16MiB", "<=64MiB", ">64MiB"]
COLLECTIVE = ("Bcast", "Allreduce", "Reduce", "Allgather", "Alltoall", "Gather",
              "Scatter", "Barrier", "Reduce_scatter")


def _median(xs):
    s = sorted(xs)
    return s[len(s) // 2] if s else 0


def sent_map(detail, key="sent"):
    """{peer: bytes} for a rank, accepting both the sparse [[peer,bytes],...] format
    and the legacy dense [bytes,...] array (peer = index)."""
    arr = (detail or {}).get(key, [])
    if arr and isinstance(arr[0], (list, tuple)):
        return {int(p): b for p, b in arr if b}
    return {i: b for i, b in enumerate(arr) if b}


def comm_structure(sent, rank_of):
    """Scalable communication summary for jobs too large to print an NxN matrix.
    `sent` maps src-rank -> {peer: bytes} (sparse). Reports the per-rank fan-out
    (degree) distribution — which distinguishes nearest-neighbor/halo from
    all-to-all — the per-rank volume spread, and the heaviest rank->rank pairs."""
    nr = len(rank_of)
    # Keep only the top-K heaviest pairs in a bounded min-heap so memory stays
    # O(K) even for a dense all-to-all (N^2 nonzero pairs would otherwise blow up).
    TOPK = 10
    heap, npairs = [], 0
    degree, vol = [], []
    for src in rank_of:
        m = sent.get(src, {})
        deg = len(m)
        tot = 0
        for dst, b in m.items():
            tot += b
            npairs += 1
            if len(heap) < TOPK:
                heapq.heappush(heap, (b, src, dst))
            elif b > heap[0][0]:
                heapq.heapreplace(heap, (b, src, dst))
        degree.append(deg)
        vol.append(tot)
    if not npairs:
        return ""
    avgdeg = sum(degree) / len(degree)
    maxdeg = max(degree)
    avgvol = sum(vol) / len(vol)
    maxvol = max(vol)
    # pattern: a small, near-constant fan-out is halo/stencil; near-full is all-to-all.
    if maxdeg <= max(8, 0.05 * nr):
        pattern = "sparse / nearest-neighbor (halo-like)"
    elif avgdeg >= 0.8 * (nr - 1):
        pattern = "dense / all-to-all"
    else:
        pattern = "intermediate (%.0f%% of ranks reached on average)" % (100.0 * avgdeg / max(nr - 1, 1))
    vol_imb = (maxvol - avgvol) / maxvol * 100.0 if maxvol else 0.0
    out = ["", "  Communication structure  (%d ranks; full matrix → report --detail mpi --format html)" % nr,
           "   peers per rank (fan-out): avg %.1f, median %d, max %d  → %s"
           % (avgdeg, _median(degree), maxdeg, pattern),
           "   sent volume per rank: median %.3f GB, max %.3f GB, imbalance %.0f%% (max vs avg)"
           % (_median(vol) / 1e9, maxvol / 1e9, vol_imb),
           "   heaviest rank → rank transfers:"]
    for b, s, d in sorted(heap, reverse=True):
        out.append("     rank %-6d → rank %-6d  %10.3f GB" % (s, d, b / 1e9))
    if npairs > TOPK:
        out.append("     ... (%d more nonzero pairs)" % (npairs - TOPK))
    return "\n".join(out)


def fmt_mpi_detail(ranks, rows):
    details = [r.get("mpi_detail") for r in ranks if r.get("mpi_detail")]
    if not details:
        return ""
    out = []

    # point-to-point vs collective (from traced byte totals)
    p2p = sum(r["bytes"] for r in rows if r["group"] == "MPI"
              and not any(c in r["name"] for c in COLLECTIVE))
    coll = sum(r["bytes"] for r in rows if r["group"] == "MPI"
               and any(c in r["name"] for c in COLLECTIVE))
    if p2p or coll:
        tot = p2p + coll or 1
        out += ["", "  MPI point-to-point vs collective (bytes)",
                "   point-to-point: %10.3f GB  (%4.1f%%)" % (p2p / 1e9, 100 * p2p / tot),
                "   collective:     %10.3f GB  (%4.1f%%)" % (coll / 1e9, 100 * coll / tot)]

    # message-size histogram (summed over ranks)
    binsum = [0] * len(MPI_BINS)
    for d in details:
        for i, v in enumerate(d.get("bins", [])):
            binsum[i] += v
    nmsg = sum(binsum)
    if nmsg:
        out += ["", "  MPI message-size distribution", "   %-10s %12s %7s" % ("size", "count", "%")]
        for lab, c in zip(MPI_BINS, binsum):
            if c:
                out.append("   %-10s %12d %6.1f%%" % (lab, c, 100.0 * c / nmsg))

    # communication matrix (sent bytes), rank -> {peer: bytes}  (sparse)
    nr = len(ranks)
    rank_of = [r.get("rank", i) for i, r in enumerate(ranks)]
    sent = {}
    for r in ranks:
        d = r.get("mpi_detail")
        if d:
            sent[r.get("rank", 0)] = sent_map(d, "sent")
    if any(m for m in sent.values()):
        # The full NxN matrix is unreadable (and O(N^2)) past a handful of ranks —
        # print it only for small jobs; otherwise summarize the structure. The
        # dense heatmap lives in the HTML report (report --detail mpi --format html).
        if nr <= 8:
            out += ["", "  Communication matrix  (sent MB, row=from rank, col=to rank)"]
            cols = sorted(rank_of)
            out.append("        " + "".join("%8d" % c for c in cols))
            for rk in cols:
                m = sent.get(rk, {})
                cells = "".join("%8.1f" % (m.get(c, 0) / 1e6) for c in cols)
                out.append("   %4d %s" % (rk, cells))
        else:
            # Too many ranks for an NxN grid: summarize structure instead of
            # truncating to a handful of senders.
            cs = comm_structure(sent, rank_of)
            if cs:
                out.append(cs)
    return "\n".join(out)


def fmt_io(rows, sortkey, top, runtime=0.0, thr=0.0, imb=True):
    rows = [r for r in rows if r["group"] == "IO"]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    rows, hidden = _thr_filter(rows, runtime, thr)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    total_bytes = sum(r["bytes"] for r in rows)
    out = ["", "  I/O", RULE,
           "  %6s %-12s %s %5s %9s %10s %7s"
           % ("time%", "call", cnt_head(imb, 8), "r/R", "incl(s)", "bytes", "GB/s"),
           RULE]
    for r in rows:
        gbs = r["bytes"] / r["t_incl"] / 1e9 if r["t_incl"] > 0 else 0.0
        tp = 100.0 * r["t_incl"] / runtime if runtime > 0 else 0.0
        out.append("  %5.1f%% %-12s %s %3d/%-3d %9.4f %10d %7.2f" % (
            tp, r["name"][:12], cnt_cell(r["count"], r["imb_count"], imb, 8),
            r["active"], r["nranks"], r["t_incl"], r["bytes"], gbs))
    out.append(RULE)
    out.append("   total I/O volume: %.3f GB  (sum over ranks)" % (total_bytes / 1e9))
    n = _hidden_note(hidden, thr)
    if n:
        out.append(n)
    out.append(foot([
        ("time%", "inclusive I/O time as % of total CPU time"),
        ("call", "POSIX I/O syscall (read/write/open/...)"),
        ("count" + ("[imb]" if imb else ""),
         "total calls over ranks" + (" [Imb% = (max-avg)/max]" if imb else "")),
        ("r/R", "ranks that called it / total ranks"),
        ("bytes,GB/s", "bytes transferred and bytes / inclusive-time")], 70))
    return "\n".join(out)


# CrayPAT imbalance over PEs (ranks): Imb.Samp = max - avg ; Imb% = (max-avg)/max.
def cp_imb(counts, nranks):
    tot = sum(counts)
    avg = tot / nranks if nranks else 0.0
    mx = max(counts) if counts else 0.0
    imb_samp = mx - avg
    imb_pct = (imb_samp / mx * 100.0) if mx > 0 else 0.0
    return tot, imb_samp, imb_pct


def fmt_flat(title, per_rank, total, nranks, top, labelfn, thr=0.0):
    agg = collections.defaultdict(lambda: collections.defaultdict(int))
    for pe, c in enumerate(per_rank):
        if c:
            for k, n in c.items():
                agg[k][pe] += n
    rows = [(k, cp_imb(list(pe.values()), nranks)) for k, pe in agg.items()]
    rows.sort(key=lambda x: -x[1][0])
    if thr > 0 and total > 0:
        kept = [x for x in rows if 100.0 * x[1][0] / total >= thr]
    else:
        kept = rows
    hidden = len(rows) - len(kept)
    if top:
        kept = kept[:top]
    imb = nranks > 1
    out = ["", "  " + title, RULE, samp_head(imb), RULE]
    for k, (tot, is_, ip) in kept:
        out.append("%s  %s" % (samp_cols(tot, total, is_, ip, imb), labelfn(k)[:54]))
    n = _hidden_note(hidden, thr)
    if n:
        out.append(n)
    out.append(foot(samp_legend(imb)))
    return "\n".join(out)


def fmt_domgroup(s, top):
    if not s.stacks:
        return ""
    grp = collections.defaultdict(lambda: collections.defaultdict(int))
    for pe, c in enumerate(s.dom):
        if c:
            for g, n in c.items():
                grp[g][pe] += n
    imb = s.nranks > 1
    out = ["", "Table 1:  Profile by Function Group  (sampling @ %d Hz, %d PEs)" % (s.hz, s.nranks),
           "          time charged to the highest-level group on each sample's stack", "",
           samp_head(imb, "Group"), RULE]
    pe_tot = collections.defaultdict(int)
    for g in grp:
        for pe, n in grp[g].items():
            pe_tot[pe] += n
    t = cp_imb(list(pe_tot.values()), s.nranks)
    out.append("%s  Total" % samp_cols(t[0], s.total, t[1], t[2], imb))
    out.append(RULE)
    for g in sorted(grp, key=lambda g: -sum(grp[g].values())):
        gt = cp_imb(list(grp[g].values()), s.nranks)
        out.append("%s  %s" % (samp_cols(gt[0], s.total, gt[1], gt[2], imb), g))
    out.append(foot(samp_legend(imb), 70))
    return "\n".join(out)


def fmt_groups(per_rank, hz, total, nranks, top, thr=0.0):
    """CrayPAT-style 'Profile by Function Group and Function' for sampling."""
    if total <= 0:
        return ""
    # aggregate per (group) and per (group,func); track dominant file:line.
    grp_pe = collections.defaultdict(lambda: collections.defaultdict(int))   # group->pe->count
    fn_pe = collections.defaultdict(lambda: collections.defaultdict(int))    # (g,f)->pe->count
    fn_line = collections.defaultdict(collections.Counter)                   # (g,f)->fileline->count
    for pe, c in enumerate(per_rank):
        if not c:
            continue
        for (g, f, fl), n in c.items():
            grp_pe[g][pe] += n
            fn_pe[(g, f)][pe] += n
            fn_line[(g, f)][fl] += n

    imb = nranks > 1

    def line(samp, imbs, imbp, label, indent):
        return "%s  %s%s" % (samp_cols(samp, total, imbs, imbp, imb), indent, label)

    out = ["", "Table 1:  Profile by Function Group and Function  (sampling @ %d Hz, %d PEs)"
           % (hz, nranks), "",
           samp_head(imb, "Group"),
           (" " * (41 if imb else 21)) + "Function=[file:line]",
           RULE]
    # Total row: imbalance over the per-PE grand totals.
    pe_tot = collections.defaultdict(int)
    for g in grp_pe:
        for pe, n in grp_pe[g].items():
            pe_tot[pe] += n
    t_tot, t_is, t_ip = cp_imb(list(pe_tot.values()), nranks)
    out.append(line(t_tot, t_is, t_ip, "Total", ""))
    out.append(RULE)

    groups = sorted(grp_pe, key=lambda g: sum(grp_pe[g].values()), reverse=True)
    for g in groups:
        gt, gis, gip = cp_imb(list(grp_pe[g].values()), nranks)
        out.append(line(gt, gis, gip, g, ""))
        funcs = [k for k in fn_pe if k[0] == g]
        funcs.sort(key=lambda k: sum(fn_pe[k].values()), reverse=True)
        if thr > 0:
            kept = [k for k in funcs if 100.0 * sum(fn_pe[k].values()) / total >= thr]
        else:
            kept = funcs
        hidden = len(funcs) - len(kept)
        if top:
            kept = kept[:top]
        for k in kept:
            ft, fis, fip = cp_imb(list(fn_pe[k].values()), nranks)
            fl = fn_line[k].most_common(1)[0][0]
            label = k[1] if fl in ("?", "") else "%s  [%s]" % (k[1], fl)
            out.append(line(ft, fis, fip, label[:52], "  "))
        n = _hidden_note(hidden, thr)
        if n:
            out.append(n)
        out.append(RULE)
    out.append(foot(samp_legend(imb)))
    return "\n".join(out)


def collapse_to_func(per_rank):
    """Sum leaf (group, func, file:line) samples down to (group, func) so the
    'self' table is function-level; the per-line breakdown lives in fmt_lines."""
    out = []
    for c in per_rank:
        if not c:
            out.append(c); continue
        fc = collections.Counter()
        for (g, f, _fl), n in c.items():
            fc[(g, f)] += n
        out.append(fc)
    return out


def _has_line(fl):
    """True if a symbolized location carries a real source line (file.c:123), not
    just a module/binary name or an unresolved 'file:?' (no debug line info)."""
    i = fl.rfind(":")
    return i != -1 and fl[i + 1:].isdigit()


def fmt_lines(per_rank, hz, total, nranks, top, thr=0.0):
    """CrayPAT-style line-level hotspots: every row is a single source line
    (function + file:line), ranked by self-sample %. The leaf samples are already
    resolved per (group, function, file:line), so this is just a re-ranking of the
    finest-grained key instead of the per-function collapse fmt_groups does."""
    if total <= 0:
        return ""
    line_pe = collections.defaultdict(lambda: collections.defaultdict(int))  # (g,f,fl)->pe->n
    for pe, c in enumerate(per_rank):
        if not c:
            continue
        for k, n in c.items():
            line_pe[k][pe] += n
    imb = nranks > 1
    rows = [(k, cp_imb(list(pe.values()), nranks)) for k, pe in line_pe.items()]
    rows.sort(key=lambda x: -x[1][0])
    kept = [x for x in rows if 100.0 * x[1][0] / total >= thr] if thr > 0 else rows
    hidden = len(rows) - len(kept)
    if top:
        kept = kept[:top]
    w = 36 if imb else 58           # imbalance columns eat into the label width
    out = ["", "Table 1b:  Profile by source line  (hotspots, sampling @ %d Hz, %d PEs)"
           % (hz, nranks),
           "           every row is one source line, ranked by self-sample %", "",
           samp_head(imb, "Function  [file:line]  (group)"), RULE]
    # Track hotspots with no source-line info (built without -g, or stripped) so we
    # can tell the user how to enable it. Unmappable regions (fl '?'/'') are a
    # missing-map issue, not a debug-info one — exclude them.
    noline = collections.Counter()
    noline_user = set()
    for (g, f, fl), (tot, is_, ip) in kept:
        loc = f if fl in ("?", "") else "%s  [%s]" % (f, fl)
        out.append("%s  %s" % (samp_cols(tot, total, is_, ip, imb),
                               ("%s  (%s)" % (loc, g))[:w]))
        if fl not in ("?", "") and not _has_line(fl):
            mod = fl[:-2] if fl.endswith(":?") else fl   # 'n1bv_32.c:?' -> 'n1bv_32.c'
            noline[mod] += tot
            if g == "USER":
                noline_user.add(mod)
    n = _hidden_note(hidden, thr)
    if n:
        out.append(n)
    noline_samp = sum(noline.values())
    if noline_samp and total and 100.0 * noline_samp / total >= 5.0:
        mods = ", ".join(m for m, _ in noline.most_common(6))
        out.append("")
        out.append("   ⚠ no source-line info for %.0f%% of the hotspots above — file:line shows a"
                   % (100.0 * noline_samp / total))
        out.append("     module/binary name (or 'file:?'); those objects were built without -g")
        out.append("     or were stripped, so line-level hotspots are unavailable for them.")
        if noline_user:
            out.append("     this includes your own application code — rebuild it first.")
        out.append("     affected: " + mods)
        out.append("     → keep line tables when building (optimization is unaffected): cc -O2 -g")
        out.append("       and do not 'strip'. For distro libraries, install the matching")
        out.append("       -dbg / -debuginfo package (e.g. libfftw3-dbg, debuginfo-install <pkg>).")
    out.append(foot(samp_legend(imb)))
    return "\n".join(out)


def line_hotspots_json(leaf, gtotal, nranks, top=200):
    """Ranked line-level hotspots for the JSON contract (same data fmt_lines
    renders), so the suite/HTML report can surface source-line hotspots too."""
    line_pe = collections.defaultdict(lambda: collections.defaultdict(int))
    for pe, c in enumerate(leaf):
        if c:
            for k, n in c.items():
                line_pe[k][pe] += n
    hot = []
    for (g, f, fl), pe in line_pe.items():
        tot, is_, ip = cp_imb(list(pe.values()), nranks)
        hot.append({"group": g, "function": f, "fileline": fl, "samples": tot,
                    "pct": 100.0 * tot / gtotal if gtotal else 0.0,
                    "imb_samp": is_, "imb_pct": ip})
    hot.sort(key=lambda x: -x["samples"])
    return hot[:top] if top else hot


def fmt_detail(rows, groups, title, sortkey, top, runtime=0.0):
    """Per-shape/size breakdown for one facility (post-recording detail)."""
    rows = [r for r in rows if r["group"] in groups]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    if top:
        rows = rows[:top]
    if not rows:
        return "  (no %s calls recorded)" % title
    out = ["", "  %s calls by shape/size" % title, RULE,
           "  %6s %-8s %-30s %9s %10s %10s"
           % ("time%", "group", "function[shape]", "count", "incl(s)", "excl(s)"),
           RULE]
    for r in rows:
        tp = 100.0 * r["t_incl"] / runtime if runtime > 0 else 0.0
        out.append("  %5.1f%% %-8s %-30s %9.0f %10.4f %10.4f" % (
            tp, r["group"], r["name"][:30], r["count"], r["t_incl"], r["t_excl"]))
    out.append(foot([
        ("time%", "inclusive time as % of total CPU time"),
        ("[shape]", "call dimensions, e.g. gemm[m,n,k], fft[nx x ny x nz]"),
        ("count", "calls of this exact shape (summed over ranks)"),
        ("incl(s)", "inclusive time: routine + callees"),
        ("excl(s)", "exclusive time: routine only")]))
    return "\n".join(out)


def fmt_heap(ranks):
    hs = [(r.get("rank", i), r["heap"]) for i, r in enumerate(ranks) if r.get("heap")]
    if not hs:
        return ""
    peaks = [h["peak"] for _, h in hs]
    out = ["", "Heap high-water mark", RULE,
           "   peak (max over ranks): %.3f GB   mean: %.3f GB" % (
               max(peaks) / 1e9, sum(peaks) / len(peaks) / 1e9)]
    worst = max(hs, key=lambda x: x[1]["peak"])
    out.append("   highest on rank %d: %.3f GB   total allocations: %d" % (
        worst[0], worst[1]["peak"] / 1e9, sum(h["allocs"] for _, h in hs)))
    leaked = sum(h["live_at_exit"] for _, h in hs)
    if leaked > 0:
        out.append("   live at exit (possible leak): %.3f GB" % (leaked / 1e9))
    return "\n".join(out)


def fmt_perpe(ranks):
    if len(ranks) < 2:
        return ""
    out = ["", "Per-PE summary", RULE,
           "   %-6s %12s %12s %8s" % ("PE", "runtime(s)", "lib excl(s)", "lib%")]
    info = []
    for r in ranks:
        rt = r.get("runtime_s", 0.0)
        lib = sum(f["t_excl"] for f in r.get("functions", []))
        info.append((r.get("rank", 0), rt, lib))
    for pe, rt, lib in sorted(info):
        out.append("   %-6d %12.3f %12.3f %7.1f%%" % (pe, rt, lib, 100 * lib / rt if rt else 0))
    rts = [x[1] for x in info]
    slow = max(info, key=lambda x: x[1])
    out.append(RULE)
    out.append("   slowest PE %d (%.3fs); mean %.3fs; spread %.1f%%" % (
        slow[0], slow[1], sum(rts) / len(rts),
        (max(rts) - min(rts)) / max(rts) * 100 if max(rts) else 0))
    return "\n".join(out)


def observations(rows, ranks, hz, total, per_rank):
    obs = []
    nr = len(ranks)
    runtime = max((r.get("runtime_s", 0.0) for r in ranks), default=0.0)

    # load imbalance among traced calls that cost real time
    for r in sorted(rows, key=lambda r: r["t_excl"], reverse=True)[:8]:
        if r["t_excl"] > 0.01 and r["imb_excl"] >= 30 and nr > 1:
            obs.append("Load imbalance: %s is %.0f%% imbalanced across ranks "
                       "(%.3fs avg excl) — check work distribution." %
                       (r["name"], r["imb_excl"], r["t_excl"]))

    # MPI communication fraction (from traced MPI inclusive time)
    mpi_t = sum(r["t_incl"] for r in rows if r["group"] == "MPI")
    if mpi_t > 0 and runtime > 0 and mpi_t / runtime > 0.15:
        obs.append("MPI is %.0f%% of runtime — communication-bound; consider "
                   "larger messages, overlap, or fewer collectives." % (mpi_t / runtime * 100))

    # slow rank (placement / imbalance hint)
    if nr > 1:
        rts = [(r.get("rank", 0), r.get("runtime_s", 0.0)) for r in ranks]
        mean = sum(t for _, t in rts) / nr
        srank, smax = max(rts, key=lambda x: x[1])
        if mean > 0 and smax > 1.15 * mean:
            obs.append("Rank %d runtime is %.0f%% above the mean — investigate "
                       "rank placement / affinity or input imbalance." %
                       (srank, (smax / mean - 1) * 100))

    if not obs:
        obs.append("No significant imbalance or communication bottleneck detected.")
    return "\nObservations\n" + RULE + "\n" + "\n".join(" * " + o for o in obs)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--imbalance", choices=["active", "world"], default="active")
    ap.add_argument("--format", choices=["table", "json", "csv"], default="table")
    ap.add_argument("--sort", choices=["t_incl", "t_excl", "count"], default="t_incl",
                    help="tracing-table sort key (default t_incl, matching the time%% column)")
    ap.add_argument("--top", type=int, default=0)
    ap.add_argument("--threshold", type=float, default=0.1,
                    help="hide functions below this %% of runtime (default 0.1; 0 = show all)")
    ap.add_argument("--folded", action="store_true",
                    help="print folded call stacks (for flamegraph.pl) and exit")
    ap.add_argument("--no-observations", action="store_true",
                    help="suppress the Observations section (suite supplies unified insights)")
    ap.add_argument("--no-header", action="store_true",
                    help="suppress the banner (the suite prints its own UPAT banner)")
    ap.add_argument("--detail", choices=list(DETAIL_GROUPS),
                    help="per-facility detail (per-shape calls / MPI comm matrix); "
                         "otherwise calls aggregate over input sizes")
    args = ap.parse_args()

    ranks = load(args.files)

    if args.folded:
        s = symbolize_samples(ranks)
        for stack, n in s.folded.most_common():
            print("%s %d" % (stack, n))
        return

    if args.detail:
        app = ranks[0].get("application", "")
        det = reduce_rows(ranks, args.imbalance, keep_shapes=True)
        runtime = cpu_ref(ranks)               # time% denominator (total CPU time)
        print("\n" + "=" * 78)
        print("  UPAT detail analysis — %s" % args.detail.upper())
        print("=" * 78)
        print(" application: %s   ranks: %d" % (app, len(ranks)))
        if args.detail == "mpi":
            mt, md = fmt_mpi(det, args.sort, args.top, runtime), fmt_mpi_detail(ranks, det)
            if mt or md:
                print(mt)
                print(md)
            else:
                print("\n  (no MPI calls traced by name. upat traces the C MPI ABI;")
                print("   Fortran codes calling the mpi_*_ bindings are captured by")
                print("   sampling only — see the function-group table in --collector upat.)")
        elif args.detail == "io":
            print(fmt_io(det, args.sort, args.top, runtime))
        else:
            print(fmt_detail(det, DETAIL_GROUPS[args.detail], args.detail.upper(),
                             args.sort, args.top, runtime))
        print()
        return

    rows = reduce_rows(ranks, args.imbalance)
    runtime = max(r.get("runtime_s", 0.0) for r in ranks)
    cpu = cpu_ref(ranks)                       # time% denominator (total CPU time)
    app = ranks[0].get("application", "")

    if args.format == "json":
        # sampling dominant-group totals (accurate time-by-group: charges e.g.
        # read()-under-MPI to MPI via the stack, unlike tracing t_excl).
        groups = {}
        s = symbolize_samples(ranks)
        if s.stacks and s.total:
            for c in s.dom:
                if c:
                    for g, n in c.items():
                        groups[g] = groups.get(g, 0) + n
        out = {"version": 1, "application": app, "runtime_s": runtime,
               "cpu_time_s": cpu_ref(ranks),    # per-rank-avg CPU time = time% denominator
               "nranks": len(ranks), "functions": rows,
               "groups": groups, "group_total": s.total,
               "line_hotspots": line_hotspots_json(s.leaf, s.total, len(ranks))}
        rf = symbolize_roofline(ranks, s)
        if rf:
            out["roofline_functions"] = rf
        json.dump(out, sys.stdout, indent=2)
        print()
        return
    if args.format == "csv":
        print("group,function,count,t_incl,t_excl,bytes,gbytes_s,imb_count_pct,imb_excl_pct,nranks_active,nranks")
        for r in sorted(rows, key=lambda r: r[args.sort], reverse=True):
            gbs = r["bytes"] / r["t_incl"] / 1e9 if r["t_incl"] > 0 else 0.0
            print("%s,%s,%.0f,%.6f,%.6f,%d,%.4f,%.2f,%.2f,%d,%d" % (
                r["group"], r["name"], r["count"], r["t_incl"], r["t_excl"], r["bytes"],
                gbs, r["imb_count"], r["imb_excl"], r["active"], r["nranks"]))
        return

    if not args.no_header:    # suppressed in the suite, which prints its own Environment header
        print("\n" + "=" * 78)
        print("  UPAT  —  Universal Performance Analysis Tool  (deep profile)")
        print("         tracing (sci-libs / MPI / I/O) + statistical sampling")
        print("=" * 78)
        print(" application: %s" % app)
        print(" ranks: %d   max runtime (s): %.3f   imbalance: %s   imb = (max-avg)/max" %
              (len(ranks), runtime, args.imbalance))

    # Table 1 — sampling, grouped by function group (CrayPAT-style).
    s = symbolize_samples(ranks)
    hz, total, per_rank = s.hz, s.total, s.leaf
    if total > 0:
        if s.stacks:
            cap = args.top or 15        # the call-tree lists are long; cap unless --top given
            print(fmt_domgroup(s, args.top))
            print(fmt_flat("Top functions (inclusive)", s.incf, total, s.nranks, cap,
                           lambda k: "%s  [%s]" % (k[1], k[0]), args.threshold))
            print(fmt_flat("Top functions (self)", collapse_to_func(s.leaf), total,
                           s.nranks, cap, lambda k: "%s  (%s)" % (k[1], k[0]),
                           args.threshold))
        else:
            print(fmt_groups(s.leaf, hz, total, s.nranks, args.top, args.threshold))
        # CrayPAT-style line-level hotspot ranking (self time), a separate section
        # from the function-level tables above. Works in both leaf and stack modes.
        print(fmt_lines(s.leaf, hz, total, s.nranks, args.top, args.threshold))

    # Tables 2-3 — exact library tracing (counts/time/imbalance, MPI volume).
    # Imbalance columns only make sense across ranks; drop them for a single rank.
    thr = args.threshold
    imb = len(ranks) > 1
    ct = fmt_compute(rows, args.sort, args.top, cpu, thr, imb)
    if ct:
        print("\nTable 2:  Library calls by group and function  (tracing)")
        print(ct)
    mt = fmt_mpi(rows, args.sort, args.top, cpu, thr, imb)
    if mt:
        print("\nTable 3:  MPI message statistics  (tracing)")
        print(mt)
        print(fmt_mpi_detail(ranks, rows))
    iot = fmt_io(rows, args.sort, args.top, cpu, thr, imb)
    if iot:
        print("\nTable 4:  I/O statistics  (tracing)")
        print(iot)
    print(fmt_heap(ranks))
    print(fmt_perpe(ranks))
    if not args.no_observations:
        print(observations(rows, ranks, hz, total, per_rank))
    print()


if __name__ == "__main__":
    main()
