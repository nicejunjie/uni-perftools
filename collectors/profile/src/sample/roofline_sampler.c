/* Event-based sampling for per-FUNCTION roofline — the "characterize" pass.
 *
 * Per-thread perf_event_open on a set of FP-ops events and DRAM-access events in
 * SAMPLING mode. Each counter overflow delivers a realtime signal; the handler
 * reads the sampled instruction pointer from the mmap ring buffer and bumps a
 * per-thread, per-CHANNEL PC histogram. Symbolization and flop/byte attribution
 * happen in postprocess (PC -> function via the same maps the time sampler emits).
 *
 * Why sampling (not hook-and-delta): this attributes FP work and memory traffic
 * to ANY function — library, user, or system — including inlined code, with cost
 * independent of call frequency. See core/analysis docs.
 *
 * Data-driven, cross-vendor event selection: the events to sample are NOT
 * hard-coded here. The upat CLI resolves them from perf's vendored pmu-events db
 * (via `uaps resolve-events`) for the host CPU and passes them in UPAT_ROOFLINE_SPEC
 * as `role,type,config,period,scale` channels (role=fp|mem; type =
 * perf_event_attr.type — RAW on x86, a dynamic PMU type on ARM; scale = flops/op
 * for fp, bytes/sample for mem). This C side stays dumb — it just opens whatever
 * (type,config) it's told and weights the per-PC counts by `scale` at emit. If the spec is
 * absent (e.g. the resolver was unavailable), it falls back to the built-in AMD
 * codes so a stock AMD run still works. Enabled by UPAT_ROOFLINE=1.
 */
#define _GNU_SOURCE
#include "libprof.h"
#include "config.h"
#include <linux/perf_event.h>
#include <sys/syscall.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <signal.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <stdint.h>

#define RF_SIG     (SIGRTMIN + 7)   /* distinct from the time sampler's SIGRTMIN+6 */
#define RF_CAP     8192             /* distinct PCs per channel per thread */
#define RING_PAGES 8                /* data pages (power of two) */
#define MAX_CHAN   8                /* FP sub-events (≤4 widths on Intel) + DRAM events */

enum { ROLE_FP = 0, ROLE_MEM = 1 };

/* per-thread histograms, reachable from tls->roof for emit */
typedef struct {
    uint64_t *pc[MAX_CHAN];
    uint32_t *cnt[MAX_CHAN];
    int       cap;
    uint64_t  total[MAX_CHAN], dropped[MAX_CHAN];
} roof_hist_t;

/* per-thread perf state — only the owning thread and its handler touch these */
static __thread int          rf_fd[MAX_CHAN]   = { -1, -1, -1, -1, -1, -1, -1, -1 };
static __thread void        *rf_ring[MAX_CHAN] = { 0 };
static __thread roof_hist_t *rf_h;

/* channel table (shared, set once at init before any thread arms) */
static int          rf_enabled;
static volatile int rf_active;
static int          rf_nchan;
static int          rf_role[MAX_CHAN];     /* ROLE_FP | ROLE_MEM */
static uint32_t     rf_type[MAX_CHAN];     /* perf_event_attr.type (RAW on x86) */
static uint64_t     rf_config[MAX_CHAN];   /* raw perf_event_attr.config */
static uint64_t     rf_period[MAX_CHAN];   /* sample period */
static double       rf_scale[MAX_CHAN];    /* flops/op (FP) or bytes/sample (MEM) */

int libprof_roofline_enabled(void) { return rf_enabled; }

/* ---- channel setup --------------------------------------------------------- */
static void add_chan(int role, uint32_t type, uint64_t config, uint64_t period, double scale)
{
    if (rf_nchan >= MAX_CHAN || config == 0 || period == 0) return;
    rf_role[rf_nchan]   = role;
    rf_type[rf_nchan]   = type;
    rf_config[rf_nchan] = config;
    rf_period[rf_nchan] = period;
    rf_scale[rf_nchan]  = scale;
    rf_nchan++;
}

/* Parse UPAT_ROOFLINE_SPEC: ';'-separated channels, each "role,type,config,period,scale".
 * role is "fp" or "mem"; type/config/period accept 0x-hex or decimal; scale is a
 * double. Returns the number of channels parsed. */
