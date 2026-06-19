/* POSIX I/O byte-volume analyzer. Bytes come from the actual return value
 * (read/write) or size*nmemb (fread/fwrite). Args/return are opaque slots. */
#include <stdint.h>
#include "analyzer.h"
#include <string.h>

static int an_rw(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *ret)
{
    (void)k; (void)d; (void)a;
    long n = (intptr_t)ret;                 /* actual bytes transferred */
    if (n > 0) md->bytes = (uint64_t)n;
    return 0;
}

static int an_frw(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *ret)
{
    (void)k; (void)d;
    long items = (intptr_t)ret;             /* fread/fwrite return item count */
    size_t size = *(size_t *)a[1];
    if (items > 0) md->bytes = (uint64_t)items * size;
    return 0;
}

static void bind(const char *name, libprof_analyzer_fn fn)
{
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (strcmp(libprof_desc[i].name, name) == 0) { libprof_desc[i].analyze = fn; return; }
}

void libprof_register_io_analyzers(void)
{
    bind("read", an_rw);   bind("write", an_rw);
    bind("pread", an_rw);  bind("pwrite", an_rw);
    bind("fread", an_frw); bind("fwrite", an_frw);
}
