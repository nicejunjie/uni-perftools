#!/usr/bin/env python3
"""scilib-report - postprocess raw scilib-prof per-rank JSON into a report.

The profiled run writes one scilib-prof.<rank>.json per process (no analysis).
This tool reduces across ranks and prints a COMPUTE table (BLAS/LAPACK/...) and a
separate MPI table (with communication volume / GB/s), plus load imbalance.

Usage:
  scilib-report scilib-prof.*.json
  scilib-report out/                       # a directory of json files
  scilib-report --imbalance world --format csv scilib-prof.*.json
"""
import sys, os, json, glob, argparse, subprocess, collections

COMPUTE_GROUPS = ["BLAS", "LAPACK", "PBLAS", "ScaLAPACK", "CBLAS", "LAPACKe", "FFTW"]

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
    if "scilibprof" in b:
        return "ETC"        # our own overhead
    return "ETC"


# dominant-group priority: a sample is charged to the highest-priority group on
# its stack, so time waiting in MPI counts as MPI (not the libc poll it blocks in).
GROUP_PRIO = {"MPI": 6, "BLAS": 5, "LAPACK": 5, "FFTW": 5, "IO": 4, "USER": 3, "ETC": 0}


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
                    if fn not in seenf:
                        incf[(g, fn)] += cnt; seenf.add(fn)
                    names.append(fn)
                dom[best] += cnt
                s.folded[";".join(reversed(names))] += cnt
        s.leaf.append(leaf); s.dom.append(dom); s.incf.append(incf)
    return s


def symbolize_roofline(ranks, samp):
    """Per-function FP-op and DRAM-fill attribution from the roofline_sampling
    block (the characterize pass), reusing the same PC->symbol pipeline. Returns
    per-function {group, file, fp, mem, self} sample counts plus the sample
    periods, or None when no rank carries roofline data. Works for any function
    (library / user / system) because attribution is by sampled PC."""
    if not any(r.get("roofline_sampling") for r in ranks):
        return None
    app_base = os.path.basename(ranks[0].get("application", "")) if ranks else ""
    addrs = collections.defaultdict(set)
    per_rank = []
    fp_period = mem_period = bpf = 0
    for r in ranks:
        rf = r.get("roofline_sampling")
        if not rf:
            per_rank.append(None); continue
        fp_period = rf.get("fp_period", fp_period)
        mem_period = rf.get("mem_period", mem_period)
        bpf = rf.get("bytes_per_fill", bpf) or 64
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
        per_rank.append((collect(rf.get("fp_samples", [])), collect(rf.get("mem_samples", []))))

    sym = {}
    for path, aset in addrs.items():
        for a, v in addr2line(path, aset).items():
            sym[(path, a)] = v

    def info(path, a):
        fn, fl = sym.get((path, a), ("0x%x" % a, "?")) if path else ("0x%x" % a, "?")
        return group_of(path, fn, app_base), fn, fl

    funcs = {}

    def add(path, a, cnt, key):
        g, fn, fl = info(path, a)
        e = funcs.setdefault(fn, {"group": g, "file": fl, "fp": 0, "mem": 0, "self": 0})
        e[key] += cnt
    for pr in per_rank:
        if not pr:
            continue
        fpl, meml = pr
        for path, a, cnt in fpl: add(path, a, cnt, "fp")
        for path, a, cnt in meml: add(path, a, cnt, "mem")
    # exclusive (self) time samples per function, from the time sampler's leaf data
    if samp and samp.leaf:
        for c in samp.leaf:
            if not c:
                continue
            for (g, fn, fl), cnt in c.items():
                e = funcs.setdefault(fn, {"group": g, "file": fl, "fp": 0, "mem": 0, "self": 0})
                e["self"] += cnt
    return {"fp_period": fp_period, "mem_period": mem_period, "bytes_per_fill": bpf,
            "hz": samp.hz if samp else 0, "functions": funcs}


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
        sys.exit("scilib-report: no input files matched")
    return ranks


