/* MPI communication-volume analyzer + message-size histogram + point-to-point
 * communication matrix. mpi.h-free (see note below): handles are opaque values,
 * PMPI_Type_size is resolved by dlsym, so one binary works under OpenMPI/MPICH.
 *
 * Detail (histogram + per-peer matrix) is accumulated in module-local state under
 * a mutex and written to the per-rank JSON via the emitter hook; the postprocess
 * tool turns it into a size distribution and an NxN communication matrix. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include "analyzer.h"

typedef int (*type_size_fn)(void *datatype, int *size);

static int type_size(void *datatype)
{
    static type_size_fn fn;
    static int resolved;
    if (!resolved) { fn = (type_size_fn)dlsym(RTLD_DEFAULT, "PMPI_Type_size"); resolved = 1; }
    int sz = 0;
    if (fn && fn(datatype, &sz) == 0 && sz > 0) return sz;
    return 0;
}

/* ---- message-size histogram + p2p communication matrix (module state) ---- */
#define NBIN 12
static pthread_mutex_t L = PTHREAD_MUTEX_INITIALIZER;
static unsigned long long bins[NBIN];
static unsigned long long *sent, *recvd;   /* bytes per peer rank */
static int npeer;

static int size_bin(unsigned long long b)
{
    unsigned long long lim = 64; int i = 0;
    while (i < NBIN - 1 && b > lim) { lim <<= 2; i++; }
    return i;
}

static void grow(int peer)
{
    if (peer < npeer) return;
    int nn = peer + 1;
    sent = realloc(sent, nn * sizeof(*sent));
    recvd = realloc(recvd, nn * sizeof(*recvd));
    for (int i = npeer; i < nn; i++) { sent[i] = 0; recvd[i] = 0; }
    npeer = nn;
}

static void record(unsigned long long bytes, int dir, int peer)
{
    pthread_mutex_lock(&L);
    bins[size_bin(bytes)]++;
    if (dir && peer >= 0) { grow(peer); (dir > 0 ? sent : recvd)[peer] += bytes; }
    pthread_mutex_unlock(&L);
}

/* dir: +1 send (peer=dest), -1 recv (peer=src), 0 collective (no peer) */
static uint64_t volume(void **a, int ci, int ti)
{
    int count = *(int *)a[ci];
    int sz = type_size(*(void **)a[ti]);
    return (uint64_t)count * (uint64_t)sz;
}

static int an_send(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 1, 2); record(md->bytes, +1, *(int *)a[3]); return 0; }
static int an_recv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 1, 2); record(md->bytes, -1, *(int *)a[3]); return 0; }
static int an_bcast(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 1, 2); record(md->bytes, 0, -1); return 0; }
static int an_reduce(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 2, 3); record(md->bytes, 0, -1); return 0; }
static int an_coll(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 1, 2); record(md->bytes, 0, -1); return 0; }
static int an_scatterv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume(a, 5, 6); record(md->bytes, 0, -1); return 0; }

static void emit(void *fp)
{
    FILE *f = fp;
    fprintf(f, ",\n  \"mpi_detail\": {\n    \"bins\": [");
    for (int i = 0; i < NBIN; i++) fprintf(f, "%s%llu", i ? "," : "", bins[i]);
    fprintf(f, "],\n    \"sent\": [");
    for (int i = 0; i < npeer; i++) fprintf(f, "%s%llu", i ? "," : "", sent[i]);
    fprintf(f, "],\n    \"recv\": [");
    for (int i = 0; i < npeer; i++) fprintf(f, "%s%llu", i ? "," : "", recvd[i]);
    fprintf(f, "]\n  }");
}

static void bind(const char *name, libprof_analyzer_fn fn)
{
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (strcmp(libprof_desc[i].name, name) == 0) { libprof_desc[i].analyze = fn; return; }
}

void libprof_register_mpi_analyzers(void)
{
    bind("MPI_Send", an_send);   bind("MPI_Isend", an_send);
    bind("MPI_Recv", an_recv);   bind("MPI_Irecv", an_recv);
    bind("MPI_Bcast", an_bcast); bind("MPI_Ibcast", an_bcast);
    bind("MPI_Allreduce", an_reduce);  bind("MPI_Iallreduce", an_reduce);
    bind("MPI_Reduce", an_reduce);     bind("MPI_Ireduce", an_reduce);
    bind("MPI_Reduce_scatter_block", an_reduce);
    bind("MPI_Allgather", an_coll);    bind("MPI_Iallgather", an_coll);
    bind("MPI_Alltoall", an_coll);     bind("MPI_Ialltoall", an_coll);
    bind("MPI_Gather", an_coll);       bind("MPI_Igather", an_coll);
    bind("MPI_Scatter", an_coll);
    bind("MPI_Gatherv", an_coll);      bind("MPI_Allgatherv", an_coll);
    bind("MPI_Scatterv", an_scatterv);
    /* Sendrecv counts as a send to dest (arg 3) for the matrix */
    bind("MPI_Sendrecv", an_send);

    extern void libprof_register_emitter(libprof_emitter_fn);
    libprof_register_emitter(emit);
}
