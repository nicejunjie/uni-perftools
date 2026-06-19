#include "libprof.h"
#include "report.h"
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* ---- a simple growable (slot,shape)->metric map used to merge threads ---- */
typedef struct {
    uint16_t         slot;
    char            *shape;     /* NULL for tier-1 */
    uint16_t         shape_len;
    uint64_t         shape_hash;
    libprof_metric_t m;
    int              used;
} mrow_t;

typedef struct { mrow_t *s; size_t cap, len; } mmap_t;

static size_t mmix(uint16_t slot, uint64_t hash) { return (size_t)(hash ^ ((uint64_t)slot * 1099511628211ULL)); }

static mrow_t *mmap_find(mmap_t *map, uint16_t slot, const char *shape,
                         uint16_t shape_len, uint64_t shape_hash)
{
    if ((map->len + 1) * 4 >= map->cap * 3) {
        size_t ncap = map->cap ? map->cap * 2 : 256;
        mrow_t *ns = calloc(ncap, sizeof(mrow_t));
        for (size_t j = 0; j < map->cap; j++) {
            if (!map->s[j].used) continue;
            size_t i = mmix(map->s[j].slot, map->s[j].shape_hash) & (ncap - 1);
            while (ns[i].used) i = (i + 1) & (ncap - 1);
            ns[i] = map->s[j];
        }
        free(map->s);
        map->s = ns; map->cap = ncap;
    }
    size_t mask = map->cap - 1;
    size_t i = mmix(slot, shape_hash) & mask;
    while (map->s[i].used) {
        mrow_t *r = &map->s[i];
        if (r->slot == slot && r->shape_hash == shape_hash && r->shape_len == shape_len &&
            (shape_len == 0 || memcmp(r->shape, shape, shape_len) == 0))
            return r;
        i = (i + 1) & mask;
    }
    map->s[i].used = 1;
    map->s[i].slot = slot;
    map->s[i].shape_len = shape_len;
    map->s[i].shape_hash = shape_hash;
    if (shape_len) { map->s[i].shape = malloc(shape_len + 1);
                     memcpy(map->s[i].shape, shape, shape_len); map->s[i].shape[shape_len] = 0; }
    map->len++;
    return &map->s[i];
}

static void macc(libprof_metric_t *dst, const libprof_metric_t *src)
{
    dst->count += src->count; dst->t_incl += src->t_incl; dst->t_excl += src->t_excl;
    dst->bytes += src->bytes;
}

/* merge callbacks over the per-thread registry */
static void merge_thread(libprof_tls_t *t, void *ud);
static void merge_shaped(const libprof_key_t *k, const libprof_metric_t *m, void *ud);

int libprof_collect_local(libprof_row_t **out)
{
    mmap_t map = {0};
    map.cap = 256; map.s = calloc(map.cap, sizeof(mrow_t));

    libprof_tls_foreach(merge_thread, &map);

    /* materialize rows with count>0 */
    libprof_row_t *rows = calloc(map.len ? map.len : 1, sizeof(libprof_row_t));
    int n = 0;
    for (size_t j = 0; j < map.cap; j++) {
        if (!map.s[j].used || map.s[j].m.count == 0) continue;
        mrow_t *r = &map.s[j];
        libprof_row_t *o = &rows[n++];
        const libprof_desc_t *d = &libprof_desc[r->slot];
        if (r->shape_len)
            snprintf(o->name, sizeof(o->name), "%s[%s]", d->name, r->shape);
        else
            snprintf(o->name, sizeof(o->name), "%s", d->name);
        snprintf(o->group, sizeof(o->group), "%s", d->group);
        o->count = r->m.count;
        o->t_incl = r->m.t_incl; o->t_excl = r->m.t_excl;
        o->bytes = r->m.bytes;
        free(r->shape);
    }
    free(map.s);
    *out = rows;
    return n;
}

static void merge_thread(libprof_tls_t *t, void *ud)
{
    mmap_t *map = ud;
    for (int s = 0; s < LIBPROF_NSLOTS; s++) {
        if (t->by_slot[s].count == 0) continue;
        mrow_t *r = mmap_find(map, (uint16_t)s, NULL, 0, 0);
        macc(&r->m, &t->by_slot[s]);
    }
    libprof_shaped_foreach(t, merge_shaped, map);
}

static void merge_shaped(const libprof_key_t *k, const libprof_metric_t *m, void *ud)
{
    mmap_t *map = ud;
    mrow_t *r = mmap_find(map, k->slot, k->shape, k->shape_len, k->shape_hash);
    macc(&r->m, m);
}
