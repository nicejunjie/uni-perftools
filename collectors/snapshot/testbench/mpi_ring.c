/* MPI ring: each rank passes a token around the ring many times, plus a
 * deliberate imbalance (higher ranks sleep-spin) to exercise MPI wait time.
 * Build with mpicc. */
#include <stdio.h>
#include <stdlib.h>
#include <mpi.h>

int main(int argc, char **argv) {
    MPI_Init(&argc, &argv);
    int rank, size;
    MPI_Comm_rank(MPI_COMM_WORLD, &rank);
    MPI_Comm_size(MPI_COMM_WORLD, &size);

    long rounds = (argc > 1) ? atol(argv[1]) : 20000;
    int next = (rank + 1) % size;
    int prev = (rank - 1 + size) % size;
    long token = 0;

    for (long r = 0; r < rounds; r++) {
        /* Imbalance: rank 0 does extra compute before sending. */
        if (rank == 0) {
            double acc = 1.0;
            for (int k = 0; k < 20000; k++) { acc = acc * 1.0000001 + 0.5; acc -= 0.5; }
            token += (long)acc;
        }
        if (rank == 0) {
            MPI_Send(&token, 1, MPI_LONG, next, 0, MPI_COMM_WORLD);
            MPI_Recv(&token, 1, MPI_LONG, prev, 0, MPI_COMM_WORLD, MPI_STATUS_IGNORE);
        } else {
            MPI_Recv(&token, 1, MPI_LONG, prev, 0, MPI_COMM_WORLD, MPI_STATUS_IGNORE);
            MPI_Send(&token, 1, MPI_LONG, next, 0, MPI_COMM_WORLD);
        }
    }

    MPI_Barrier(MPI_COMM_WORLD);
    if (rank == 0) printf("mpi_ring: %d ranks, %ld rounds, token=%ld\n", size, rounds, token);
    MPI_Finalize();
    return 0;
}
