"""Analysis viewpoints — recipes over one result dir. Each appends lines to `out`.
Everything here is derived from data the collectors already emit (no collector
changes): snapshot counters (snap.json) + profile attribution (prof.*.json).
"""
import os
import re
import sys
import json
import math
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "contract"))
sys.path.insert(0, os.path.join(os.path.dirname(_HERE), "roofline"))
import contract   # noqa: E402
import roofline   # noqa: E402


# Plain-language definitions for the jargon/acronyms the reports use — surfaced as
# `#` comment lines in the text report and as mouse-over tooltips in HTML (Intel
# APS style). Shared by viewpoints (text) and htmlrep (HTML) so the two never drift.
GLOSSARY = {
    "retiring": "pipeline slots doing useful work — instructions that completed and committed",
    "frontend-bound": "slots stalled fetching/decoding instructions (instruction-cache, branch, decode)",
    "backend-bound": "slots stalled in execution — waiting on data from cache/DRAM or on busy execution units",
    "bad speculation": "slots wasted on instructions from mispredicted branches (work later thrown away)",
    "SMT contention": "slots lost to the sibling hyperthread sharing this physical core (SMT = simultaneous multithreading)",
    "branch mispredict": "fraction of branches predicted wrong — the main cause of 'bad speculation' slots",
    "DRAM bandwidth": "rate of data moved to/from main memory (DRAM = dynamic RAM, the off-chip main memory)",
    "cache-miss rate": "fraction of last-level-cache accesses that missed and had to go to DRAM",
    "last-level cache misses": "LLC (last-level cache) misses per 1k instructions — the largest/slowest on-die cache before DRAM",
    "data-TLB misses": "data TLB misses per 1k instructions (TLB = translation-lookaside buffer; a miss triggers a page-table walk)",
    "instruction-TLB misses": "instruction TLB misses per 1k instructions — page-walk pressure from a large code footprint",
    "DRAM fills": "demand cache-line fills from DRAM per 1k instructions",
    "DRAM-bound": "share of demand cache fills that came from DRAM (vs. a closer cache)",
    "NUMA remote access": "share of memory accesses served by another socket's memory (NUMA = non-uniform memory access; remote is slower)",
    "memory-bound (slots)": "estimated share of pipeline slots stalled waiting on the memory hierarchy",
    "MPI time": "time in MPI calls (MPI = Message-Passing Interface, the inter-rank communication library)",
    "MPI imbalance": "(max-avg)/max of per-rank MPI time — the recoverable fraction if ranks were balanced",
    "FP efficiency": "achieved floating-point throughput vs. the CPU's vector (SIMD) peak — low means scalar/under-vectorized code",
    # short labels used in the HTML cards/tables and the roofline
    "IPC": "instructions per cycle — retired instructions ÷ cycles (higher = more throughput)",
    "CPI": "cycles per instruction — cycles ÷ retired instructions (lower = cheaper instructions)",
    "GFLOP/s": "billions of floating-point operations per second",
    "core util": "physical-core utilization — busy cores ÷ available cores",
    "peak RSS": "peak resident set size — the most physical memory the run held at once",
    "AI": "arithmetic intensity — FLOPs performed per byte read from DRAM (FLOP/byte)",
}

# Short display labels (HTML/roofline) that mean the same as a glossary key above.
_GLOSS_ALIASES = {
    "cache-miss": "cache-miss rate", "NUMA remote": "NUMA remote access",
    "mem-bound": "memory-bound (slots)", "frontend": "frontend-bound",
    "backend": "backend-bound", "DRAM bandwidth ": "DRAM bandwidth",
}


def define(term):
    """Plain-language definition for a label, following short-label aliases. Returns
    None if the term isn't in the glossary (so callers can skip the tooltip)."""
    return GLOSSARY.get(term) or GLOSSARY.get(_GLOSS_ALIASES.get(term, ""))


def _gloss(out, *terms):
    """Append `# term — definition` comment lines for any known terms (Intel-APS
    style). Unknown terms are silently skipped so callers can pass labels freely."""
    for t in terms:
        d = GLOSSARY.get(t)
        if d:
            roofline.comment(out, "%s — %s" % (t, d))


def _rule(header):
    """A horizontal divider matching a 4-space-indented table header's width."""
    return "    " + "─" * (len(header) - 4)


def _cpu(profile):
    """time% denominator: total CPU time (thread-seconds) so a function's share is
    bounded 0-100% and comparable to Samp%. Falls back to wall runtime if absent."""
    p = profile or {}
    return p.get("cpu_time_s") or p.get("runtime_s", 0.0) or 0.0


def _load_json(path):
    try:
        return json.load(open(path))
    except Exception:
        return None


def _m(snap, key):
    for x in (snap or {}).get("metrics", []):
        if x.get("key") == key:
            return x.get("value")
    return None


def _disp(snap, key):
    for x in (snap or {}).get("metrics", []):
        if x.get("key") == key:
            return x.get("display")
    return None


# ----------------------------------------------------- environment / run info
import glob as _glob          # noqa: E402
import platform as _plat      # noqa: E402
import socket as _socket      # noqa: E402
import subprocess as _sp      # noqa: E402
import datetime as _dt        # noqa: E402

# Loaded sci-lib sonames carry their version (libfftw3.so.3.7.11, libmpi.so.40.40.7,
# libopenblasp-r0.3.33.so) — label -> substring to match in the basename.
_KNOWN_LIBS = [("OpenBLAS", "libopenblas"), ("BLIS", "libblis"), ("MKL", "libmkl_rt"),
               ("MKL", "libmkl_core"), ("ATLAS", "libatlas"), ("ESSL", "libessl"),
               ("NVPL", "libnvpl"), ("LAPACK", "liblapack"), ("ScaLAPACK", "libscalapack"),
               ("FFTW", "libfftw3"), ("Open MPI", "libmpi.so"), ("MPICH", "libmpich"),
               ("Intel MPI", "libmpi_intel")]


def _read(path):
    try:
        return open(path).read()
    except OSError:
        return ""


