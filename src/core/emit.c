/* Writes this process's raw profile as <prefix>.<rank>.json. No analysis, no
 * cross-rank work, no MPI calls - aggregation/imbalance/formatting all happen in
 * the postprocess tool (tools/scilib-report.py) reading these files. */
#define _GNU_SOURCE
#include "libprof.h"
#include "report.h"
#include "config.h"
#include "util.h"
#include <stdio.h>
#include <stdlib.h>

__attribute__((weak)) int libprof_sample_enabled(void);
__attribute__((weak)) int libprof_sample_hz(void);

static void json_str(FILE *f, const char *s)
{
    fputc('"', f);
    for (; *s; s++) {
        if (*s == '"' || *s == '\\') fputc('\\', f);
        fputc(*s, f);
    }
    fputc('"', f);
}

/* ---- sampling: per-thread leaf-PC histograms + executable memory maps ----
 * Raw only; the postprocess tool symbolizes PCs to function/source line. */
struct samp_ctx { FILE *f; int first; uint64_t total, dropped; };

static void emit_thread_samples(libprof_tls_t *t, void *ud)
{
    struct samp_ctx *c = ud;
    c->total += t->samp_total;
    c->dropped += t->samp_dropped;
    if (!t->samp_pc) return;
    for (int i = 0; i < t->samp_cap; i++) {
        if (t->samp_cnt[i] == 0) continue;
        fprintf(c->f, "%s\n      [%llu, %u]", c->first ? "" : ",",
                (unsigned long long)t->samp_pc[i], t->samp_cnt[i]);
        c->first = 0;
    }
}

static void emit_maps(FILE *f)
{
    FILE *m = fopen("/proc/self/maps", "r");
    if (!m) return;
    char line[4096];
    int first = 1;
    while (fgets(line, sizeof(line), m)) {
        unsigned long start, end, off;
        char perms[8], path[4096];
        path[0] = 0;
        /* "start-end perms offset dev inode pathname" */
        if (sscanf(line, "%lx-%lx %7s %lx %*s %*s %4095[^\n]", &start, &end, perms, &off, path) < 4)
            continue;
        if (perms[2] != 'x') continue;            /* executable segments only */
        char *p = path; while (*p == ' ') p++;
        if (p[0] != '/') continue;                /* skip [vdso], anon, etc. */
        fprintf(f, "%s\n      {\"path\": ", first ? "" : ",");
        json_str(f, p);
        fprintf(f, ", \"start\": %lu, \"end\": %lu, \"off\": %lu}", start, end, off);
        first = 0;
    }
    fclose(m);
}

static void write_sampling(FILE *f)
{
    if (!libprof_sample_enabled || !libprof_sample_enabled()) return;
    fprintf(f, ",\n  \"sampling\": {\n    \"hz\": %d,\n    \"samples\": [",
            libprof_sample_hz ? libprof_sample_hz() : 0);
    struct samp_ctx c = { f, 1, 0, 0 };
    libprof_tls_foreach(emit_thread_samples, &c);
    fprintf(f, "\n    ],\n    \"total\": %llu,\n    \"dropped\": %llu,\n    \"maps\": [",
            (unsigned long long)c.total, (unsigned long long)c.dropped);
    emit_maps(f);
    fprintf(f, "\n    ]\n  }");
}

void libprof_write_raw(libprof_row_t *rows, int n, double apptime)
{
    int rank = get_MPI_rank();
    char path[1100];
    snprintf(path, sizeof(path), "%s.%d.json", libprof_cfg.prefix, rank);

    FILE *f = fopen(path, "w");
    if (!f) { perror("scilib-prof: fopen"); return; }

    char *exe = NULL;
    get_exe_path(&exe);

    fprintf(f, "{\n  \"rank\": %d,\n  \"application\": ", rank);
    json_str(f, exe ? exe : "");
    fprintf(f, ",\n  \"runtime_s\": %.6f,\n  \"nthreads\": %d,\n  \"functions\": [\n",
            apptime, libprof_tls_nthreads());
    free(exe);

    for (int i = 0; i < n; i++) {
        libprof_row_t *r = &rows[i];
        fprintf(f, "    {\"group\": ");
        json_str(f, r->group);
        fprintf(f, ", \"function\": ");
        json_str(f, r->name);
        fprintf(f, ", \"count\": %llu, \"t_incl\": %.9f, \"t_excl\": %.9f, \"bytes\": %llu}%s\n",
                (unsigned long long)r->count, r->t_incl, r->t_excl,
                (unsigned long long)r->bytes, i + 1 < n ? "," : "");
    }
    fprintf(f, "\n  ]");
    write_sampling(f);
    fprintf(f, "\n}\n");
    fclose(f);

    if (!libprof_cfg.quiet)
        fprintf(stderr, "[scilib-prof] wrote %s  (analyze: scilib-report %s.*.json)\n",
                path, libprof_cfg.prefix);
}
