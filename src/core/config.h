#ifndef LIBPROF_CONFIG_H
#define LIBPROF_CONFIG_H

/* The library only emits raw per-rank data; all analysis/formatting/imbalance
 * options live in the postprocess tool (tools/scilib-report.py). */
typedef struct {
    int  debug;             /* SCILIB_DEBUG       (0..3) */
    int  shape;             /* SCILIB_SHAPE       1 => per-shape rows (size-resolved) */
    int  quiet;             /* SCILIB_QUIET       1 => no "wrote ..." note on stderr */
    int  sample;            /* SCILIB_SAMPLE      1 (default) => sampling profiler on */
    int  sample_hz;         /* SCILIB_SAMPLE_HZ   sampling rate (default 1000) */
    int  sample_cpu;        /* SCILIB_SAMPLE_CPU  1 => CPU-time clock instead of wall */
    char prefix[1024];      /* SCILIB_OUTPUT      output path prefix (default scilib-prof) */
} libprof_config_t;

extern libprof_config_t libprof_cfg;

void libprof_config_parse(void);

#endif