def _human_bytes(n):
    n = float(n)
    for u in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or u == "TiB":
            return ("%.0f %s" % (n, u)) if u == "B" else ("%.1f %s" % (n, u))
        n /= 1024.0


def _cores_sockets():
    """(physical cores, sockets) from /proc/cpuinfo."""
    cores, socks, cur = set(), set(), {}
    for line in _read("/proc/cpuinfo").splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            cur[k.strip()] = v.strip()
        elif "physical id" in cur and "core id" in cur:
            cores.add((cur["physical id"], cur["core id"]))
            socks.add(cur["physical id"])
            cur = {}
    if "physical id" in cur and "core id" in cur:
        cores.add((cur["physical id"], cur["core id"]))
        socks.add(cur["physical id"])
    return (len(cores) or os.cpu_count() or 1), (len(socks) or 1)


def _os_pretty():
    for line in _read("/etc/os-release").splitlines():
        if line.startswith("PRETTY_NAME="):
            return line.split("=", 1)[1].strip().strip('"')
    return _plat.system()


def _cpu_uarch():
    """Microarchitecture + codename from /proc/cpuinfo CPUID (x86 family/model or
    ARM implementer/part), e.g. 'Zen 5 (Granite Ridge)', 'Sapphire Rapids',
    'Neoverse V2 (Grace)'. None if unrecognized."""
    info = {}
    for line in _read("/proc/cpuinfo").splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            info.setdefault(k.strip(), v.strip())
    vendor = info.get("vendor_id", "")
    try:
        fam = int(info.get("cpu family", "0"))
        mod = int(info.get("model", "0"))
    except ValueError:
        fam = mod = 0
    if vendor == "AuthenticAMD":
        if fam == 0x1A:  # Zen 5
            return ("Zen 5 (Turin)" if mod <= 0x1F else
                    "Zen 5 (Granite Ridge)" if 0x40 <= mod <= 0x4F else
                    "Zen 5 (Strix Point)" if 0x20 <= mod <= 0x2F else "Zen 5")
        if fam == 0x19:  # Zen 3 / Zen 4
            return ("Zen 3 (Milan)" if mod <= 0x0F else
                    "Zen 4 (Genoa)" if 0x10 <= mod <= 0x1F else
                    "Zen 3 (Vermeer)" if 0x20 <= mod <= 0x2F else
                    "Zen 3 (Cezanne)" if 0x50 <= mod <= 0x5F else
                    "Zen 4 (Raphael)" if 0x60 <= mod <= 0x6F else
                    "Zen 4 (Phoenix)" if 0x70 <= mod <= 0x7F else
                    "Zen 4c (Bergamo/Siena)" if 0xA0 <= mod <= 0xAF else "Zen 3 / Zen 4")
        if fam == 0x17:  # Zen / Zen+ / Zen 2
            return ("Zen (Naples)" if mod <= 0x0F else
                    "Zen 2 (Rome)" if 0x30 <= mod <= 0x3F else
                    "Zen 2 (Matisse/Renoir)" if 0x60 <= mod <= 0x7F else "Zen / Zen 2")
    if vendor == "GenuineIntel" and fam == 6:
        return {0x4F: "Broadwell-EP", 0x56: "Broadwell-DE",
                0x55: "Skylake / Cascade / Cooper Lake-SP",
                0x6A: "Ice Lake-SP", 0x6C: "Ice Lake-D",
                0x8F: "Sapphire Rapids", 0xCF: "Emerald Rapids",
                0xAD: "Granite Rapids", 0xAF: "Sierra Forest",
                0x57: "Knights Landing", 0x85: "Knights Mill"}.get(mod)
    # ARM: identified by implementer + part (hex), independent of vendor_id.
    try:
        part = int(info.get("CPU part", "0"), 16)
    except ValueError:
        part = 0
    return {0xd0c: "Neoverse N1", 0xd40: "Neoverse V1", 0xd49: "Neoverse N2",
            0xd4f: "Neoverse V2 (Grace / Graviton4)", 0xd83: "Neoverse V3",
            0xd8e: "Neoverse N3"}.get(part)


def _compiler_of(app):
    """Compiler string(s) from the app binary's ELF .comment section (best-effort)."""
    if not app or not os.path.exists(app):
        return None
    try:
        r = _sp.run(["readelf", "-p", ".comment", app], capture_output=True, text=True, timeout=10)
    except (OSError, _sp.SubprocessError):
        return None
    out = []
    for line in r.stdout.splitlines():
        i = line.find("]")
        s = line[i + 1:].strip() if i != -1 else ""
        if s and any(k in s for k in ("GCC", "clang", "Intel", "ifort", "ifx", "nvc", "AOCC", "Flang")):
            out.append(s)
    return " | ".join(dict.fromkeys(out)) if out else None


def environment_view(result_dir, manifest, snap, profile, out, collector=None):
    """APS-style header as grouped ══ sections (Run / Machine / Software /
    Measurement), one fact per line."""
    for title, items in environment_rows(result_dir, manifest, snap, profile, collector):
        if not items:
            continue
        out.append("\n══ %s ══" % title)
        for k, v in items:
            out.append("    %-14s %s" % (k, v))


