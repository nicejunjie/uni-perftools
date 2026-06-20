/* Event-based sampling for per-FUNCTION roofline — the "characterize" pass.
 *
 * Per-thread perf_event_open on an FP-ops event and a DRAM-fill event in
 * SAMPLING mode. Each counter overflow delivers a realtime signal; the handler
 * reads the sampled instruction pointer from the mmap ring buffer and bumps a
 * per-thread, per-event PC histogram. Symbolization and flop/byte attribution
 * happen in postprocess (PC -> function via the same maps the time sampler emits).
 *
 * Why sampling (not hook-and-delta): this attributes FP work and memory traffic
 * to ANY function — library, user, or system — including inlined code, with cost
 * independent of call frequency. See core/analysis docs.
 *
 * Enabled by SCILIB_ROOFLINE=1. AMD core-PMU events for now; other vendors
 * degrade to "unsupported" (no roofline_sampling block emitted).
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
#define RF_CAP     8192             /* distinct PCs per event per thread */
#define RING_PAGES 8                /* data pages (power of two) */

enum { EV_FP = 0, EV_MEM = 1, EV_N = 2 };

/* per-thread histograms, reachable from tls->roof for emit */
typedef struct {
    uint64_t *pc[EV_N];
    uint32_t *cnt[EV_N];
    int       cap;
    uint64_t  total[EV_N], dropped[EV_N];
} roof_hist_t;

/* per-thread perf state — only the owning thread and its handler touch these */
static __thread int          rf_fd[EV_N]   = { -1, -1 };
static __thread void        *rf_ring[EV_N] = { 0, 0 };
static __thread roof_hist_t *rf_h;

static int          rf_enabled;
static volatile int rf_active;
static uint64_t     rf_config[EV_N];     /* raw event codes */
static uint64_t     rf_period[EV_N];
static int          rf_bytes_per_fill = 64;

int libprof_roofline_enabled(void) { return rf_enabled; }

/* ---- vendor event selection ------------------------------------------------ */
static int detect_events(void)
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
    if (!strcmp(vendor, "AuthenticAMD")) {
        rf_config[EV_FP]  = 0x0F03;   /* FpRetSseAvxOps  (matches snapshot gflops) */
        rf_config[EV_MEM] = 0x4843;   /* ls_dmnd_fills_from_sys: DRAM (matches mem_fills_dram) */
        return 1;
    }
    /* Intel needs width-weighted FP umasks + an offcore DRAM event — TODO. */
    return 0;
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
    for (int e = 0; e < EV_N; e++)
        if (fd == rf_fd[e]) { drain(e); return; }
}

/* ---- per-thread arm/disarm ------------------------------------------------- */
static long perf_open(uint64_t config, uint64_t period, int tid)
{
    struct perf_event_attr a;
    memset(&a, 0, sizeof a);
    a.type = PERF_TYPE_RAW;
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
    if (!rf_enabled || rf_fd[EV_FP] != -1 || rf_fd[EV_MEM] != -1) return;
    libprof_tls_t *t = libprof_tls ? libprof_tls : libprof_tls_init();
    if (!t) return;
    roof_hist_t *h = calloc(1, sizeof *h);
    if (!h) return;
    h->cap = RF_CAP;
    for (int e = 0; e < EV_N; e++) {
        h->pc[e]  = calloc(h->cap, sizeof(uint64_t));
        h->cnt[e] = calloc(h->cap, sizeof(uint32_t));
    }
    t->roof = h;
    rf_h = h;

    int tid = (int)syscall(SYS_gettid);
    long pg = sysconf(_SC_PAGESIZE);
    size_t mbytes = (size_t)(1 + RING_PAGES) * pg;
    for (int e = 0; e < EV_N; e++) {
        long fd = perf_open(rf_config[e], rf_period[e], tid);
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
    for (int e = 0; e < EV_N; e++) {
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
    if (!detect_events()) return;        /* unsupported vendor → silently off */
    rf_period[EV_FP]  = libprof_cfg.roof_fp_period  > 0 ? libprof_cfg.roof_fp_period  : 1000000;
    rf_period[EV_MEM] = libprof_cfg.roof_mem_period > 0 ? libprof_cfg.roof_mem_period : 10000;

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
            fprintf(c->f, "%s\n      [%llu,%u]", c->first ? "" : ",",
                    (unsigned long long)h->pc[c->ev][i], h->cnt[c->ev][i]);
            c->first = 0;
        }
}

static void sum_totals(libprof_tls_t *t, void *ud)
{
    uint64_t *tot = ud;                  /* tot[EV_N*2]: total, dropped per event */
    roof_hist_t *h = t->roof;
    if (!h) return;
    for (int e = 0; e < EV_N; e++) { tot[e] += h->total[e]; tot[EV_N + e] += h->dropped[e]; }
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
        fprintf(f, "%s\n      {\"path\":\"%s\",\"start\":%lu,\"end\":%lu,\"off\":%lu}",
                first ? "" : ",", p, start, end, off);
        first = 0;
    }
    fclose(m);
}

void libprof_roofline_emit(void *fp)
{
    if (!rf_enabled) return;
    FILE *f = fp;
    uint64_t tot[EV_N * 2] = {0};
    libprof_tls_foreach(sum_totals, tot);
    fprintf(f, ",\n  \"roofline_sampling\": {\n"
               "    \"fp_event\": \"0x%llx\", \"mem_event\": \"0x%llx\",\n"
               "    \"fp_period\": %llu, \"mem_period\": %llu, \"bytes_per_fill\": %d,\n"
               "    \"fp_total\": %llu, \"mem_total\": %llu,\n"
               "    \"fp_samples\": [",
            (unsigned long long)rf_config[EV_FP], (unsigned long long)rf_config[EV_MEM],
            (unsigned long long)rf_period[EV_FP], (unsigned long long)rf_period[EV_MEM],
            rf_bytes_per_fill,
            (unsigned long long)tot[EV_FP], (unsigned long long)tot[EV_MEM]);
    struct rfctx c = { f, 1, EV_FP };
    libprof_tls_foreach(emit_one, &c);
    fprintf(f, "\n    ],\n    \"mem_samples\": [");
    c.first = 1; c.ev = EV_MEM;
    libprof_tls_foreach(emit_one, &c);
    fprintf(f, "\n    ],\n    \"maps\": [");
    emit_maps(f);
    fprintf(f, "\n    ]\n  }");
}
