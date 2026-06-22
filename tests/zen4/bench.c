#include <stdlib.h>
#include <string.h>
#include <mpi.h>
/* system reference BLAS dgemm (Fortran ABI) */
extern void dgemm_(const char*,const char*,const int*,const int*,const int*,
  const double*,const double*,const int*,const double*,const int*,
  const double*,double*,const int*);
int main(int argc,char**argv){
  MPI_Init(&argc,&argv);
  int rank,sz; MPI_Comm_rank(MPI_COMM_WORLD,&rank); MPI_Comm_size(MPI_COMM_WORLD,&sz);
  int n   = (argc>1)?atoi(argv[1]):700;
  int reps= (argc>2)?atoi(argv[2]):8;
  double *A=malloc(sizeof(double)*n*n),*B=malloc(sizeof(double)*n*n),*C=malloc(sizeof(double)*n*n);
  for(int i=0;i<n*n;i++){A[i]=(i%7)*0.5;B[i]=(i%5)*0.25;} memset(C,0,sizeof(double)*n*n);
  double one=1.0;
  for(int r=0;r<reps;r++){
    dgemm_("N","N",&n,&n,&n,&one,A,&n,B,&n,&one,C,&n);
    double s=C[0]; MPI_Allreduce(MPI_IN_PLACE,&s,1,MPI_DOUBLE,MPI_SUM,MPI_COMM_WORLD);
    MPI_Barrier(MPI_COMM_WORLD);
  }
  double t=C[n]; MPI_Bcast(&t,1,MPI_DOUBLE,0,MPI_COMM_WORLD);
  MPI_Finalize(); return 0;
}