def environment_rows(result_dir, manifest, snap, profile, collector=None):
    """Run/environment metadata grouped into sections: [(title, [(label, value), …]), …]
    — Run (app/command/ranks/threads/time), Machine (CPU/cores/SMT/memory/host),
    Software (OS/kernel/libraries+versions/compiler), Measurement (HWPC scope/paranoid/
    sampling). Shared by the text and HTML reports.

    `collector` ("uaps" | "upat" | None) scopes the two tier-specific bits: the
    *timing* (snap and profile may be separate runs, so a upat report takes its
    elapsed/CPU from the profile and a uaps report from the snapshot) and the
    *Measurement* block (HWPC scope for the snapshot vs statistical sampling Hz for
    the profiler). Machine and Software are shared, run-independent metadata."""
    profs = sorted(_glob.glob(os.path.join(result_dir, "prof.*.json")))
    raw = next((d for d in (_load_json(p) for p in profs) if d), None)
    manifest = manifest or {}
    app_path = (raw or {}).get("application") or (manifest.get("command") or [""])[0]
    app = os.path.basename(app_path) if app_path else "?"
    cmd = " ".join(manifest.get("command", []))
    nranks = ((profile or {}).get("nranks") or _m(snap, "nranks")
              or _m(snap, "mpi_ranks") or len(profs) or 1)
    nthr = int((raw or {}).get("nthreads") or _m(snap, "max_threads") or 1)
    # Timing is per-run: snap and profile can be separate runs (the suite collects
    # each tier independently), so a upat report reports the profile's own wall/CPU
    # time and a uaps report the snapshot's — never one tier's timing on the other's.
    if collector == "upat":
        elapsed = (profile or {}).get("runtime_s") or _m(snap, "elapsed_time")
        ct = (profile or {}).get("cpu_time_s")           # per-rank avg
        cputime = ct * nranks if ct else None            # -> total job CPU
    else:
        elapsed = _m(snap, "elapsed_time") or (profile or {}).get("runtime_s")
        cputime = _m(snap, "cpu_time")
        if not cputime:
            ct = (profile or {}).get("cpu_time_s")
            cputime = ct * nranks if ct else None
    mtimes = [os.path.getmtime(p) for p in (profs + [os.path.join(result_dir, contract.SNAP)])
              if os.path.exists(p)]
    date = _dt.datetime.fromtimestamp(max(mtimes)).strftime("%Y-%m-%d %H:%M") if mtimes else ""

    # Grouped into dedicated sections; one fact per line (easy to grep).
    run = [("application", app)]
    if cmd:
        run.append(("command", cmd))
    run.append(("ranks", str(nranks)))
    run.append(("threads/rank", str(nthr)))
    if elapsed:
        run.append(("elapsed", "%.1f s" % elapsed))
    if cputime:
        run.append(("CPU time", "%.0f s" % cputime))
    if date:
        run.append(("date", date))

    # CPU brand string: empirical peaks first. ARM has no /proc/cpuinfo "model
    # name" line (so the empirical brand is empty) — prefer the microarch name
    # (e.g. "Neoverse V2 (Grace …)") over the bare "aarch64" from platform.
    cpu = ((roofline.peaks() or {}).get("cpu") or _cpu_uarch()
           or _plat.processor() or "?")
    cores, socks = _cores_sockets()
    smt = _read("/sys/devices/system/cpu/smt/active").strip()
    memkb = next((int(l.split()[1]) for l in _read("/proc/meminfo").splitlines()
                  if l.startswith("MemTotal:")), 0)
    machine = [("CPU", cpu)]
    uarch = _cpu_uarch()
    if uarch:
        machine.append(("microarch", uarch))
    machine.append(("cores", "%d socket%s × %d physical (%d logical)"
                    % (socks, "" if socks == 1 else "s", cores, os.cpu_count() or cores)))
    if smt in ("0", "1"):
        machine.append(("SMT", "on" if smt == "1" else "off"))
    if memkb:
        machine.append(("memory", _human_bytes(memkb * 1024)))
    machine.append(("host", _socket.gethostname()))

    software = [("OS", _os_pretty()), ("kernel", _plat.release())]
    libs = {}
    for m in (raw or {}).get("sampling", {}).get("maps", []):
        b = os.path.basename(m.get("path", "")).lower()
        for label, key in _KNOWN_LIBS:
            if key in b and label not in libs:
                libs[label] = os.path.basename(m.get("path", ""))
    software += list(libs.items())           # one library per line
    cc = _compiler_of(app_path)
    if cc:
        software.append(("compiler", cc))

    # How the data was gathered — genuinely tier-specific: the snapshot reads HW
    # counters (node-wide or per-process), the profiler statistically samples call
    # stacks at a fixed rate. Show each tier only its own collection method.
    measurement = []
    par = _read("/proc/sys/kernel/perf_event_paranoid").strip()
    hz = (raw or {}).get("sampling", {}).get("hz")
    if collector == "upat":
        if hz:
            measurement.append(("method", "statistical call-stack sampling"))
            measurement.append(("sampling", "%d Hz" % hz))
        if par:
            measurement.append(("paranoid", par))
    else:
        measurement.append(("method", "hardware performance counters"))
        measurement.append(("HWPC scope",
                            "node-level (system-wide)" if _m(snap, "system_wide") else "per-process"))
        if par:
            measurement.append(("paranoid", par))

    return [("Run", run), ("Machine", machine), ("Software", software),
            ("Measurement", measurement)]