static int parse_spec(const char *spec)
{
    char *buf = strdup(spec);
    if (!buf) return 0;
    for (char *save = NULL, *tok = strtok_r(buf, ";", &save); tok;
         tok = strtok_r(NULL, ";", &save)) {
        char role[8] = {0};
        char ty[32] = {0}, cfg[32] = {0}, per[32] = {0}, scl[32] = {0};
        /* role,type,config,period,scale */
        if (sscanf(tok, "%7[^,],%31[^,],%31[^,],%31[^,],%31s", role, ty, cfg, per, scl) != 5)
            continue;
        int r = (strcmp(role, "mem") == 0) ? ROLE_MEM : ROLE_FP;
        uint32_t type   = (uint32_t)strtoul(ty, NULL, 0);
        uint64_t config = strtoull(cfg, NULL, 0);
        uint64_t period = strtoull(per, NULL, 0);
        double   scale  = strtod(scl, NULL);
        add_chan(r, type, config, period, scale);
    }
    free(buf);
    return rf_nchan;
}

/* Built-in fallback when no spec is provided: AMD core-PMU FP + DRAM-fill events
 * (the historical hard-coded path). Other vendors have no fallback — they rely on
 * the resolver-provided spec, so without it roofline stays off (graceful). */
static int fallback_amd(void)
{
    char vendor[64] = {0}, line[256];
    FILE *f = fopen("/proc/cpuinfo", "r");
    if (f) {
        while (fgets(line, sizeof line, f)) {
            if (!strncmp(line, "vendor_id", 9)) {
                char *c = strchr(line, ':');
                if (c) sscanf(c + 2, "%63s", vendor);
                break;
            }
        }
        fclose(f);
    }
    if (strcmp(vendor, "AuthenticAMD") != 0) return 0;
    uint64_t fp_p  = libprof_cfg.roof_fp_period  > 0 ? libprof_cfg.roof_fp_period  : 1000000;
    uint64_t mem_p = libprof_cfg.roof_mem_period > 0 ? libprof_cfg.roof_mem_period : 10000;
    add_chan(ROLE_FP,  PERF_TYPE_RAW, 0x0F03, fp_p, 1.0);    /* FpRetSseAvxOps (ops proxy, 1 flop/op) */
    add_chan(ROLE_MEM, PERF_TYPE_RAW, 0x4843, mem_p, 64.0);  /* ls_dmnd_fills_from_sys.dram_io_all × line */
    return rf_nchan;
}

static int build_channels(void)
{
    const char *spec = getenv("UPAT_ROOFLINE_SPEC");
    if (spec && *spec && parse_spec(spec) > 0) return rf_nchan;
    return fallback_amd();
}

/* ---- ring buffer drain (signal context) ------------------------------------ */
static void ring_read(char *dst, const char *base, uint64_t size, uint64_t off, uint64_t len)
{
    off %= size;
    uint64_t first = size - off;
    if (first > len) first = len;
    memcpy(dst, base + off, first);
    if (len > first) memcpy(dst + first, base, len - first);
}

static void hist_bump(int ev, uint64_t pc)
{
    roof_hist_t *h = rf_h;
    if (!h || !h->pc[ev]) return;
    h->total[ev]++;
    uint64_t mask = (uint64_t)h->cap - 1, i = (pc >> 2) & mask;
    for (int p = 0; p < h->cap; p++) {
        if (h->cnt[ev][i] == 0) { h->pc[ev][i] = pc; h->cnt[ev][i] = 1; return; }
        if (h->pc[ev][i] == pc) { h->cnt[ev][i]++; return; }
        i = (i + 1) & mask;
    }
    h->dropped[ev]++;
}

