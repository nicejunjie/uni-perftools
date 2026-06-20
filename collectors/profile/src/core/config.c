#define _GNU_SOURCE
#include "config.h"
#include <stdlib.h>
#include <string.h>

libprof_config_t libprof_cfg;

static int getenv_int(const char *name, int dflt)
{
    const char *v = getenv(name);
    return v && *v ? atoi(v) : dflt;
}

void libprof_config_parse(void)
{
    libprof_config_t *c = &libprof_cfg;
    c->debug = getenv_int("UPAT_DEBUG", 0);
    c->shape = getenv_int("UPAT_SHAPE", 0);
    c->quiet = getenv_int("UPAT_QUIET", 0);
    c->sample = getenv_int("UPAT_SAMPLE", 1);
    c->sample_hz = getenv_int("UPAT_SAMPLE_HZ", 1000);
    c->sample_cpu = getenv_int("UPAT_SAMPLE_CPU", 0);
    c->sample_stack = getenv_int("UPAT_SAMPLE_STACK", 64);
    if (c->sample_stack < 1) c->sample_stack = 1;
    if (c->sample_stack > 128) c->sample_stack = 128;
    c->heap = getenv_int("UPAT_HEAP", 0);
    if (c->sample_hz < 1) c->sample_hz = 1;
    if (c->sample_hz > 100000) c->sample_hz = 100000;

    c->roofline = getenv_int("UPAT_ROOFLINE", 0);          /* pass-2 characterize */
    c->roof_fp_period  = getenv_int("UPAT_ROOFLINE_FP_PERIOD", 1000000);
    c->roof_mem_period = getenv_int("UPAT_ROOFLINE_MEM_PERIOD", 10000);

    const char *out = getenv("UPAT_OUTPUT");
    snprintf(c->prefix, sizeof(c->prefix), "%s", (out && *out) ? out : "upat");
}
