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

/* Launcher rank env vars, in priority order. Must stay in sync with
 * core/contract/contract.py:rank_from_env — a launcher missing here makes every
 * rank fall back to 0 and clobber the same prof.0.json. SLURM_PROCID covers bare
 * `srun` (no PMI export); PALS_RANKID/ALPS_APP_PE cover HPE/Cray launchers. */
static const char *const RANK_ENV[] = {
    "OMPI_COMM_WORLD_RANK", "PMI_RANK", "MV2_COMM_WORLD_RANK", "PMIX_RANK",
    "SLURM_PROCID", "PALS_RANKID", "ALPS_APP_PE", NULL,
};

static const char *rank_env_value(void)
{
    /* Match the Rust (uaps) and Python (contract) detectors: SKIP a var that is unset,
     * empty, or not a valid integer, and fall through to the next key. A launcher that
     * exports e.g. OMPI_COMM_WORLD_RANK="" alongside a real SLURM_PROCID must not make
     * every rank parse as 0 here while the other two tiers pick SLURM — that divergence
     * would collide all ranks on prof.0.json and disagree with uaps/contract. */
    for (int i = 0; RANK_ENV[i]; i++) {
        const char *r = getenv(RANK_ENV[i]);
        if (r && *r) {
            char *end;
            (void)strtol(r, &end, 10);
            if (end != r && *end == '\0')
                return r;
        }
    }
    return NULL;
}

int check_MPI(void)
{
    return rank_env_value() != NULL;
}

/* Global MPI rank from the launcher environment (no MPI calls needed). */
int get_MPI_rank(void)
{
    const char *r = rank_env_value();
    return r ? atoi(r) : 0;
}
