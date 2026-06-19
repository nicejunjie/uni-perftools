#include "libprof.h"
#include <stdlib.h>
#include <string.h>

/* Open-addressing (linear probe) hashmap for shaped keys only. Hot calls with
 * no shape never reach here; this stays cold and small. */
typedef struct {
    libprof_key_t    key;   /* shape pointer is interned in the owning thread arena */
    libprof_metric_t m;
    int              used;
} slot_t;

struct libprof_hashmap {
    slot_t *slots;
    size_t  cap;     /* power of two */
    size_t  len;
};

uint64_t libprof_fnv1a(const void *data, size_t len)
{
    const unsigned char *p = data;
    uint64_t h = 1469598103934665603ULL;
    for (size_t i = 0; i < len; i++) { h ^= p[i]; h *= 1099511628211ULL; }
    return h;
}

/* Chunked bump allocator. Interned pointers are handed out into the current
 * chunk and must stay valid forever (the shaped hashmap stores them), so when a
 * chunk fills we allocate a NEW chunk and leave existing data in place rather
 * than realloc'ing (which would dangle every prior pointer). */
const char *libprof_intern(libprof_tls_t *t, const char *s, uint16_t len)
{
    size_t need = (size_t)len + 1;
    if (t->arena == NULL || t->arena_off + need > t->arena_cap) {
        size_t cap = t->arena_cap ? t->arena_cap : 64 * 1024;
        if (cap < need) cap = need;
        char *chunk = malloc(cap);
        if (!chunk) return NULL;
        t->arena = chunk;       /* old chunk intentionally retained, not freed */
        t->arena_cap = cap;
        t->arena_off = 0;
    }
    char *dst = t->arena + t->arena_off;
    memcpy(dst, s, len);
    dst[len] = '\0';
    t->arena_off += need;
    return dst;
}

static void hm_grow(struct libprof_hashmap *h);

static int key_eq(const libprof_key_t *a, const libprof_key_t *b)
{
    return a->slot == b->slot && a->shape_hash == b->shape_hash &&
           a->shape_len == b->shape_len &&
           memcmp(a->shape, b->shape, a->shape_len) == 0;
}

static size_t key_mix(const libprof_key_t *k)
{
    return (size_t)(k->shape_hash ^ ((uint64_t)k->slot * 1099511628211ULL));
}

libprof_metric_t *libprof_shaped_get(libprof_tls_t *t, const libprof_key_t *k)
{
    struct libprof_hashmap *h = t->shaped;
    if (!h) {
        h = calloc(1, sizeof(*h));
        h->cap = 64;
        h->slots = calloc(h->cap, sizeof(slot_t));
        t->shaped = h;
    }
    if ((h->len + 1) * 4 >= h->cap * 3) hm_grow(h);

    size_t mask = h->cap - 1;
    size_t i = key_mix(k) & mask;
    while (h->slots[i].used) {
        if (key_eq(&h->slots[i].key, k)) return &h->slots[i].m;
        i = (i + 1) & mask;
    }
    h->slots[i].used = 1;
    h->slots[i].key = *k;
    h->len++;
    return &h->slots[i].m;
}

static void hm_grow(struct libprof_hashmap *h)
{
    size_t ncap = h->cap * 2;
    slot_t *ns = calloc(ncap, sizeof(slot_t));
    size_t mask = ncap - 1;
    for (size_t j = 0; j < h->cap; j++) {
        if (!h->slots[j].used) continue;
        size_t i = key_mix(&h->slots[j].key) & mask;
        while (ns[i].used) i = (i + 1) & mask;
        ns[i] = h->slots[j];
    }
    free(h->slots);
    h->slots = ns;
    h->cap = ncap;
}

/* Iterate a thread's shaped rows (for merge at finalize). */
void libprof_shaped_foreach(libprof_tls_t *t,
                            void (*fn)(const libprof_key_t *, const libprof_metric_t *, void *),
                            void *ud)
{
    struct libprof_hashmap *h = t->shaped;
    if (!h) return;
    for (size_t j = 0; j < h->cap; j++)
        if (h->slots[j].used) fn(&h->slots[j].key, &h->slots[j].m, ud);
}