# ----------------------------------------------------------------- roofline
def roofline_view(snap, profile, out):
    """Whole-program roofline from the snapshot's hardware counters: one point
    (measured FLOP/s vs DRAM traffic) placed against the empirical ceilings.

    Per-FUNCTION roofline is intentionally NOT here: shape-derived per-kernel
    points explode (same kernel × every input size), double-count nested calls,
    and only cover a few hand-coded formulas. That belongs to a two-pass profile
    feature (survey hot functions → characterize each with counters), where it
    works for library, user, and system code alike."""
    # GPU offload: uaps reads only CPU counters, so a CPU-only roofline would
    # misrepresent a GPU-offloaded job (near-zero CPU FLOPs → bogus "idle"/"memory-
    # bound" placement). Suppress it and say why, rather than plot a wrong point.
    if _m(snap, "gpu_offload"):
        out.append("\n══ Roofline (whole program) ══")
        out.append("    (suppressed — GPU offload detected. uaps measures only CPU counters, so a")
        out.append("     CPU-only roofline misrepresents a GPU-offloaded job. Profile the device")
        out.append("     kernels with a GPU tool — nsight / rocprof / VTune.)")
        return
    pk = roofline.peaks()
    points = []
    g = _m(snap, "gflops")
    # DRAM read traffic incl. the hardware prefetcher (mem_dram_reads); fall back
    # to demand-from-DRAM fills where the L2 counter is absent. Demand-only badly
    # undercounts streaming traffic, which would inflate the arithmetic intensity.
    fills = _m(snap, "mem_dram_reads") or _m(snap, "mem_fills_dram")
    elapsed = _m(snap, "elapsed_time")
    bw_gbs = ai = None
    if g and fills and elapsed:
        dram_bytes = fills * 64.0
        flops = g * 1e9 * elapsed
        if dram_bytes > 0:
            bw_gbs = dram_bytes / elapsed / 1e9
            ai = flops / dram_bytes
            # whole-program FP-counter precision is vendor-dependent (Intel = DP
            # only; AMD = mixed SP+DP) → judged against the DP ceiling.
            points.append({"label": "whole-program (measured)", "ai": ai,
                           "gflops": g, "prec": "dp"})
    out.append("\n══ Roofline (whole program) ══")
    roofline.render(points, pk, out)
    if not points:
        out.append("    (no whole-program point — %s)" % _hwpc_gap_reason(snap))
        return
    # The measured point's coordinates, spelled out (application FLOP rate +
    # achieved DRAM bandwidth + their ratio).
    out.append("    measured application:  %.1f GFLOP/s   %.1f GB/s DRAM   AI %.3f FLOP/byte"
               % (g, bw_gbs, ai))
    if _m(snap, "fp_mixed_precision"):
        # AMD/ARM: FP count mixes SP+DP with no split, and the compute roof is
        # precision-dependent — place the point against BOTH roofs, don't guess one.
        v = roofline.precision_unknown_summary(points[0]["ai"], points[0]["gflops"], pk)
        if v:
            out.append("    whole program: %s" % v)
    else:
        c = roofline.classify(points[0]["ai"], points[0]["gflops"], pk, "dp")
        if c:
            if c[2] == "latency":
                out.append("    whole program sits at %.0f%% of the DP ceiling — far below the "
                           "roofline: latency/overhead/idle-bound, not compute- or bandwidth-bound."
                           % c[1])
            else:
                out.append("    whole program is %s-bound at %.0f%% of the DP ceiling." % (c[2], c[1]))


# ------------------------------------------------ per-function roofline (B)
def roofline_func_view(profile, out):
    """Per-function roofline from event-based sampling (the characterize pass):
    flops = FP-op samples x period, DRAM bytes = fill samples x period x line,
    time = exclusive (self) time samples / Hz. Works for any function — library,
    user, or system — ranked by exclusive time (the survey). Precision per
    arbitrary function isn't knowable from the FP counter, so points are judged
    against the DP ceiling."""
    rf = (profile or {}).get("roofline_functions")
    if not rf:
        return
    pk = roofline.peaks()
    hz = rf.get("hz", 0)
    rows = []
    for fn, e in rf.get("functions", {}).items():
        # flops/bytes are already period- and width-weighted in postprocess
        # (Intel's 4 FP_ARITH umasks summed at 1/2/4/8; AMD/ARM single events).
        self_s, fp_samp = e.get("self", 0), e.get("fp_samp", 0)
        flops, byts = e.get("flops", 0.0), e.get("bytes", 0.0)
        if self_s < 2 and fp_samp < 4:          # below the noise floor
            continue
        t = self_s / hz if hz else 0.0
        if t <= 0:
            continue
        gflops = flops / t / 1e9
        ai = (flops / byts) if byts > 0 else None
        rows.append((self_s, fn, e.get("group", "ETC"), t, gflops, ai))
    if not rows:
        return
    rows.sort(reverse=True)                       # by exclusive time = the hotspots
    # time% denominator = total roofline-sampler samples (over *all* sampled
    # functions, not just those above the noise floor) so the column is the
    # sampler's own Samp% — bounded 0-100% and summing to 100%. (Using CPU/wall
    # seconds here breaks for blocking syscalls: their wall/thread-summed self-time
    # exceeds CPU time and the % runs past 100%.)
    total_self = sum(e.get("self", 0) for e in rf.get("functions", {}).values()) or 1
    out.append("\n══ Roofline (per function — measured, event-based sampling) ══")
    out.append("  flops = FP-event samples x period x width; bytes = DRAM-access samples x line.")
    # Log-log roofline plot of the hottest functions that have measurable DRAM
    # traffic (finite AI); each is a lettered point matching the table below.
    # Compute-bound / cache-resident functions (AI=inf) can't be placed on the
    # AI axis, so they appear only in the table.
    pts = [{"label": fn, "ai": ai, "gflops": gflops, "prec": "dp"}
           for self_s, fn, grp, t, gflops, ai in rows
           if ai is not None and gflops > 0][:8]
    plot = roofline.ascii_plot(pts, pk) if pts else []
    if plot:
        out.append("")
        out.extend(plot)
        out.append("")
    # mark each plotted function with its plot letter in the table
    letter = {p["label"]: chr(ord("A") + i) for i, p in enumerate(pts)} if len(pts) > 1 else {}
    hdr = ("    %1s %-24s %7s %7s %6s %9s %6s  bound"
           % ("", "function", "self(s)", "time%", "AI", "GFLOP/s", "%peak"))
    out.append(hdr)
    out.append(_rule(hdr))
    for self_s, fn, grp, t, gflops, ai in rows[:12]:
        c = roofline.classify(ai if ai is not None else 1e12, gflops, pk, "dp")
        if not c:
            continue
        ceil, pct, bound = c
        ais = "%.1f" % ai if ai is not None else "  inf"
        if ai is None:
            bound = "compute"
        tp = 100.0 * self_s / total_self
        out.append("    %1s %-24s %7.3f %6.1f%% %6s %9.1f %5.0f%%  %s"
                   % (letter.get(fn, ""), fn[:24], t, tp, ais, gflops, pct, bound))
    out.append(_rule(hdr))
    out.append("    (AI=inf → no DRAM traffic sampled = cache-resident/compute-bound; precision assumed DP)")


