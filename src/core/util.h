#ifndef LIBPROF_UTIL_H
#define LIBPROF_UTIL_H

void get_exe_path(char **path);    /* malloc'd; caller frees. NULL on failure */
int  libprof_skip_exe(const char *path);  /* 1 if exe is a launcher we ignore */
int  check_MPI(void);              /* 1 if running under an MPI launcher */
int  get_MPI_rank(void);           /* global rank from launcher env, or 0 */

#endif