def reduce_rows(ranks, imbalance):
    nranks = len(ranks)
    # key -> per-rank accumulation
    agg = {}
    for r in ranks:
        for fn in r["functions"]:
            key = (fn["group"], fn["function"])
            a = agg.setdefault(key, {"group": fn["group"], "name": fn["function"],
                                     "counts": [], "incl": [], "excl": [], "bytes": 0})
            a["counts"].append(fn["count"])
            a["incl"].append(fn["t_incl"])
            a["excl"].append(fn["t_excl"])
            a["bytes"] += fn["bytes"]
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


def fmt_compute(rows, sortkey, top):
    rows = [r for r in rows if r["group"] in COMPUTE_GROUPS]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    out = ["", "  Compute (BLAS / LAPACK / PBLAS / ScaLAPACK / CBLAS / LAPACKe / FFTW)",
           "  " + "-" * 78,
           "   %-9s %-24s %12s %11s %11s" % ("group", "function", "count[imb]", "incl(s)", "excl(s)"),
           "  " + "-" * 78]
    for r in rows:
        out.append("   %-9s %-24s %9.0f %5s %11.4f %11.4f" % (
            r["group"], r["name"][:24], r["count"], imb_s(r["imb_count"]),
            r["t_incl"], r["t_excl"]))
    return "\n".join(out)


def fmt_mpi(rows, sortkey, top):
    rows = [r for r in rows if r["group"] == "MPI"]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    total_bytes = sum(r["bytes"] for r in rows)
    out = ["", "  MPI (communication)",
           "  " + "-" * 78,
           "   %-18s %11s %5s %10s %12s %8s" % ("function", "count[imb]", "r/R", "incl(s)", "bytes", "GB/s"),
           "  " + "-" * 78]
    for r in rows:
        gbs = r["bytes"] / r["t_incl"] / 1e9 if r["t_incl"] > 0 else 0.0
        out.append("   %-18s %8.0f %5s %3d/%-3d %10.4f %12d %8.2f" % (
            r["name"][:18], r["count"], imb_s(r["imb_count"]),
            r["active"], r["nranks"], r["t_incl"], r["bytes"], gbs))
    out.append("  " + "-" * 78)
    out.append("   total communication volume: %.3f GB  (sum over ranks)" % (total_bytes / 1e9))
    return "\n".join(out)


MPI_BINS = ["<=64B", "<=256B", "<=1KiB", "<=4KiB", "<=16KiB", "<=64KiB",
            "<=256KiB", "<=1MiB", "<=4MiB", "<=16MiB", "<=64MiB", ">64MiB"]
COLLECTIVE = ("Bcast", "Allreduce", "Reduce", "Allgather", "Alltoall", "Gather",
              "Scatter", "Barrier", "Reduce_scatter")


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

    # communication matrix (sent bytes, MB), rank -> peer
    nr = len(ranks)
    rank_of = [r.get("rank", i) for i, r in enumerate(ranks)]
    sent = {}
    for r in ranks:
        d = r.get("mpi_detail")
        if d:
            sent[r.get("rank", 0)] = d.get("sent", [])
    if any(sum(v) for v in sent.values()):
        out += ["", "  Communication matrix  (sent MB, row=from rank, col=to rank)"]
        if nr <= 16:
            hdr = "        " + "".join("%8d" % c for c in sorted(rank_of))
            out.append(hdr)
            for rk in sorted(rank_of):
                v = sent.get(rk, [])
                cells = "".join("%8.1f" % (v[c] / 1e6 if c < len(v) else 0.0) for c in sorted(rank_of))
                out.append("   %4d %s" % (rk, cells))
        else:
            out.append("   (%d ranks — too large to print; per-rank sent totals:)" % nr)
            for rk in sorted(rank_of):
                out.append("   rank %4d sent %10.3f GB" % (rk, sum(sent.get(rk, [])) / 1e9))
    return "\n".join(out)


