/* uaps MPI timing shim — mpi.h-FREE, portable across OpenMPI / MPICH / Cray MPI
 * and across C and Fortran codes.
 *
 * LD_PRELOAD-ed by `uaps run` when the target is an MPI launcher. It interposes
 * the common MPI calls (C `MPI_Xxx` AND Fortran `mpi_xxx_`), times each by
 * forwarding to the real `PMPI_Xxx` / `pmpi_xxx_` resolved with dlsym, and at
 * finalize writes `$UAPS_MPI_OUTDIR/rank_<n>.txt` (rank from the launcher's env).
 *
 * No <mpi.h>, no MPI constants, no mpicc — args are forwarded as opaque pointer
 * slots (ABI-correct on x86-64 SysV / aarch64 AAPCS), exactly like the upat
 * collector. This sidesteps the OpenMPI/MPICH handle-ABI mismatch that breaks
 * mpi.h-based shims, and catches Fortran apps (QE, VASP, ...) too.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

/* per-function accumulation (keyed by the literal display name) */
#define MAXFN 48
static const char *g_fn[MAXFN];
static double g_ft[MAXFN];
static long g_fc[MAXFN];
static int g_nfn;
static double g_mpi_time, g_init_wall;
static int g_started;
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;

static void acc(const char *name, double dt)
{
    pthread_mutex_lock(&g_lock);
    g_mpi_time += dt;
    for (int i = 0; i < g_nfn; i++)
        if (g_fn[i] == name) { g_ft[i] += dt; g_fc[i]++; pthread_mutex_unlock(&g_lock); return; }
    if (g_nfn < MAXFN) { g_fn[g_nfn] = name; g_ft[g_nfn] = dt; g_fc[g_nfn] = 1; g_nfn++; }
    pthread_mutex_unlock(&g_lock);
}

static int rank_from_env(void)
{
    const char *k[] = {"OMPI_COMM_WORLD_RANK", "PMI_RANK", "PMIX_RANK",
                       "SLURM_PROCID", "MV2_COMM_WORLD_RANK", "MPI_LOCALRANKID", 0};
    for (int i = 0; k[i]; i++) {
        const char *v = getenv(k[i]);
        if (v && *v)
            return atoi(v);
    }
    return 0;
}

static void start_timer(void)
{
    if (!g_started) { g_started = 1; g_init_wall = now(); }
}

static void write_report(void)
{
    double wall = g_started ? now() - g_init_wall : 0.0;
    const char *dir = getenv("UAPS_MPI_OUTDIR");
    if (!dir)
        return;
    int rank = rank_from_env();
    char path[4096];
    snprintf(path, sizeof path, "%s/rank_%d.txt", dir, rank);
    FILE *f = fopen(path, "w");
    if (!f)
        return;
    fprintf(f, "rank=%d\nwall=%.9f\nmpi_time=%.9f\n", rank, wall, g_mpi_time);
    for (int i = 0; i < g_nfn; i++)
        if (g_fc[i] > 0)
            fprintf(f, "fn=MPI_%s %.9f %ld\n", g_fn[i], g_ft[i], g_fc[i]);
    fclose(f);
}

/* ---- per-arity wrapper generators: C MPI_X(N args) + Fortran mpi_x_(N+1) ---- */
#define R(sym) ({ static void *p_; if (!p_) p_ = dlsym(RTLD_NEXT, sym); p_; })

#define CW1(X) int MPI_##X(void*a){double t=now();int(*p)(void*)=(void*)R("PMPI_" #X);int r=p?p(a):0;acc(#X,now()-t);return r;}
#define CW2(X) int MPI_##X(void*a,void*b){double t=now();int(*p)(void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b):0;acc(#X,now()-t);return r;}
#define CW3(X) int MPI_##X(void*a,void*b,void*c){double t=now();int(*p)(void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c):0;acc(#X,now()-t);return r;}
#define CW5(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e){double t=now();int(*p)(void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e):0;acc(#X,now()-t);return r;}
#define CW6(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e,void*f){double t=now();int(*p)(void*,void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e,f):0;acc(#X,now()-t);return r;}
#define CW7(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e,void*f,void*g){double t=now();int(*p)(void*,void*,void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e,f,g):0;acc(#X,now()-t);return r;}
#define CW8(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h){double t=now();int(*p)(void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e,f,g,h):0;acc(#X,now()-t);return r;}
#define CW9(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h,void*i){double t=now();int(*p)(void*,void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e,f,g,h,i):0;acc(#X,now()-t);return r;}
#define CW12(X) int MPI_##X(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h,void*i,void*j,void*k,void*l){double t=now();int(*p)(void*,void*,void*,void*,void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("PMPI_" #X);int r=p?p(a,b,c,d,e,f,g,h,i,j,k,l):0;acc(#X,now()-t);return r;}

