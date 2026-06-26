/* Hybrid MPI + OpenMP validation workload for uaps per-thread HW counting.
 *
 * Real MPI program (compile with mpicc -fopenmp). Each OpenMP thread burns a
 * controllable, FIXED amount of FP work (FMAs), so the TOTAL job work is
 *   nranks * nthreads * reps * iters
 * and is INVARIANT to how you split ranks-vs-threads (R*T held constant). That
 * is the headline invariant for "does uaps sum ALL OpenMP threads": a fixed
 * total of FP/integer work run as R ranks x T threads must aggregate to the same
 * hw_instructions as (R*T) ranks x 1 thread.
 *
 * MPI_THREAD_FUNNELED: only the main thread calls MPI (Allreduce per rep), so the
 * PMPI shim sees per-rank MPI time even with threads present.
 *
 *   hybrid <iters> <reps> [mode]
 *     iters : FMA iterations PER THREAD per rep (fixed, not divided by nthreads)
 *     reps  : number of parallel regions (each region ~= one barrier); controls
 *             region granularity for the short-region miss test
 *     mode  : balanced   (default) every thread does `iters`
 *             imbalanced  thread t does iters*2*(t+1)/nt  (~44% thread imbalance)
 *             mpiskew     rank 0 does 4x the reps of compute -> other ranks wait
 *                         in Allreduce -> cross-rank MPI imbalance
 */
#include <mpi.h>
#include <omp.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static double burn(long n, int seed)
{
    volatile double a = 1.0 + seed * 1e-9;
    double b = 0.9999999, c = 1e-7;
    for (long i = 0; i < n; i++)
        a = a * b + c; /* 2 FLOPs/iter; converges, never overflows */
    return (double)a;
}

int main(int argc, char **argv)
{
    int provided = 0;
    MPI_Init_thread(&argc, &argv, MPI_THREAD_FUNNELED, &provided);
    int rank = 0, size = 1;
    MPI_Comm_rank(MPI_COMM_WORLD, &rank);
    MPI_Comm_size(MPI_COMM_WORLD, &size);

    long iters = (argc > 1) ? atol(argv[1]) : 50000000L;
    long reps  = (argc > 2) ? atol(argv[2]) : 1L;
    const char *mode = (argc > 3) ? argv[3] : "balanced";
    int imbalanced = strcmp(mode, "imbalanced") == 0;
    int mpiskew    = strcmp(mode, "mpiskew") == 0;

    /* mpiskew: rank 0 does 3x the per-thread work each rep, so it is the straggler
     * the other ranks block on inside Allreduce -> cross-rank MPI-time imbalance. */
    long base_iters = (mpiskew && rank == 0) ? iters * 3 : iters;

    double acc = 0.0;
    for (long r = 0; r < reps; r++) {
#pragma omp parallel reduction(+ : acc)
        {
            int t = omp_get_thread_num();
            int nt = omp_get_num_threads();
            long w = base_iters;
            if (imbalanced)
                w = iters * 2 * (t + 1) / nt; /* 0.25x .. 2x of iters */
            acc += burn(w, t);
        }
        /* FUNNELED: only the main thread communicates. One collective per rep. */
        double g = 0.0;
        MPI_Allreduce(&acc, &g, 1, MPI_DOUBLE, MPI_SUM, MPI_COMM_WORLD);
        acc += g * 0.0; /* keep `acc` and `g` live without changing it */
    }

    if (acc == 1234567.89) /* unreachable: defeats dead-code elimination */
        printf("rank %d size %d provided %d acc %f\n", rank, size, provided, acc);

    MPI_Finalize();
    return 0;
}