static void drain(int ev)
{
    struct perf_event_mmap_page *meta = rf_ring[ev];
    if (!meta) return;
    long pg = sysconf(_SC_PAGESIZE);
    char *base = (char *)meta + pg;
    uint64_t size = (uint64_t)RING_PAGES * pg;
    uint64_t head = __atomic_load_n(&meta->data_head, __ATOMIC_ACQUIRE);
    uint64_t tail = meta->data_tail;
    while (tail < head) {
        struct perf_event_header hd;
        ring_read((char *)&hd, base, size, tail, sizeof hd);
        if (hd.size == 0) break;
        if (hd.type == PERF_RECORD_SAMPLE) {            /* layout: header, then u64 IP */
            uint64_t ip;
            ring_read((char *)&ip, base, size, tail + sizeof hd, sizeof ip);
            hist_bump(ev, ip);
        }
        tail += hd.size;
    }
    __atomic_store_n(&meta->data_tail, tail, __ATOMIC_RELEASE);
}

static void on_overflow(int sig, siginfo_t *si, void *uc)
{
    (void)sig; (void)uc;
    if (!rf_active) return;
    int fd = si->si_fd;
    for (int e = 0; e < rf_nchan; e++)
        if (fd == rf_fd[e]) { drain(e); return; }
}

/* ---- per-thread arm/disarm ------------------------------------------------- */
static long perf_open(uint32_t type, uint64_t config, uint64_t period, int tid)
{
    struct perf_event_attr a;
    memset(&a, 0, sizeof a);
    a.type = type;
    a.size = sizeof a;
    a.config = config;
    a.sample_period = period;
    a.sample_type = PERF_SAMPLE_IP;
    a.disabled = 1;
    a.exclude_kernel = 1;        /* userspace only: no privilege needed, PCs symbolizable */
    a.exclude_hv = 1;
    a.precise_ip = 2;            /* low skid where supported; degrade below */
    a.wakeup_events = 1;
    long fd = syscall(SYS_perf_event_open, &a, tid, -1, -1, 0);
    while (fd < 0 && a.precise_ip > 0) {        /* AMD core PMU: no PEBS — fall to skid */
        a.precise_ip--;
        fd = syscall(SYS_perf_event_open, &a, tid, -1, -1, 0);
    }
    return fd;
}

void libprof_roofline_thread_start(void)
{
    if (!rf_enabled || rf_fd[0] != -1) return;
    libprof_tls_t *t = libprof_tls ? libprof_tls : libprof_tls_init();
    if (!t) return;
    roof_hist_t *h = calloc(1, sizeof *h);
    if (!h) return;
    h->cap = RF_CAP;
    for (int e = 0; e < rf_nchan; e++) {
        h->pc[e]  = calloc(h->cap, sizeof(uint64_t));
        h->cnt[e] = calloc(h->cap, sizeof(uint32_t));
    }
    t->roof = h;
    rf_h = h;

    int tid = (int)syscall(SYS_gettid);
    long pg = sysconf(_SC_PAGESIZE);
    size_t mbytes = (size_t)(1 + RING_PAGES) * pg;
    for (int e = 0; e < rf_nchan; e++) {
        long fd = perf_open(rf_type[e], rf_config[e], rf_period[e], tid);
        if (fd < 0) continue;
        void *m = mmap(NULL, mbytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (m == MAP_FAILED) { close((int)fd); continue; }
        rf_ring[e] = m;
        rf_fd[e]   = (int)fd;
        fcntl((int)fd, F_SETFL, O_RDWR | O_NONBLOCK | O_ASYNC);
        fcntl((int)fd, F_SETSIG, RF_SIG);
        struct f_owner_ex own = { F_OWNER_TID, tid };
        fcntl((int)fd, F_SETOWN_EX, &own);
        ioctl((int)fd, PERF_EVENT_IOC_RESET, 0);
        ioctl((int)fd, PERF_EVENT_IOC_ENABLE, 0);
    }
}

void libprof_roofline_thread_stop(void)
{
    long pg = sysconf(_SC_PAGESIZE);
    size_t mbytes = (size_t)(1 + RING_PAGES) * pg;
    for (int e = 0; e < rf_nchan; e++) {
        if (rf_fd[e] >= 0) {
            ioctl(rf_fd[e], PERF_EVENT_IOC_DISABLE, 0);
            drain(e);
        }
        if (rf_ring[e]) { munmap(rf_ring[e], mbytes); rf_ring[e] = NULL; }
        if (rf_fd[e] >= 0) { close(rf_fd[e]); rf_fd[e] = -1; }
    }
}

/* ---- lifecycle ------------------------------------------------------------- */
void libprof_roofline_init(void)
{
    if (!libprof_cfg.roofline) return;
    if (build_channels() <= 0) return;   /* no resolvable events for this host → off */

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_overflow;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&sa.sa_mask);
    if (sigaction(RF_SIG, &sa, NULL) != 0) return;

    rf_enabled = 1;
    rf_active  = 1;
    libprof_roofline_thread_start();
}

