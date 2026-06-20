#ifndef LIBPROF_CONFIG_H
#define LIBPROF_CONFIG_H

/* The library only emits raw per-rank data; all analysis/formatting/imbalance
 * options live in the postprocess tool (tools/upat-report.py). */
typedef struct {
    int  debug;             /* UPAT_DEBUG       (0..3) */
    int  shape;             /* UPAT_SHAPE       1 => per-shape rows (size-resolved) */
    int  quiet;             /* UPAT_QUIET       1 => no "wrote ..." note on stderr */
    int  sample;            /* UPAT_SAMPLE      1 (default) => sampling profiler on */
    int  sample_hz;         /* UPAT_SAMPLE_HZ   sampling rate (default 1000) */
    int  sample_cpu;        /* UPAT_SAMPLE_CPU  1 => CPU-time clock instead of wall */
    int  sample_stack;      /* UPAT_SAMPLE_STACK  frames to unwind (1=leaf; default 64) */
    int  heap;              /* UPAT_HEAP        1 => track heap high-water mark */
    int  roofline;          /* UPAT_ROOFLINE    1 => per-function FP/DRAM event sampling */
    int  roof_fp_period;    /* UPAT_ROOFLINE_FP_PERIOD   FP-ops sample period */
    int  roof_mem_period;   /* UPAT_ROOFLINE_MEM_PERIOD  DRAM-fill sample period */
    char prefix[1024];      /* UPAT_OUTPUT      output path prefix (default upat) */
} libprof_config_t;

extern libprof_config_t libprof_cfg;

void libprof_config_parse(void);

#endif
