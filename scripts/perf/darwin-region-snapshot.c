#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/proc_info.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

/*
 * A vmmap-compatible raw region source for restricted macOS runners.
 *
 * vmmap needs task_for_pid(), which CI sandboxes commonly deny.  libproc's
 * PROC_PIDREGIONPATHINFO still exposes the same resident-page counters and
 * mapped-file paths used by the code-page/RSS diagnostic.  Keep this tool raw:
 * the companion analysis records the categorisation rather than baking policy
 * into the sampler.
 */
int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s PID\n", argv[0]);
        return 2;
    }
    char *end = NULL;
    errno = 0;
    long value = strtol(argv[1], &end, 10);
    if (errno != 0 || end == argv[1] || *end != '\0' || value <= 0 ||
        value > INT_MAX) {
        fprintf(stderr, "invalid PID: %s\n", argv[1]);
        return 2;
    }

    const int pid = (int)value;
    const long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) {
        perror("sysconf(_SC_PAGESIZE)");
        return 3;
    }

    struct rusage_info_v0 usage;
    memset(&usage, 0, sizeof(usage));
    struct timespec captured;
    if (clock_gettime(CLOCK_REALTIME, &captured) != 0) {
        perror("clock_gettime(CLOCK_REALTIME)");
        return 4;
    }
    if (proc_pid_rusage(pid, RUSAGE_INFO_V0, (rusage_info_t *)&usage) != 0) {
        perror("proc_pid_rusage");
        return 4;
    }
    uint64_t capture_wall_ns = (uint64_t)captured.tv_sec * UINT64_C(1000000000) +
                               (uint64_t)captured.tv_nsec;
    printf("# pid=%d capture_wall_ns=%" PRIu64 " rss_bytes=%" PRIu64
           " footprint_bytes=%" PRIu64
           " user_cpu_ns=%" PRIu64 " system_cpu_ns=%" PRIu64 "\n",
           pid, capture_wall_ns, usage.ri_resident_size, usage.ri_phys_footprint,
           usage.ri_user_time, usage.ri_system_time);
    puts("address\tsize_bytes\tresident_bytes\tprivate_resident_bytes\tshared_resident_bytes\tdirtied_bytes\tprotection\tmax_protection\tuser_tag\tshare_mode\toffset\tpath");
    uint64_t cursor = 0;
    size_t region_count = 0;
    for (;;) {
        struct proc_regionwithpathinfo region;
        memset(&region, 0, sizeof(region));
        int result = proc_pidinfo(pid, PROC_PIDREGIONPATHINFO, cursor,
                                  &region, sizeof(region));
        if (result <= 0) {
            if (region_count == 0) {
                fprintf(stderr, "proc_pidinfo returned no regions for pid %d\n", pid);
                return 5;
            }
            break;
        }
        if ((size_t)result < sizeof(region)) {
            fprintf(stderr, "short proc_pidinfo result: %d\n", result);
            return 5;
        }

        const struct proc_regioninfo *info = &region.prp_prinfo;
        printf("0x%016" PRIx64 "\t%" PRIu64 "\t%" PRIu64 "\t%" PRIu64
               "\t%" PRIu64 "\t%" PRIu64 "\t%u\t%u\t%u\t%u\t%" PRIu64
               "\t%s\n",
               info->pri_address,
               info->pri_size,
               (uint64_t)info->pri_pages_resident * (uint64_t)page_size,
               (uint64_t)info->pri_private_pages_resident * (uint64_t)page_size,
               (uint64_t)info->pri_shared_pages_resident * (uint64_t)page_size,
               (uint64_t)info->pri_pages_dirtied * (uint64_t)page_size,
               info->pri_protection,
               info->pri_max_protection,
               info->pri_user_tag,
               info->pri_share_mode,
               info->pri_offset,
               region.prp_vip.vip_path);
        region_count++;

        uint64_t next = info->pri_address + info->pri_size;
        if (info->pri_size == 0 || next <= cursor) {
            break;
        }
        cursor = next;
    }
    return 0;
}
