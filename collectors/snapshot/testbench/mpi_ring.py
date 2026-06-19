# mpi4py ring — Python MPI workload for validating `uaps run --mpi`.
#
# Uses stdlib `array` (no numpy needed). Uppercase Send/Recv map directly to
# the C-ABI MPI_Send / MPI_Recv, which the PMPI LD_PRELOAD shim intercepts —
# mpi4py being a runtime-imported extension does NOT prevent interception,
# because the calls bottom out in libmpi C symbols resolved via the global
# scope where LD_PRELOAD lives.
#
# Requires mpi4py built against the SAME MPI that mpirun launches (ABI match).
# Example (local venv, system OpenMPI):
#   python3 -m venv venv && MPICC=$(command -v mpicc) venv/bin/pip install mpi4py
#   uaps run --mpi -- mpirun --oversubscribe -n 4 venv/bin/python testbench/mpi_ring.py
from mpi4py import MPI
from array import array

comm = MPI.COMM_WORLD
rank = comm.Get_rank()
size = comm.Get_size()
nxt = (rank + 1) % size
prv = (rank - 1 + size) % size

token = array('q', [0])
rounds = 8000
for _ in range(rounds):
    if rank == 0:
        x = 0.0
        for _ in range(2000):          # imbalance: rank 0 does extra work
            x = x * 1.0000001 + 0.5
        token[0] += 1
        comm.Send([token, MPI.LONG_LONG], dest=nxt, tag=0)
        comm.Recv([token, MPI.LONG_LONG], source=prv, tag=0)
    else:
        comm.Recv([token, MPI.LONG_LONG], source=prv, tag=0)
        comm.Send([token, MPI.LONG_LONG], dest=nxt, tag=0)

comm.Barrier()
if rank == 0:
    print(f"mpi4py ring done: {size} ranks, {rounds} rounds, token={token[0]}")
