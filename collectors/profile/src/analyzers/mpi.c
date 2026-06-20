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

/* ---- Fortran bindings (mpi_*_): args are by reference, datatype is a Fortran
 * integer handle. Convert it with MPI_Type_f2c, then reuse PMPI_Type_size. The
 * (count,datatype,peer) slot indices match the C calls. ---- */
typedef void *(*type_f2c_fn)(int);

static int type_size_f(int fhandle)
{
    static type_f2c_fn f2c;
    static int resolved;
    if (!resolved) { f2c = (type_f2c_fn)dlsym(RTLD_DEFAULT, "MPI_Type_f2c"); resolved = 1; }
    /* Skip non-positive Fortran handles: MPI_DATATYPE_NULL (0) / MPI_UNDEFINED
     * appear as the unused send-type in MPI_IN_PLACE collectives, and calling
     * MPI_Type_size on them trips the FATAL error handler and aborts the app. */
    if (!f2c || fhandle <= 0) return 0;
    return type_size(f2c(fhandle));
}

static uint64_t volume_f(void **a, int ci, int ti)
{
    int count = *(int *)a[ci];
    int sz = type_size_f(*(int *)a[ti]);
    return (uint64_t)count * (uint64_t)sz;
}

static int fan_send(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume_f(a, 1, 2); record(md->bytes, +1, *(int *)a[3]); return 0; }
static int fan_recv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume_f(a, 1, 2); record(md->bytes, -1, *(int *)a[3]); return 0; }
static int fan_coll(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume_f(a, 1, 2); record(md->bytes, 0, -1); return 0; }
static int fan_reduce(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume_f(a, 2, 3); record(md->bytes, 0, -1); return 0; }
static int fan_scatterv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; md->bytes = volume_f(a, 5, 6); record(md->bytes, 0, -1); return 0; }

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

    /* Fortran bindings (mpi_*_) — used by Fortran HPC codes (QE, VASP, ...) */
    bind("mpi_send_", fan_send);   bind("mpi_isend_", fan_send);
    bind("mpi_recv_", fan_recv);   bind("mpi_irecv_", fan_recv);
    bind("mpi_sendrecv_", fan_send);
    bind("mpi_bcast_", fan_coll);  bind("mpi_ibcast_", fan_coll);
    bind("mpi_allreduce_", fan_reduce);  bind("mpi_iallreduce_", fan_reduce);
    bind("mpi_reduce_", fan_reduce);     bind("mpi_ireduce_", fan_reduce);
    bind("mpi_allgather_", fan_coll);    bind("mpi_iallgather_", fan_coll);
    bind("mpi_alltoall_", fan_coll);     bind("mpi_ialltoall_", fan_coll);
    bind("mpi_gather_", fan_coll);       bind("mpi_scatter_", fan_coll);
    bind("mpi_gatherv_", fan_coll);      bind("mpi_allgatherv_", fan_coll);
    bind("mpi_scatterv_", fan_scatterv);

    extern void libprof_register_emitter(libprof_emitter_fn);
    libprof_register_emitter(emit);
}
