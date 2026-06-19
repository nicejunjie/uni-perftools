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
    c->debug = getenv_int("SCILIB_DEBUG", 0);
    c->shape = getenv_int("SCILIB_SHAPE", 0);
    c->quiet = getenv_int("SCILIB_QUIET", 0);
    c->sample = getenv_int("SCILIB_SAMPLE", 1);
    c->sample_hz = getenv_int("SCILIB_SAMPLE_HZ", 1000);
    c->sample_cpu = getenv_int("SCILIB_SAMPLE_CPU", 0);
    c->sample_stack = getenv_int("SCILIB_SAMPLE_STACK", 64);
    if (c->sample_stack < 1) c->sample_stack = 1;
    if (c->sample_stack > 128) c->sample_stack = 128;
    c->heap = getenv_int("SCILIB_HEAP", 0);
    if (c->sample_hz < 1) c->sample_hz = 1;
    if (c->sample_hz > 100000) c->sample_hz = 100000;

    const char *out = getenv("SCILIB_OUTPUT");
    snprintf(c->prefix, sizeof(c->prefix), "%s", (out && *out) ? out : "scilib-prof");
}