void libprof_roofline_stop_all(void)
{
    rf_active = 0;
    libprof_roofline_thread_stop();
}

/* ---- emit the "roofline_sampling" JSON object (called from emit.c) --------- */
struct rfctx { FILE *f; int first; int ev; };

static void emit_one(libprof_tls_t *t, void *ud)
{
    struct rfctx *c = ud;
    roof_hist_t *h = t->roof;
    if (!h || !h->pc[c->ev]) return;
    for (int i = 0; i < h->cap; i++)
        if (h->cnt[c->ev][i]) {
            fprintf(c->f, "%s\n        [%llu,%u]", c->first ? "" : ",",
                    (unsigned long long)h->pc[c->ev][i], h->cnt[c->ev][i]);
            c->first = 0;
        }
}

static void sum_totals(libprof_tls_t *t, void *ud)
{
    uint64_t *tot = ud;                  /* tot[MAX_CHAN*2]: total, dropped per channel */
    roof_hist_t *h = t->roof;
    if (!h) return;
    for (int e = 0; e < rf_nchan; e++) { tot[e] += h->total[e]; tot[MAX_CHAN + e] += h->dropped[e]; }
}

static void emit_maps(FILE *f)
{
    FILE *m = fopen("/proc/self/maps", "r");
    if (!m) return;
    char line[4096];
    int first = 1;
    while (fgets(line, sizeof line, m)) {
        unsigned long start, end, off; char perms[8], path[4096]; path[0] = 0;
        if (sscanf(line, "%lx-%lx %7s %lx %*s %*s %4095[^\n]", &start, &end, perms, &off, path) < 4)
            continue;
        if (perms[2] != 'x') continue;
        char *p = path; while (*p == ' ') p++;
        if (p[0] != '/') continue;
        /* Escape the path as a JSON string: a raw %s breaks the whole prof.*.json
         * (silently dropping the run) if a mapped path contains a '"' or '\\' —
         * rare but legal. Mirrors sampler.c's emit_maps() / emit.c's json_str(). */
        fprintf(f, "%s\n      {\"path\":\"", first ? "" : ",");
        for (char *q = p; *q; q++) {
            if (*q == '"' || *q == '\\') fputc('\\', f);
            fputc(*q, f);
        }
        fprintf(f, "\",\"start\":%lu,\"end\":%lu,\"off\":%lu}", start, end, off);
        first = 0;
    }
    fclose(m);
}

void libprof_roofline_emit(void *fp)
{
    if (!rf_enabled) return;
    FILE *f = fp;
    uint64_t tot[MAX_CHAN * 2] = {0};
    libprof_tls_foreach(sum_totals, tot);

    /* One object per channel: role + raw config + period + scale + per-PC samples.
     * Postprocess weights each PC's count by (period × scale): flops for fp, bytes
     * for mem. Multiple fp channels (Intel's width-split umasks) sum per function. */
    fprintf(f, ",\n  \"roofline_sampling\": {\n    \"channels\": [");
    for (int e = 0; e < rf_nchan; e++) {
        fprintf(f, "%s\n      {\"role\":\"%s\", \"config\":\"0x%llx\", \"period\":%llu, "
                   "\"scale\":%g, \"total\":%llu,\n      \"samples\": [",
                e ? "," : "",
                rf_role[e] == ROLE_MEM ? "mem" : "fp",
                (unsigned long long)rf_config[e], (unsigned long long)rf_period[e],
                rf_scale[e], (unsigned long long)tot[e]);
        struct rfctx c = { f, 1, e };
        libprof_tls_foreach(emit_one, &c);
        fprintf(f, "\n      ]}");
    }
    fprintf(f, "\n    ],\n    \"maps\": [");
    emit_maps(f);
    fprintf(f, "\n    ]\n  }");
}
