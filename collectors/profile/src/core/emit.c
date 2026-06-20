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

__attribute__((weak)) void libprof_sample_emit(void *file);
__attribute__((weak)) void libprof_roofline_emit(void *file);

/* registry of extra per-rank JSON emitters (MPI comm matrix, heap, ...) */
#define LIBPROF_MAX_EMIT 8
static libprof_emitter_fn lp_emitters[LIBPROF_MAX_EMIT];
static int lp_n_emit;
void libprof_register_emitter(libprof_emitter_fn fn)
{
    if (lp_n_emit < LIBPROF_MAX_EMIT) lp_emitters[lp_n_emit++] = fn;
}
void libprof_emit_extras(void *file)
{
    for (int i = 0; i < lp_n_emit; i++) lp_emitters[i]((FILE *)file);
}

static void json_str(FILE *f, const char *s)
{
    fputc('"', f);
    for (; *s; s++) {
        if (*s == '"' || *s == '\\') fputc('\\', f);
        fputc(*s, f);
    }
    fputc('"', f);
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
    if (libprof_sample_emit) libprof_sample_emit(f);
    if (libprof_roofline_emit) libprof_roofline_emit(f);
    libprof_emit_extras(f);
    fprintf(f, "\n}\n");
    fclose(f);

    if (!libprof_cfg.quiet)
        fprintf(stderr, "[scilib-prof] wrote %s  (analyze: scilib-report %s.*.json)\n",
                path, libprof_cfg.prefix);
}
