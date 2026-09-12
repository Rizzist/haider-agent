/* macOS resource identities for the opt-in native session-close audit.
 * pbi_nfiles is allocated capacity; PROC_PIDLISTFDS counts actual descriptors.
 * No target-process memory or file contents are read.
 */
#include <libproc.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/proc_info.h>

int main(int argc, char **argv) {
    if (argc != 2) return 64;
    char *end = NULL;
    long parsed = strtol(argv[1], &end, 10);
    if (!end || *end || parsed <= 0 || parsed > INT32_MAX) return 64;
    int pid = (int)parsed;
    struct proc_bsdinfo bsd;
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &bsd, sizeof bsd) != sizeof bsd)
        return 1;
    printf("allocated_fd_slots\t%u\n", bsd.pbi_nfiles);

    int bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, NULL, 0);
    if (bytes <= 0 || bytes > INT32_MAX - 4096) return 2;
    int capacity = bytes + 4096;
    struct proc_fdinfo *fds = calloc(1, capacity);
    if (!fds) return 3;
    bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds, capacity);
    if (bytes <= 0 || bytes >= capacity) {
        free(fds);
        return 4;
    }
    for (int i = 0; i < bytes / (int)sizeof(*fds); ++i) {
        struct vnode_fdinfowithpath path = {0};
        if (fds[i].proc_fdtype == PROX_FDTYPE_VNODE)
            proc_pidfdinfo(pid, fds[i].proc_fd, PROC_PIDFDVNODEPATHINFO,
                           &path, sizeof path);
        if (fds[i].proc_fdtype == PROX_FDTYPE_SOCKET) {
            struct socket_fdinfo socket = {0};
            if (proc_pidfdinfo(pid, fds[i].proc_fd, PROC_PIDFDSOCKETINFO,
                               &socket, sizeof socket) == sizeof socket
                && socket.psi.soi_kind == SOCKINFO_UN) {
                const struct sockaddr_un *address =
                    &socket.psi.soi_proto.pri_un.unsi_addr.ua_sun;
                snprintf(path.pvip.vip_path, sizeof path.pvip.vip_path,
                         "unix:%.*s", (int)sizeof address->sun_path,
                         address->sun_path);
            }
        }
        printf("fd\t%d\t%u\t%s\n", fds[i].proc_fd,
               fds[i].proc_fdtype, path.pvip.vip_path);
    }
    free(fds);

    uint64_t threads[2048];
    bytes = proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, threads, sizeof threads);
    if (bytes <= 0 || bytes >= (int)sizeof threads) return 5;
    for (int i = 0; i < bytes / (int)sizeof(*threads); ++i) {
        struct proc_threadinfo info = {0};
        if (proc_pidinfo(pid, PROC_PIDTHREADINFO, threads[i], &info,
                         sizeof info) != sizeof info) return 6;
        printf("thread\t%llu\t%s\n", threads[i], info.pth_name);
    }
    return 0;
}