def fmt_io(rows, sortkey, top):
    rows = [r for r in rows if r["group"] == "IO"]
    rows.sort(key=lambda r: r[sortkey], reverse=True)
    if top:
        rows = rows[:top]
    if not rows:
        return ""
    total_bytes = sum(r["bytes"] for r in rows)
    out = ["", "  I/O", "  " + "-" * 70,
           "   %-14s %11s %5s %10s %12s %8s" % ("call", "count[imb]", "r/R", "incl(s)", "bytes", "GB/s"),
           "  " + "-" * 70]
    for r in rows:
        gbs = r["bytes"] / r["t_incl"] / 1e9 if r["t_incl"] > 0 else 0.0
        out.append("   %-14s %8.0f %5s %3d/%-3d %10.4f %12d %8.2f" % (
            r["name"][:14], r["count"], imb_s(r["imb_count"]),
            r["active"], r["nranks"], r["t_incl"], r["bytes"], gbs))
    out.append("  " + "-" * 70)
    out.append("   total I/O volume: %.3f GB  (sum over ranks)" % (total_bytes / 1e9))
    return "\n".join(out)


# CrayPAT imbalance over PEs (ranks): Imb.Samp = max - avg ; Imb% = (max-avg)/max.
def cp_imb(counts, nranks):
    tot = sum(counts)
    avg = tot / nranks if nranks else 0.0
    mx = max(counts) if counts else 0.0
    imb_samp = mx - avg
    imb_pct = (imb_samp / mx * 100.0) if mx > 0 else 0.0
    return tot, imb_samp, imb_pct


def fmt_flat(title, per_rank, total, nranks, top, labelfn):
    agg = collections.defaultdict(lambda: collections.defaultdict(int))
    for pe, c in enumerate(per_rank):
        if c:
            for k, n in c.items():
                agg[k][pe] += n
    rows = [(k, cp_imb(list(pe.values()), nranks)) for k, pe in agg.items()]
    rows.sort(key=lambda x: -x[1][0])
    out = ["", "  " + title, "  " + "-" * 78,
           "   Samp%      Samp  Imb.Samp  Imb.Samp%  Function",
           " " + "-" * 78]
    for k, (tot, is_, ip) in rows[:top or 12]:
        out.append("%6.1f%% %9d %9.1f %8.1f%%  %s" % (100.0 * tot / total, tot, is_, ip, labelfn(k)[:54]))
    return "\n".join(out)


def fmt_domgroup(s, top):
    if not s.stacks:
        return ""
    grp = collections.defaultdict(lambda: collections.defaultdict(int))
    for pe, c in enumerate(s.dom):
        if c:
            for g, n in c.items():
                grp[g][pe] += n
    out = ["", "Table 1:  Profile by Function Group  (sampling @ %d Hz, %d PEs)" % (s.hz, s.nranks),
           "          time charged to the highest-level group on each sample's stack", "",
           "   Samp%      Samp  Imb.Samp  Imb.Samp%  Group", " " + "-" * 70]
    pe_tot = collections.defaultdict(int)
    for g in grp:
        for pe, n in grp[g].items():
            pe_tot[pe] += n
    t = cp_imb(list(pe_tot.values()), s.nranks)
    out.append("%6.1f%% %9d %9.1f %8.1f%%  Total" % (100.0, t[0], t[1], t[2]))
    out.append(" " + "-" * 70)
    for g in sorted(grp, key=lambda g: -sum(grp[g].values())):
        gt = cp_imb(list(grp[g].values()), s.nranks)
        out.append("%6.1f%% %9d %9.1f %8.1f%%  %s" % (100.0 * gt[0] / s.total, gt[0], gt[1], gt[2], g))
    return "\n".join(out)