# ------------------------------------------------------- microarch / memory
def _hwpc_gap_reason(snap):
    """Why a vendor-HWPC metric (top-down slots / FP / roofline) is absent — inferred
    from what DID populate. The vendor PMU groups need the thread ON-CPU and free PMU
    counter slots; the generic counters (IPC) are single events that survive when the
    larger vendor groups can't be scheduled, so their presence localizes the cause."""
    if _m(snap, "ipc") is None and _m(snap, "hw_instructions") is None:
        # even the generic single events are gone → counting itself was disallowed
        return ("no HW counters at all — perf_event_paranoid is > 1 (need ≤ 1 for "
                "per-process counting), or perf is disabled / unavailable")
    # generic counters worked, so paranoid is fine — the *vendor PMU group* didn't run.
    nr = _m(snap, "nranks") or 1
    thr = _m(snap, "max_threads") or 1
    busy = _m(snap, "cpu_cores_used")          # avg cores busy (summed over ranks)
    on_cpu = (busy / (nr * thr) * 100.0) if busy else 0.0
    if on_cpu < 10.0:
        return ("threads were only ~%.0f%% on-CPU (idle / I/O- or sync-bound), so the "
                "per-thread PMU group accumulated too little runtime to measure — "
                "oversubscription compounds it" % on_cpu)
    return ("the vendor PMU group could not be scheduled — the pmu-events DB was not "
            "found for this CPU model, or the PMU counters were oversubscribed")


def microarch_view(snap, out):
    if not snap:
        return
    out.append("\n══ Microarchitecture (top-down pipeline slots) ══")
    slots_shown = False
    for k, lbl in [("topdown_retiring_pct", "retiring (useful)"),
                   ("topdown_frontend_pct", "frontend-bound"),
                   ("topdown_backend_pct", "backend-bound"),
                   ("topdown_backend_mem_pct", "  └ memory"),
                   ("topdown_backend_core_pct", "  └ core"),
                   ("topdown_badspec_pct", "bad speculation")]:
        v = _m(snap, k)
        if v is not None:
            out.append("    %-18s %5.1f%%" % (lbl, v))
            slots_shown = True
    # SMT contention = the slots unaccounted by the four buckets (the sibling thread
    # took them). Only meaningful with SMT on; NA otherwise (then the four sum ~100%).
    four = [_m(snap, k) for k in ("topdown_retiring_pct", "topdown_frontend_pct",
                                  "topdown_backend_pct", "topdown_badspec_pct")]
    if all(v is not None for v in four):
        if _m(snap, "smt_active"):
            out.append("    %-18s %5.1f%%" % ("SMT contention", max(0.0, 100.0 - sum(four))))
            roofline.comment(out, "retiring + frontend + backend + bad-spec + SMT = 100% of slots")
        else:
            out.append("    %-18s %5s" % ("SMT contention", "NA"))
    # The four slots come from the vendor top-down PMU GROUP (co-scheduled events that
    # need the thread on-CPU); when it couldn't be measured, say so + why rather than
    # silently dropping the slots (branch mispredict below is generic-derived and may
    # still appear).
    if not slots_shown:
        out.append("    top-down slots unavailable — %s." % _hwpc_gap_reason(snap))
    # branch mispredict is NOT part of the slot partition above — it's a rate over
    # branches and the main *cause* of the 'bad speculation' slots. Shown separately.
    bm = _disp(snap, "branch_mispredict_rate")
    if bm is not None:
        out.append("    %-18s %s   (of branches — main cause of bad speculation)"
                   % ("branch mispredict", bm))
    _gloss(out, "retiring", "frontend-bound", "backend-bound", "bad speculation",
           "SMT contention", "branch mispredict")


def memory_view(snap, out):
    if not snap:
        return
    out.append("\n══ Memory access ══")
    bw = _m(snap, "dram_bandwidth_gbs")
    if bw is not None:
        peak_bw = (roofline.peaks() or {}).get("peak_bw_gbs")
        s = "%.1f GB/s" % bw + (" (%.0f%% of peak)" % (bw / peak_bw * 100.0) if peak_bw else "")
        out.append("    %-22s %s" % ("DRAM bandwidth", s))
    # NB: dTLB *rate* is dropped — the generic HW_CACHE DTLB access event under-counts
    # on AMD, so misses/access is unreliable; the instruction-normalized MPKI is robust.
    for k, lbl in [("cache_miss_rate", "cache-miss rate"), ("llc_mpki", "last-level cache misses"),
                   ("dtlb_mpki", "data-TLB misses"), ("itlb_mpki", "instruction-TLB misses"),
                   ("dram_dpki", "DRAM fills"), ("dram_bound_pct", "DRAM-bound"),
                   ("numa_remote_pct", "NUMA remote access"), ("memory_bound", "memory-bound (slots)")]:
        if _disp(snap, k) is not None:
            out.append("    %-24s %s" % (lbl, _disp(snap, k)))
    _gloss(out, "DRAM bandwidth", "cache-miss rate", "last-level cache misses",
           "data-TLB misses", "instruction-TLB misses", "NUMA remote access",
           "memory-bound (slots)")


def mpi_snapshot_view(snap, out):
    """APS-style MPI bird's-eye from the snapshot's own PMPI-shim metrics (present
    when the run was MPI): total MPI time, % of runtime, imbalance, ranks, and the
    top calls by time. (The profile collector's MPI section is more detailed; this
    is the snapshot's at-a-glance view.)"""
    if _m(snap, "mpi_time") is None:
        return
    out.append("\n══ MPI ══")
    for k, lbl in [("mpi_time", "MPI time"), ("mpi_time_pct", "MPI % of runtime"),
                   ("mpi_imbalance_pct", "MPI imbalance"), ("mpi_ranks", "ranks")]:
        if _disp(snap, k) is not None:
            out.append("    %-24s %s" % (lbl, _disp(snap, k)))
    tops = [(x.get("label", "").strip(), x.get("display", ""))
            for k in ("mpi_top1", "mpi_top2", "mpi_top3", "mpi_top4", "mpi_top5")
            for x in (snap or {}).get("metrics", []) if x.get("key") == k]
    if tops:
        out.append("    top calls (time):")
        for name, t in tops:
            out.append("      %-34s %s" % (name, t))
    _gloss(out, "MPI time", "MPI imbalance")


