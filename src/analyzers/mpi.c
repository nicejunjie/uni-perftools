/* MPI communication-volume analyzer: bytes = count * sizeof(datatype).
 *
 * Deliberately mpi.h-free for portability. OpenMPI and MPICH disagree on what
 * MPI_Datatype is (an 8-byte pointer vs a 4-byte int handle) and implement the
 * predefined constants differently; including mpi.h and referencing those is
 * exactly what breaks tools like mpiP across implementations. Instead we:
 *   - take the datatype as an opaque pointer-sized value passed through by the
 *     opaque-dialect wrapper, and
 *   - resolve PMPI_Type_size at runtime via dlsym and call it generically.
 * PMPI_Type_size reads the handle correctly for whichever MPI is loaded (the low
 * 32 bits for a MPICH int handle, the full pointer for OpenMPI). */
#define _GNU_SOURCE
#include <dlfcn.h>
#include "analyzer.h"
#include <string.h>

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

/* args[i] points AT the i-th (opaque, pointer-sized) argument slot. */
static int bytes_at(libprof_delta_t *md, void **a, int count_idx, int type_idx)
{
    int count = *(int *)a[count_idx];           /* element count (int slot) */
    void *dt  = *(void **)a[type_idx];           /* datatype handle, opaque */
    md->bytes = (uint64_t)count * (uint64_t)type_size(dt);
    return 0;
}

/* arg index of (count, datatype) varies by call family */
static int an_p2p(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; return bytes_at(md, a, 1, 2); }      /* Send/Recv/Bcast/Isend/Irecv */
static int an_reduce(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; return bytes_at(md, a, 2, 3); }      /* Allreduce/Reduce */
static int an_coll(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; return bytes_at(md, a, 1, 2); }      /* gather/alltoall (send count) */
static int an_scatterv(libprof_key_t *k, libprof_delta_t *md, const libprof_desc_t *d, void **a, void *r)
{ (void)k;(void)d;(void)r; return bytes_at(md, a, 5, 6); }      /* Scatterv (recv count/type) */

static void bind(const char *name, libprof_analyzer_fn fn)
{
    for (int i = 0; i < LIBPROF_NSLOTS; i++)
        if (strcmp(libprof_desc[i].name, name) == 0) { libprof_desc[i].analyze = fn; return; }
}

void libprof_register_mpi_analyzers(void)
{
    /* point-to-point + bcast: (count,datatype) at (1,2) */
    bind("MPI_Send", an_p2p);   bind("MPI_Recv", an_p2p);
    bind("MPI_Isend", an_p2p);  bind("MPI_Irecv", an_p2p);
    bind("MPI_Sendrecv", an_p2p);
    bind("MPI_Bcast", an_p2p);  bind("MPI_Ibcast", an_p2p);
    /* reductions: (count,datatype) at (2,3) */
    bind("MPI_Allreduce", an_reduce);  bind("MPI_Iallreduce", an_reduce);
    bind("MPI_Reduce", an_reduce);     bind("MPI_Ireduce", an_reduce);
    bind("MPI_Reduce_scatter_block", an_reduce);
    /* gather/scatter/alltoall + vector variants with a scalar send count at (1,2) */
    bind("MPI_Allgather", an_coll);   bind("MPI_Iallgather", an_coll);
    bind("MPI_Alltoall", an_coll);    bind("MPI_Ialltoall", an_coll);
    bind("MPI_Gather", an_coll);      bind("MPI_Igather", an_coll);
    bind("MPI_Scatter", an_coll);
    bind("MPI_Gatherv", an_coll);     bind("MPI_Allgatherv", an_coll);
    bind("MPI_Scatterv", an_scatterv);
    /* Alltoallv / Reduce_scatter (all-array counts) and Wait/Test/Barrier:
     * count + time only (no single scalar volume). */
}
