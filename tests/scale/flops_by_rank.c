/* Synthetic per-rank workload for uaps large-scale (oversubscription) testing.
 *
 * NOT a real MPI program — it just reads its rank from the launcher env and burns
 * a controllable number of FMAs, so `mpirun -n N` launches N independent copies.
 * That is all uaps's PER-RANK collection needs (each rank's process is counted on
 * its own node); we don't need mpicc or real MPI traffic to exercise it.
 *
 *   flops_by_rank <iters> [skew]
 *     iters : base FMA iterations per rank
 *     skew  : if "skew", odd ranks do 2x the work -> a KNOWN cpu-time/wall
 *             imbalance of ~(2-1.5)/2 = 25% across ranks; omitted -> homogeneous.
 */
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static int rank_from_env(void)
{
    const char *k[] = {"OMPI_COMM_WORLD_RANK", "PMI_RANK", "PMIX_RANK", "SLURM_PROCID", 0};
    for (int i = 0; k[i]; i++) {
        const char *v = getenv(k[i]);
        if (v && *v) return atoi(v);
    }
    return 0;
}

int main(int argc, char **argv)
{
    int rank = rank_from_env();
    long base = (argc > 1) ? atol(argv[1]) : 100000000L;
    int skew = (argc > 2 && strcmp(argv[2], "skew") == 0);
    long iters = skew ? base * (1 + (rank % 2)) : base; /* odd ranks 2x in skew mode */

    volatile double a = 1.0;
    double b = 0.9999999, c = 1e-7;
    for (long i = 0; i < iters; i++)
        a = a * b + c; /* 2 FLOPs/iter; converges, so no overflow */

    /* keep `a` live; never actually prints to the target's stdout */
    if (a > 1e30) fprintf(stderr, "unreachable %f\n", a);
    return 0;
}