def time_breakdown_view(profile, out):
    """Where wall time goes, from the sampling dominant-group attribution (each
    sample charged to the highest-priority group on its stack). A quick 'is this
    MPI-, compute-, or idle-bound' read, as a horizontal bar."""
    groups = (profile or {}).get("groups") or {}
    total = (profile or {}).get("group_total") or 0
    if not groups or total <= 0:
        return
    cat = {"MPI": "MPI", "IO": "I/O", "USER": "user code",
           "WAIT": "idle/wait", "ETC": "system/other",
           "BLAS": "math-libs", "LAPACK": "math-libs", "PBLAS": "math-libs",
           "ScaLAPACK": "math-libs", "CBLAS": "math-libs", "LAPACKe": "math-libs", "FFTW": "math-libs"}
    agg = {}
    for g, n in groups.items():
        c = cat.get(g, "other / wait")
        agg[c] = agg.get(c, 0) + n
    out.append("\n══ Time breakdown (sampled, by dominant group) ══")
    for c, n in sorted(agg.items(), key=lambda x: -x[1]):
        pc = 100.0 * n / total
        out.append("    %-14s %5.1f%%  %s" % (c, pc, "█" * int(round(pc / 100.0 * 40))))


def scheduling_view(snap, out):
    """Kernel software counters: context switches, CPU migrations, page faults —
    scheduling pressure / oversubscription / memory-pressure signals."""
    if _m(snap, "ctx_switches") is None and _m(snap, "page_faults") is None:
        return
    el = _m(snap, "elapsed_time") or 0.0
    out.append("\n══ Scheduling / OS ══")

    def _persec(v):
        return "%.0f/s" % (v / el) if el else "%d" % int(v)

    # Per-second rates (comparable across run lengths), with the raw total in
    # parentheses for context. These are scheduling/OS events — time-normalized,
    # not instruction-normalized like the cache/TLB counters above.
    for k, lbl in [("ctx_switches", "context switches"),
                   ("cpu_migrations", "CPU migrations"),
                   ("page_faults", "page faults (minor+major)")]:
        v = _m(snap, k)
        if v is not None:
            out.append("    %-26s %12s   (%d total)" % (lbl, _persec(v), int(v)))
    mj = _m(snap, "page_faults_maj")
    if mj is not None:
        # Major faults hit disk — call them out absolutely; even a few hurt.
        out.append("    %-26s %12d   %s" % ("major page faults", int(mj),
                   "(disk-backed — should be ~0)" if mj else "(none — no disk paging)"))


# ----------------------------------------------------------- load imbalance
def imbalance_view(profile, out):
    fns = (profile or {}).get("functions", [])
    nr = (profile or {}).get("nranks", 1)
    if nr < 2:
        return
    rows = []
    for f in fns:
        imb = f.get("imb_excl", 0.0)
        t = f.get("t_excl", 0.0)
        if t > 0.005 and imb >= 5:
            # recoverable = what the slow rank could shed = max - avg. With
            # imb = (max-avg)/max and t = avg, this is t*(imb/100)/(1-imb/100).
            frac = min(imb, 99.0) / 100.0
            recoverable = t * frac / (1.0 - frac)   # ~ time the slow rank could shed
            rows.append((recoverable, imb, t, f.get("name", ""), f.get("group", "")))
    if not rows:
        return
    runtime = _cpu(profile)
    rows.sort(reverse=True)
    out.append("\n══ Load imbalance (across %d ranks, ranked by recoverable time) ══" % nr)
    hdr = "    %-24s %6s %7s %11s %11s" % ("function", "imb%", "time%", "avg excl(s)", "recover(s)")
    out.append(hdr)
    out.append(_rule(hdr))
    for rec, imb, t, name, grp in rows[:10]:
        tp = 100.0 * t / runtime if runtime else 0.0
        out.append("    %-24s %5.0f%% %6.1f%% %11.4f %11.4f" % (name[:24], imb, tp, t, rec))
    out.append(_rule(hdr))


# ----------------------------------------------------------- MPI wait-state
# Classify MPI calls by what dominates their time. Collectives and blocking
# receives spend most of their time *blocked on peers* (synchronization / wait);
# sends and nonblocking initiations are *transfer/initiation*. This is a call-type
# heuristic — true late-sender/late-receiver attribution needs cross-rank trace
# replay, which the collectors don't do.
_MPI_TRANSFER = re.compile(
    r"MPI_I?[BSR]?send|MPI_Irecv|MPI_Put|MPI_Get|MPI_R?Accumulate|MPI_[PU]npack|MPI_Start",
    re.I)
_MPI_WAIT = re.compile(
    r"MPI_(Barrier|Wait|Test|Recv|Sendrecv|M?Probe|Allreduce|Reduce|Allgather|Gather|"
    r"Scatter|Alltoall|Bcast|E?Scan|Reduce_scatter|Neighbor)",
    re.I)
# nonblocking initiations: I-prefixed p2p + nonblocking collectives + one-sided
_MPI_NONBLOCK = re.compile(
    r"MPI_(I[bsr]?send|Issend|Irecv|Iallreduce|Ireduce|Iallgather|Igather|Iscatter|"
    r"Ialltoall|Ibcast|Ibarrier|Iscan|Put|Get|R?Accumulate)", re.I)
_MPI_WAITCALL = re.compile(r"MPI_(Wait|Test)", re.I)


def _mpi_class(name):
    # wait first: e.g. MPI_Sendrecv contains "Send" but is a blocking exchange
    if _MPI_WAIT.search(name):
        return "wait"
    if _MPI_TRANSFER.search(name):
        return "transfer"
    return "transfer"            # default unknowns to transfer (conservative)