def fmt_groups(per_rank, hz, total, nranks, top):
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

    def line(samp, imbs, imbp, label, indent):
        tn, te = ("Total", "")
        return "%6.1f%% %9d %9.1f %8.1f%%  %s%s" % (
            100.0 * samp / total, samp, imbs, imbp, indent, label)

    out = ["", "Table 1:  Profile by Function Group and Function  (sampling @ %d Hz, %d PEs)"
           % (hz, nranks), "",
           "   Samp%      Samp  Imb.Samp  Imb.Samp%  Group",
           "                                         Function=[file:line]",
           " " + "-" * 78]
    # Total row: imbalance over the per-PE grand totals.
    pe_tot = collections.defaultdict(int)
    for g in grp_pe:
        for pe, n in grp_pe[g].items():
            pe_tot[pe] += n
    t_tot, t_is, t_ip = cp_imb(list(pe_tot.values()), nranks)
    out.append(line(t_tot, t_is, t_ip, "Total", ""))
    out.append(" " + "-" * 78)

    groups = sorted(grp_pe, key=lambda g: sum(grp_pe[g].values()), reverse=True)
    for g in groups:
        gt, gis, gip = cp_imb(list(grp_pe[g].values()), nranks)
        out.append(line(gt, gis, gip, g, ""))
        funcs = [k for k in fn_pe if k[0] == g]
        funcs.sort(key=lambda k: sum(fn_pe[k].values()), reverse=True)
        for k in funcs[:top or 8]:
            ft, fis, fip = cp_imb(list(fn_pe[k].values()), nranks)
            fl = fn_line[k].most_common(1)[0][0]
            label = k[1] if fl in ("?", "") else "%s  [%s]" % (k[1], fl)
            out.append(line(ft, fis, fip, label[:52], "  "))
        out.append(" " + "-" * 78)
    return "\n".join(out)


def fmt_heap(ranks):
    hs = [(r.get("rank", i), r["heap"]) for i, r in enumerate(ranks) if r.get("heap")]
    if not hs:
        return ""
    peaks = [h["peak"] for _, h in hs]
    out = ["", "Heap high-water mark", " " + "-" * 60,
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
    out = ["", "Per-PE summary", " " + "-" * 60,
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
    out.append(" " + "-" * 60)
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
    return "\nObservations\n " + "-" * 60 + "\n" + "\n".join(" * " + o for o in obs)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--imbalance", choices=["active", "world"], default="active")
    ap.add_argument("--format", choices=["table", "json", "csv"], default="table")
    ap.add_argument("--sort", choices=["t_incl", "t_excl", "count"], default="t_excl")
    ap.add_argument("--top", type=int, default=0)
    ap.add_argument("--folded", action="store_true",
                    help="print folded call stacks (for flamegraph.pl) and exit")
    ap.add_argument("--no-observations", action="store_true",
                    help="suppress the Observations section (suite supplies unified insights)")
    args = ap.parse_args()

    ranks = load(args.files)

    if args.folded:
        s = symbolize_samples(ranks)
        for stack, n in s.folded.most_common():
            print("%s %d" % (stack, n))
        return

    rows = reduce_rows(ranks, args.imbalance)
    runtime = max(r.get("runtime_s", 0.0) for r in ranks)
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
               "nranks": len(ranks), "functions": rows,
               "groups": groups, "group_total": s.total}
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

    print("\n" + "=" * 80)
    print("                      Scientific Library Profiler")
    print("=" * 80)
    print(" application: %s" % app)
    print(" ranks: %d   max runtime (s): %.3f   imbalance: %s   imb = (max-avg)/max" %
          (len(ranks), runtime, args.imbalance))

    # Table 1 — sampling, grouped by function group (CrayPAT-style).
    s = symbolize_samples(ranks)
    hz, total, per_rank = s.hz, s.total, s.leaf
    if total > 0:
        if s.stacks:
            print(fmt_domgroup(s, args.top))
            print(fmt_flat("Top functions (inclusive)", s.incf, total, s.nranks, args.top,
                           lambda k: "%s  [%s]" % (k[1], k[0])))
            print(fmt_flat("Top functions (self)", s.leaf, total, s.nranks, args.top,
                           lambda k: "%s  %s" % (k[1], k[2])))
        else:
            print(fmt_groups(s.leaf, hz, total, s.nranks, args.top))

    # Tables 2-3 — exact library tracing (counts/time/imbalance, MPI volume).
    ct = fmt_compute(rows, args.sort, args.top)
    if ct:
        print("\nTable 2:  Library calls by group and function  (tracing)")
        print(ct)
    mt = fmt_mpi(rows, args.sort, args.top)
    if mt:
        print("\nTable 3:  MPI message statistics  (tracing)")
        print(mt)
        print(fmt_mpi_detail(ranks, rows))
    iot = fmt_io(rows, args.sort, args.top)
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
