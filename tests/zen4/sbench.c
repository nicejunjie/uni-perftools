/* Serial DGEMM micro-benchmark for exercising the per-function roofline sampler
 * without MPI. Links the system reference/OpenBLAS dgemm. Usage: ./sbench [n] [reps] */
#include <stdlib.h>
#include <string.h>
extern void dgemm_(const char*,const char*,const int*,const int*,const int*,
  const double*,const double*,const int*,const double*,const int*,const double*,double*,const int*);
int main(int argc,char**argv){
  int n=(argc>1)?atoi(argv[1]):800, reps=(argc>2)?atoi(argv[2]):8;
  double *A=malloc(8.*n*n),*B=malloc(8.*n*n),*C=malloc(8.*n*n);
  for(int i=0;i<n*n;i++){A[i]=(i%7)*0.5;B[i]=(i%5)*0.25;} memset(C,0,8.*n*n);
  double one=1.0;
  for(int r=0;r<reps;r++) dgemm_("N","N",&n,&n,&n,&one,A,&n,B,&n,&one,C,&n);
  return (C[0]>1e30);
}