def mpi_summary_view(snap, profile, out):
    """APS-style bird's-eye MPI summary: total MPI time, % of runtime, and the
    top 5 MPI functions by time. (The detailed wait-state breakdown + comm matrix
    live in the UPAT section / --detail mpi.)"""
    fns = [f for f in (profile or {}).get("functions", []) if f.get("group") == "MPI"]
    nr = (profile or {}).get("nranks", 1)
    if not fns or nr < 2:                      # only meaningful for parallel runs
        return
    runtime = (profile or {}).get("runtime_s", 0.0) or (_m(snap, "elapsed_time") or 0.0)
    mpi_t = sum(f.get("t_incl", 0.0) for f in fns)
    out.append("\n══ MPI summary ══")
    out.append("    MPI time %.4fs  (%.1f%% of runtime, %d ranks)"
               % (mpi_t, (mpi_t / runtime * 100.0) if runtime else 0.0, nr))
    top = sorted(fns, key=lambda f: -f.get("t_incl", 0.0))[:5]
    hdr = "    %-20s %10s %7s %10s %6s" % ("function", "time(s)", "%MPI", "calls", "imb%")
    out.append(hdr)
    out.append(_rule(hdr))
    for f in top:
        ft = f.get("t_incl", 0.0)
        out.append("    %-20s %10.4f %6.1f%% %10.0f %5.0f%%"
                   % (f.get("name", "")[:20], ft, (ft / mpi_t * 100.0) if mpi_t else 0.0,
                      f.get("count", 0), f.get("imb_excl", 0.0)))
    out.append(_rule(hdr))


def mpi_view(profile, out):
    fns = [f for f in (profile or {}).get("functions", []) if f.get("group") == "MPI"]
    if not fns:
        return
    nr = (profile or {}).get("nranks", 1)
    if nr < 2:                                 # MPI wait-state is a multi-rank concern;
        return                                 # for a single rank these calls are no-ops
    rows, wait_t, xfer_t = [], 0.0, 0.0
    for f in fns:
        t = f.get("t_excl", 0.0)
        cls = _mpi_class(f.get("name", ""))
        if cls == "wait":
            wait_t += t
        else:
            xfer_t += t
        rows.append((t, cls, f.get("name", ""), f.get("imb_excl", 0.0)))
    total = wait_t + xfer_t
    if total <= 0:
        return
    rows.sort(reverse=True)
    out.append("\n══ MPI wait-state (call-type heuristic) ══")
    out.append("    synchronization/wait %6.4fs (%2.0f%%) | transfer/initiation %6.4fs (%2.0f%%)"
               % (wait_t, wait_t / total * 100, xfer_t, xfer_t / total * 100))
    # imb% is a cross-rank metric — omit it for a single-rank run (always 0%).
    runtime = _cpu(profile)
    imb_col = nr >= 2
    hdr = ("    %-20s %7s %10s %7s %6s" % ("call", "time%", "excl(s)", "class", "imb%")) if imb_col \
        else ("    %-20s %7s %10s %7s" % ("call", "time%", "excl(s)", "class"))
    out.append(hdr)
    out.append(_rule(hdr))
    for t, cls, name, imb in rows[:10]:
        tp = 100.0 * t / runtime if runtime else 0.0
        out.append(("    %-20s %6.1f%% %10.5f %7s %5.0f%%" % (name[:20], tp, t, cls, imb)) if imb_col
                   else ("    %-20s %6.1f%% %10.5f %7s" % (name[:20], tp, t, cls)))
    out.append(_rule(hdr))
    # late-sender / load-imbalance signal: wait dominates AND a wait call is imbalanced
    wait_imb = max((imb for t, cls, name, imb in rows if cls == "wait"), default=0.0)
    if nr >= 2 and wait_t / total >= 0.6 and wait_imb >= 15:
        out.append("    → wait-dominated MPI with imbalanced collectives/receives "
                   "(%.0f%% imb): likely late-sender / load imbalance — rebalance work "
                   "across ranks before tuning communication." % wait_imb)
    elif nr >= 2 and wait_t / total >= 0.6:
        out.append("    → wait-dominated but balanced: ranks are well-balanced; "
                   "reduce synchronization (fewer/larger collectives, overlap with compute).")
    # comm/compute overlap: does the app even attempt overlap, and does Wait eat it?
    names = [n for _, _, n, _ in rows]
    has_nb = any(_MPI_NONBLOCK.search(n) for n in names)
    wait_call_t = sum(t for t, _, n, _ in rows if _MPI_WAITCALL.search(n))
    # blocking comm = wait-class time that isn't just polling Wait/Test calls
    has_blocking_comm = (wait_t - wait_call_t) > 0
    if not has_nb and has_blocking_comm:
        out.append("    overlap: all communication is blocking — no comm/compute overlap "
                   "possible. Consider nonblocking calls (Isend/Irecv or Iallreduce-style "
                   "collectives) + compute + Wait.")
    elif has_nb and wait_call_t / total >= 0.3:
        out.append("    overlap: nonblocking comm in use but Wait* is %.0f%% of MPI time — "
                   "little compute overlapped; move independent work between init and Wait."
                   % (wait_call_t / total * 100))
    elif has_nb:
        out.append("    overlap: nonblocking comm in use with low Wait* time — "
                   "good comm/compute overlap.")


# ------------------------------------------------------- anomaly / variance
def _stats(xs):
    n = len(xs)
    mean = sum(xs) / n
    var = sum((x - mean) ** 2 for x in xs) / n
    return mean, math.sqrt(var)


def _median(xs):
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2.0


