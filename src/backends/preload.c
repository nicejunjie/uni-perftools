#define _GNU_SOURCE
#include <dlfcn.h>
#include "libprof.h"
#include "util.h"

static int skip;

/* dl backend: original symbol is the next one in the search order. */
void *libprof_resolve(libprof_desc_t *d)
{
    d->orig = dlsym(RTLD_NEXT, d->name);
    return d->orig;
}

__attribute__((constructor))
static void libprof_preload_ctor(void)
{
    char *exe = NULL;
    get_exe_path(&exe);
    skip = libprof_skip_exe(exe);
    free(exe);
    if (skip) return;
    libprof_init();
}

__attribute__((destructor))
static void libprof_preload_dtor(void)
{
    if (skip) return;
    libprof_finalize();
}
