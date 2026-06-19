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


def symbolize_samples(ranks):
    """Return (hz, total, nranks, per_rank) where per_rank[i] is a Counter keyed
    by (group, func, file:line)."""
    hz = 0
    app_base = os.path.basename(ranks[0].get("application", "")) if ranks else ""
    addrs_by_path = collections.defaultdict(set)
    rank_keys = []
    for r in ranks:
        s = r.get("sampling")
        if not s or not s.get("samples"):
            rank_keys.append(None)
            continue
        hz = s.get("hz", hz)
        maps = s.get("maps", [])
        keys = []
        for pc, cnt in s["samples"]:
            m = map_lookup(maps, pc)
            if m:
                a = file_vaddr(m, pc)
                addrs_by_path[m["path"]].add(a)
                keys.append((m["path"], a, cnt))
            else:
                keys.append((None, pc, cnt))
        rank_keys.append(keys)

    sym = {}
    for path, aset in addrs_by_path.items():
        for a, v in addr2line(path, aset).items():
            sym[(path, a)] = v

    per_rank = []
    total = 0
    for keys in rank_keys:
        if keys is None:
            per_rank.append(None); continue
        c = collections.Counter()
        for path, a, cnt in keys:
            if path is None:
                fn, fl = ("0x%x" % a, "?")
            else:
                fn, fl = sym.get((path, a), ("0x%x" % a, "?"))
            c[(group_of(path, fn, app_base), fn, fl)] += cnt
            total += cnt
        per_rank.append(c)
    return hz, total, len(ranks), per_rank


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
    return (mx - mn) / avg * 100.0 if avg > 0 else 0.0


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


# CrayPAT imbalance over PEs (ranks): Imb.Samp = max - avg ; Imb% = (max-avg)/max.
def cp_imb(counts, nranks):
    tot = sum(counts)
    avg = tot / nranks if nranks else 0.0
    mx = max(counts) if counts else 0.0
    imb_samp = mx - avg
    imb_pct = (imb_samp / mx * 100.0) if mx > 0 else 0.0
    return tot, imb_samp, imb_pct


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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--imbalance", choices=["active", "world"], default="active")
    ap.add_argument("--format", choices=["table", "json", "csv"], default="table")
    ap.add_argument("--sort", choices=["t_incl", "t_excl", "count"], default="t_excl")
    ap.add_argument("--top", type=int, default=0)
    args = ap.parse_args()

    ranks = load(args.files)
    rows = reduce_rows(ranks, args.imbalance)
    runtime = max(r.get("runtime_s", 0.0) for r in ranks)
    app = ranks[0].get("application", "")

    if args.format == "json":
        json.dump({"version": 1, "application": app, "runtime_s": runtime,
                   "nranks": len(ranks), "functions": rows}, sys.stdout, indent=2)
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
    print(" ranks: %d   max runtime (s): %.3f   imbalance: %s   imb = (max-min)/avg" %
          (len(ranks), runtime, args.imbalance))

    # Table 1 — sampling, grouped by function group (CrayPAT-style).
    hz, total, nranks, per_rank = symbolize_samples(ranks)
    if total > 0:
        print(fmt_groups(per_rank, hz, total, nranks, args.top))

    # Tables 2-3 — exact library tracing (counts/time/imbalance, MPI volume).
    ct = fmt_compute(rows, args.sort, args.top)
    if ct:
        print("\nTable 2:  Library calls by group and function  (tracing)")
        print(ct)
    mt = fmt_mpi(rows, args.sort, args.top)
    if mt:
        print("\nTable 3:  MPI message statistics  (tracing)")
        print(mt)
    print()


if __name__ == "__main__":
    main()
