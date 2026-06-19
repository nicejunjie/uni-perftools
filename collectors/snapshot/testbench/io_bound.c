/* I/O-bound: write then re-read a temp file, flushing to the block layer so
 * disk_read/disk_write show up. Expected snapshot: low CPU, large I/O bytes. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>

int main(int argc, char **argv) {
    size_t mb = (argc > 1) ? (size_t)atol(argv[1]) : 256;
    const char *path = "testbench_io.tmp";
    char *buf = malloc(1024 * 1024);
    memset(buf, 0xab, 1024 * 1024);

    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return 1; }
    for (size_t i = 0; i < mb; i++) {
        if (write(fd, buf, 1024 * 1024) < 0) { perror("write"); return 1; }
    }
    fsync(fd);
    close(fd);

    /* Drop nothing we can't; just re-read to drive rchar at minimum. */
    fd = open(path, O_RDONLY);
    size_t total = 0; ssize_t r;
    while ((r = read(fd, buf, 1024 * 1024)) > 0) total += r;
    close(fd);
    unlink(path);

    printf("io_bound: bytes=%zu\n", total);
    free(buf);
    return 0;
}
