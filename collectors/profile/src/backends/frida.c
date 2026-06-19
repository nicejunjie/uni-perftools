#include "frida-gum.h"
#include "libprof.h"
#include "util.h"
#include <stdlib.h>

static GumInterceptor *interceptor;
static gpointer       *hooks;
static int             skip;

/* frida backend: orig is filled by gum_interceptor_replace_fast at install. */
void *libprof_resolve(libprof_desc_t *d) { return d->orig; }

__attribute__((constructor))
static void libprof_frida_ctor(void)
{
    char *exe = NULL;
    get_exe_path(&exe);
    skip = libprof_skip_exe(exe);
    free(exe);
    if (skip) return;

    libprof_init();

    hooks = calloc(LIBPROF_NSLOTS, sizeof(gpointer));
    gum_init_embedded();
    interceptor = gum_interceptor_obtain();
    gum_interceptor_begin_transaction(interceptor);
    for (int i = 0; i < LIBPROF_NSLOTS; i++) {
        gpointer orig = gum_find_function(libprof_desc[i].name);
        gpointer wrap = gum_find_function(libprof_desc[i].wname);
        if (orig && wrap) {
            gum_interceptor_replace_fast(interceptor, orig, wrap,
                                         (gpointer *)&libprof_desc[i].orig);
            hooks[i] = orig;
        } else {
            hooks[i] = NULL;
        }
    }
    gum_interceptor_end_transaction(interceptor);
}

__attribute__((destructor))
static void libprof_frida_dtor(void)
{
    if (skip) return;
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (hooks[i]) gum_interceptor_revert(interceptor, hooks[i]);
    g_object_unref(interceptor);
    gum_deinit_embedded();
    libprof_finalize();
}