#define FW2(X,l) void mpi_##l##_(void*a,void*b){double t=now();void(*p)(void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b);acc(#X,now()-t);}
#define FW3(X,l) void mpi_##l##_(void*a,void*b,void*c){double t=now();void(*p)(void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c);acc(#X,now()-t);}
#define FW4(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d){double t=now();void(*p)(void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d);acc(#X,now()-t);}
#define FW6(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f){double t=now();void(*p)(void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f);acc(#X,now()-t);}
#define FW7(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f,void*g){double t=now();void(*p)(void*,void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f,g);acc(#X,now()-t);}
#define FW8(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h){double t=now();void(*p)(void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f,g,h);acc(#X,now()-t);}
#define FW9(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h,void*i){double t=now();void(*p)(void*,void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f,g,h,i);acc(#X,now()-t);}
#define FW10(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h,void*i,void*j){double t=now();void(*p)(void*,void*,void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f,g,h,i,j);acc(#X,now()-t);}
#define FW13(X,l) void mpi_##l##_(void*a,void*b,void*c,void*d,void*e,void*f,void*g,void*h,void*i,void*j,void*k,void*l2,void*m){double t=now();void(*p)(void*,void*,void*,void*,void*,void*,void*,void*,void*,void*,void*,void*,void*)=(void*)R("pmpi_" #l "_");if(p)p(a,b,c,d,e,f,g,h,i,j,k,l2,m);acc(#X,now()-t);}

/* point-to-point */
CW6(Send)      FW7(Send, send)
CW7(Recv)      FW8(Recv, recv)
CW7(Isend)     FW8(Isend, isend)
CW7(Irecv)     FW8(Irecv, irecv)
CW12(Sendrecv) FW13(Sendrecv, sendrecv)
/* collectives */
CW5(Bcast)        FW6(Bcast, bcast)
CW6(Allreduce)    FW7(Allreduce, allreduce)
CW7(Reduce)       FW8(Reduce, reduce)
CW7(Allgather)    FW8(Allgather, allgather)
CW7(Alltoall)     FW8(Alltoall, alltoall)
CW8(Gather)       FW9(Gather, gather)
CW8(Scatter)      FW9(Scatter, scatter)
CW8(Allgatherv)   FW9(Allgatherv, allgatherv)
CW9(Alltoallv)    FW10(Alltoallv, alltoallv)
/* synchronization / completion */
CW1(Barrier)   FW2(Barrier, barrier)
CW2(Wait)      FW3(Wait, wait)
CW3(Waitall)   FW4(Waitall, waitall)

/* ---- lifecycle (start the wall clock; write the rank file at finalize) ---- */
int MPI_Init(void *a, void *b)
{
    int (*p)(void *, void *) = (void *)R("PMPI_Init");
    int r = p ? p(a, b) : 0;
    start_timer();
    return r;
}
int MPI_Init_thread(void *a, void *b, void *c, void *d)
{
    int (*p)(void *, void *, void *, void *) = (void *)R("PMPI_Init_thread");
    int r = p ? p(a, b, c, d) : 0;
    start_timer();
    return r;
}
void mpi_init_(void *ierr)
{
    void (*p)(void *) = (void *)R("pmpi_init_");
    if (p) p(ierr);
    start_timer();
}
void mpi_init_thread_(void *a, void *b, void *c)
{
    void (*p)(void *, void *, void *) = (void *)R("pmpi_init_thread_");
    if (p) p(a, b, c);
    start_timer();
}
int MPI_Finalize(void)
{
    write_report();
    int (*p)(void) = (void *)R("PMPI_Finalize");
    return p ? p() : 0;
}
void mpi_finalize_(void *ierr)
{
    write_report();
    void (*p)(void *) = (void *)R("pmpi_finalize_");
    if (p) p(ierr);
}
