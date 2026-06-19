#define _GNU_SOURCE
#include "libprof.h"
#include <stdlib.h>
#include <pthread.h>

__thread libprof_tls_t *libprof_tls = NULL;

/* Registry of every thread's state, so finalize can merge them. */
static libprof_tls_t  *tls_list = NULL;
static pthread_mutex_t tls_lock = PTHREAD_MUTEX_INITIALIZER;
static int             tls_count = 0;

#define STACK_CAP0  64
#define ARENA_CAP0  (64 * 1024)

libprof_tls_t *libprof_tls_init(void)
{
    libprof_tls_t *t = calloc(1, sizeof(*t));
    t->cap   = STACK_CAP0;
    t->stack = calloc(t->cap, sizeof(libprof_frame_t));
    t->arena_cap = ARENA_CAP0;
    t->arena = malloc(t->arena_cap);

    pthread_mutex_lock(&tls_lock);
    t->tid  = tls_count++;
    t->next = tls_list;
    tls_list = t;
    pthread_mutex_unlock(&tls_lock);

    libprof_tls = t;
    return t;
}

libprof_frame_t *libprof_grow_stack(libprof_tls_t *t)
{
    int newcap = t->cap * 2;
    libprof_frame_t *s = realloc(t->stack, newcap * sizeof(libprof_frame_t));
    if (!s) return &t->stack[t->cap - 1];   /* degrade: reuse last frame */
    t->stack = s;
    t->cap = newcap;
    return &t->stack[t->depth];
}

/* Visit every registered thread state (used by aggregate.c at finalize). */
void libprof_tls_foreach(void (*fn)(libprof_tls_t *, void *), void *ud)
{
    pthread_mutex_lock(&tls_lock);
    for (libprof_tls_t *t = tls_list; t; t = t->next) fn(t, ud);
    pthread_mutex_unlock(&tls_lock);
}
int libprof_tls_nthreads(void) { return tls_count; }
