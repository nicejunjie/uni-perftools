#define _GNU_SOURCE
#include "util.h"
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <limits.h>

void get_exe_path(char **path)
{
    char *p = malloc(PATH_MAX);
    ssize_t len = readlink("/proc/self/exe", p, PATH_MAX - 1);
    if (len != -1) { p[len] = '\0'; *path = p; }
    else           { free(p); *path = NULL; }
}

#define ARR(a) (sizeof(a) / sizeof((a)[0]))

int libprof_skip_exe(const char *str)
{
    if (!str) return 0;
    static const char *exe_list[] = {
        "ibrun","mpirun","orterun","orted","mpirun_rsh","mpiexec",
        "mpiexec.hydra","srun","hydra_bstrap_proxy","hydra_pmi_proxy",
        "numactl","pip","pip3","virtualenv",
        "map","scorep","nvprof","nsys","ncu",
    };
    static const char *dir_list[] = { "/bin", "/usr", "/sbin" };
    size_t slen = strlen(str);
    for (size_t i = 0; i < ARR(dir_list); i++) {
        size_t l = strlen(dir_list[i]);
        if (slen >= l && strncmp(str, dir_list[i], l) == 0) return 1;
    }
    for (size_t i = 0; i < ARR(exe_list); i++) {
        size_t l = strlen(exe_list[i]);
        if (slen >= l && strcmp(str + slen - l, exe_list[i]) == 0) return 1;
    }
    return 0;
}

int check_MPI(void)
{
    return getenv("PMI_RANK") || getenv("MV2_COMM_WORLD_RANK") ||
           getenv("OMPI_COMM_WORLD_RANK") || getenv("PMIX_RANK");
}

/* Global MPI rank from the launcher environment (no MPI calls needed). */
int get_MPI_rank(void)
{
    const char *r = getenv("OMPI_COMM_WORLD_RANK");
    if (!r) r = getenv("PMI_RANK");
    if (!r) r = getenv("MV2_COMM_WORLD_RANK");
    if (!r) r = getenv("PMIX_RANK");
    return r ? atoi(r) : 0;
}