def anomaly_view(result_dir, out):
    """Cross-rank outlier + variance detection from the raw per-rank profiles.
    Surfaces the rank(s) deviating from the pack and the call that varies most
    across ranks — the signal that says *which* rank/region to chase."""
    profs = contract.prof_glob(result_dir)
    ranks = []
    for p in profs:
        d = _load_json(p)
        if d is None:
            continue
        busy = sum(f.get("t_excl", 0.0) for f in d.get("functions", []))
        per_fn = {f.get("function", ""): f.get("t_excl", 0.0) for f in d.get("functions", [])}
        ranks.append({"rank": d.get("rank", 0), "busy": busy, "fns": per_fn})
    if len(ranks) < 3:                     # <3 ranks → use the imbalance view instead
        return
    busies = [r["busy"] for r in ranks]
    mean, _ = _stats(busies)
    mx, mn = max(busies), min(busies)
    if mean <= 0:
        return
    out.append("\n══ Anomaly / variance (across %d ranks) ══" % len(ranks))
    spread = (mx - mn) / mx * 100.0 if mx else 0.0
    out.append("    per-rank busy time: min %.4fs  max %.4fs  median %.4fs  spread %.0f%%"
               % (mn, mx, _median(busies), spread))
    # robust outlier test: median + MAD (resistant to the outlier inflating σ).
    med = _median(busies)
    mad = _median([abs(b - med) for b in busies])
    worst = max(ranks, key=lambda r: abs(r["busy"] - med))
    dev = abs(worst["busy"] - med)
    if mad > 0:
        mz = 0.6745 * dev / mad           # modified z-score
        flag = mz > 3.5
        detail = "%.1f modified-z" % mz
    else:                                  # ties at the median → use relative gap
        flag = med > 0 and dev / med > 0.25
        detail = "%.0f%% off median" % (dev / med * 100.0 if med else 0)
    if flag and dev / med > 0.1:
        side = "slower" if worst["busy"] > med else "faster"
        out.append("    → rank %d is an outlier: %.0f%% %s than the median (%s)"
                   % (worst["rank"], dev / med * 100.0, side, detail))
    else:
        out.append("    → no outlier rank (all within the pack).")
    # call with the highest cross-rank variability (present on >=half the ranks)
    allfns = set().union(*(r["fns"].keys() for r in ranks))
    best = None
    for fn in allfns:
        vals = [r["fns"].get(fn, 0.0) for r in ranks]
        present = [v for v in vals if v > 0]
        if len(present) < len(ranks) / 2:
            continue
        fmean, fsd = _stats(vals)
        if fmean <= 1e-4:                  # ignore sub-0.1ms noise
            continue
        cv = fsd / fmean
        if best is None or cv > best[0]:
            best = (cv, fn, min(vals), max(vals))
    if best and best[0] > 0.15:
        out.append("    most variable call: %s (CV %.0f%%, %.4f..%.4fs across ranks)"
                   % (best[1][:24], best[0] * 100.0, best[2], best[3]))


# ----------------------------------------------------------- vectorization
def vectorization_view(snap, profile, out):
    """FP/SIMD efficiency. Prefers a real HWPC vectorization% if the platform
    exposes one; otherwise uses achieved GFLOP/s vs the empirical vector peak as
    a proxy and flags compute-bound code running far below it (scalar/under-vec).
    True per-loop SIMD width needs a compiler vec-report — pointed to below."""
    if not snap:
        return
    vec_pct = _m(snap, "vectorization_pct")
    g = _m(snap, "gflops")
    retiring = _m(snap, "topdown_retiring_pct")
    membound = _m(snap, "topdown_backend_mem_pct")
    pk = roofline.peaks()
    lines = []
    if vec_pct is not None:                       # real HWPC metric (e.g. Intel)
        lines.append("    vectorized FP ops %.1f%% (hardware counter)" % vec_pct)
        if vec_pct < 60 and (retiring or 0) > 30:
            lines.append("    → low SIMD utilization in compute-heavy code — see vec report below.")
    elif g and pk and pk.get("peak_gflops") and not _m(snap, "gpu_offload"):
        # (skip the CPU vector-peak proxy under GPU offload — CPU GFLOP/s is near
        #  zero there, so "FP efficiency" would read as bogus under-vectorization.)
        eff = g / pk["peak_gflops"] * 100.0
        lines.append("    FP efficiency %.1f%% of vector peak (%.0f / %.0f GFLOP/s)"
                     % (eff, g, pk["peak_gflops"]))
        # only meaningful when compute is the limiter, not memory
        compute_bound = (membound or 0) < 30
        if eff < 15 and compute_bound:
            lines.append("    → FP throughput far below vector peak in compute-bound code: "
                         "likely scalar / under-vectorized (or a reference library).")
    if not lines:
        return
    out.append("\n══ Vectorization ══")
    out.extend(lines)
    _gloss(out, "FP efficiency")
    # candidates: hottest non-library compute kernels (library SIMD is the lib's job)
    fns = (profile or {}).get("functions", [])
    cand = sorted((f for f in fns if f.get("group") in ("USER", "ETC")
                   and f.get("t_excl", 0) > 0.005),
                  key=lambda f: -f.get("t_excl", 0))[:5]
    if cand:
        out.append("    inspect (hot user code): " + ", ".join(f.get("name", "")[:20] for f in cand))
    out.append("    confirm SIMD per loop with a compiler vec report "
               "(gcc -fopt-info-vec / icc -qopt-report=5) and build -O3 -march=native.")


# ------------------------------------------------------------- threading
def threading_view(snap, out):
    if not snap:
        return
    # At node scope the thread count / imbalance are the launcher's, not the ranks',
    # so per-process "parallel efficiency" is meaningless (and would read >100%).
    # Node CPU utilization is already reported in the snapshot/microarch sections.
    if _m(snap, "system_wide"):
        return
    threads = _m(snap, "max_threads")
    if not threads or threads < 2:
        return
    cores = _m(snap, "cpu_cores_used")
    active = _m(snap, "active_threads")
    timb = _m(snap, "thread_imbalance_pct")
    out.append("\n══ Threading ══")
    out.append("    %-22s %d" % ("threads (peak)", int(threads)))
    if active is not None:
        out.append("    %-22s %d of %d" % ("active threads", int(active), int(threads)))
    if cores is not None:
        out.append("    %-22s %.2f" % ("avg cores used", cores))
        out.append("    %-22s %.0f%%" % ("parallel efficiency", cores / threads * 100.0))
    if timb is not None:
        out.append("    %-22s %.0f%%  ((max-avg)/max of per-thread time)" % ("thread imbalance", timb))
        # Active-spin makes idle OpenMP threads busy-wait, so their /proc CPU time reads
        # as "busy" and this imbalance is only a LOWER BOUND (often ~0 when it's really
        # large). We can't separate spin from work in cputime — flag it instead.
        if _m(snap, "omp_spin_wait"):
            out.append("      ⚠ OpenMP active-spin (OMP_WAIT_POLICY≠passive): idle threads busy-wait,")
            out.append("        so this is a LOWER BOUND — re-run with OMP_WAIT_POLICY=passive for the")
            out.append("        true imbalance (and parallel efficiency above).")
