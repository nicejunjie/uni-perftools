/* uaps MPI profiling shim.
 *
 * LD_PRELOAD this in front of an MPI program (e.g. via `uaps run --mpi`).
 * It uses the standard PMPI profiling interface: each MPI_Xxx here wraps the
 * real PMPI_Xxx, accumulating wall time spent inside MPI per call type. At
 * MPI_Finalize each rank writes "$UAPS_MPI_OUTDIR/rank_<rank>.txt", which the
 * Rust MpiCollector aggregates into MPI time / imbalance metrics.
 *
 * Build: mpicc -shared -fPIC -O2 uaps_mpi.c -o uaps_mpi.so
 */
#define _GNU_SOURCE
#include <mpi.h>
#include <stdio.h>
#include <stdlib.h>

typedef enum {
    F_SEND, F_RECV, F_SENDRECV, F_ISEND, F_IRECV, F_WAIT, F_WAITALL,
    F_BARRIER, F_BCAST, F_REDUCE, F_ALLREDUCE, F_GATHER, F_SCATTER, F_ALLTOALL,
    F_N
} fn_id;

static const char *g_names[F_N] = {
    "MPI_Send", "MPI_Recv", "MPI_Sendrecv", "MPI_Isend", "MPI_Irecv",
    "MPI_Wait", "MPI_Waitall", "MPI_Barrier", "MPI_Bcast", "MPI_Reduce",
    "MPI_Allreduce", "MPI_Gather", "MPI_Scatter", "MPI_Alltoall",
};

static double g_t[F_N];
static long g_c[F_N];
static double g_mpi_time = 0.0;
static double g_init_wall = 0.0;
static int g_rank = -1;

static inline void acc(fn_id id, double dt) {
    g_t[id] += dt;
    g_c[id] += 1;
    g_mpi_time += dt;
}

/* ----- lifecycle ----- */

int MPI_Init(int *argc, char ***argv) {
    int r = PMPI_Init(argc, argv);
    g_init_wall = PMPI_Wtime();
    PMPI_Comm_rank(MPI_COMM_WORLD, &g_rank);
    return r;
}

int MPI_Init_thread(int *argc, char ***argv, int required, int *provided) {
    int r = PMPI_Init_thread(argc, argv, required, provided);
    g_init_wall = PMPI_Wtime();
    PMPI_Comm_rank(MPI_COMM_WORLD, &g_rank);
    return r;
}

int MPI_Finalize(void) {
    double wall = PMPI_Wtime() - g_init_wall;
    const char *dir = getenv("UAPS_MPI_OUTDIR");
    if (dir && g_rank >= 0) {
        char path[4096];
        snprintf(path, sizeof path, "%s/rank_%d.txt", dir, g_rank);
        FILE *f = fopen(path, "w");
        if (f) {
            fprintf(f, "rank=%d\n", g_rank);
            fprintf(f, "wall=%.9f\n", wall);
            fprintf(f, "mpi_time=%.9f\n", g_mpi_time);
            for (int i = 0; i < F_N; i++) {
                if (g_c[i] > 0) {
                    fprintf(f, "fn=%s %.9f %ld\n", g_names[i], g_t[i], g_c[i]);
                }
            }
            fclose(f);
        }
    }
    return PMPI_Finalize();
}

/* ----- point-to-point ----- */

int MPI_Send(const void *buf, int count, MPI_Datatype dt, int dest, int tag, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Send(buf, count, dt, dest, tag, comm);
    acc(F_SEND, PMPI_Wtime() - t0);
    return r;
}

int MPI_Recv(void *buf, int count, MPI_Datatype dt, int src, int tag, MPI_Comm comm, MPI_Status *st) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Recv(buf, count, dt, src, tag, comm, st);
    acc(F_RECV, PMPI_Wtime() - t0);
    return r;
}

int MPI_Sendrecv(const void *sbuf, int scount, MPI_Datatype sdt, int dest, int stag,
                 void *rbuf, int rcount, MPI_Datatype rdt, int src, int rtag,
                 MPI_Comm comm, MPI_Status *st) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Sendrecv(sbuf, scount, sdt, dest, stag, rbuf, rcount, rdt, src, rtag, comm, st);
    acc(F_SENDRECV, PMPI_Wtime() - t0);
    return r;
}

int MPI_Isend(const void *buf, int count, MPI_Datatype dt, int dest, int tag, MPI_Comm comm, MPI_Request *req) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Isend(buf, count, dt, dest, tag, comm, req);
    acc(F_ISEND, PMPI_Wtime() - t0);
    return r;
}

int MPI_Irecv(void *buf, int count, MPI_Datatype dt, int src, int tag, MPI_Comm comm, MPI_Request *req) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Irecv(buf, count, dt, src, tag, comm, req);
    acc(F_IRECV, PMPI_Wtime() - t0);
    return r;
}

int MPI_Wait(MPI_Request *req, MPI_Status *st) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Wait(req, st);
    acc(F_WAIT, PMPI_Wtime() - t0);
    return r;
}

int MPI_Waitall(int count, MPI_Request reqs[], MPI_Status sts[]) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Waitall(count, reqs, sts);
    acc(F_WAITALL, PMPI_Wtime() - t0);
    return r;
}

/* ----- collectives ----- */

int MPI_Barrier(MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Barrier(comm);
    acc(F_BARRIER, PMPI_Wtime() - t0);
    return r;
}

int MPI_Bcast(void *buf, int count, MPI_Datatype dt, int root, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Bcast(buf, count, dt, root, comm);
    acc(F_BCAST, PMPI_Wtime() - t0);
    return r;
}

int MPI_Reduce(const void *sbuf, void *rbuf, int count, MPI_Datatype dt, MPI_Op op, int root, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Reduce(sbuf, rbuf, count, dt, op, root, comm);
    acc(F_REDUCE, PMPI_Wtime() - t0);
    return r;
}

int MPI_Allreduce(const void *sbuf, void *rbuf, int count, MPI_Datatype dt, MPI_Op op, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Allreduce(sbuf, rbuf, count, dt, op, comm);
    acc(F_ALLREDUCE, PMPI_Wtime() - t0);
    return r;
}

int MPI_Gather(const void *sbuf, int scount, MPI_Datatype sdt, void *rbuf, int rcount,
               MPI_Datatype rdt, int root, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Gather(sbuf, scount, sdt, rbuf, rcount, rdt, root, comm);
    acc(F_GATHER, PMPI_Wtime() - t0);
    return r;
}

int MPI_Scatter(const void *sbuf, int scount, MPI_Datatype sdt, void *rbuf, int rcount,
                MPI_Datatype rdt, int root, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Scatter(sbuf, scount, sdt, rbuf, rcount, rdt, root, comm);
    acc(F_SCATTER, PMPI_Wtime() - t0);
    return r;
}

int MPI_Alltoall(const void *sbuf, int scount, MPI_Datatype sdt, void *rbuf, int rcount,
                 MPI_Datatype rdt, MPI_Comm comm) {
    double t0 = PMPI_Wtime();
    int r = PMPI_Alltoall(sbuf, scount, sdt, rbuf, rcount, rdt, comm);
    acc(F_ALLTOALL, PMPI_Wtime() - t0);
    return r;
}
