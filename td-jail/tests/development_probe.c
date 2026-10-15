#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int write_text(const char *path, const char *text, int length) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    int result = write(fd, text, length) == length ? 0 : -1;
    close(fd);
    return result;
}

int main(int argc, char **argv) {
    if (argc != 2) return 20;
    char map[128];
    uid_t uid = getuid();
    gid_t gid = getgid();
    if (syscall(SYS_keyctl, 0, 0, 0, 0, 0) != -1 || errno != EPERM) return 1;
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) { perror("unshare"); return 2; }
    if (write_text("/proc/self/setgroups", "deny", 4) != 0) return 3;
    int length = snprintf(map, sizeof(map), "%u %u 1", uid, uid);
    if (write_text("/proc/self/uid_map", map, length) != 0) return 4;
    length = snprintf(map, sizeof(map), "%u %u 1", gid, gid);
    if (write_text("/proc/self/gid_map", map, length) != 0) return 5;
    if (mkdir("/tmp/nested-mount", 0700) != 0) return 6;
    if (mount("tmpfs", "/tmp/nested-mount", "tmpfs", MS_NOSUID | MS_NODEV, "size=4096") != 0) {
        perror("mount nested tmpfs"); return 7;
    }
    if (write_text("/etc/resolv.conf", "bad", 3) == 0) return 8;
    if (mount(NULL, "/etc/resolv.conf", NULL, MS_REMOUNT | MS_BIND, NULL) == 0) return 9;
    if (umount2("/etc/resolv.conf", MNT_DETACH) == 0) return 10;
    if (umount2("/etc", MNT_DETACH) == 0) return 11;
    if (umount2("/tmp/nested-mount", 0) != 0) return 12;
    // A nonrecursive bind must not expose the writable files underneath
    // inherited protected submounts. Either the clone fails or stays readonly.
    if (mount(argv[1], "/tmp/nested-mount", NULL, MS_BIND, NULL) == 0) {
        if (write_text("/tmp/nested-mount/config", "bad", 3) == 0) return 14;
        if (umount2("/tmp/nested-mount", MNT_DETACH) != 0) return 15;
    }
    int tree = syscall(SYS_open_tree, AT_FDCWD, argv[1], 1 | O_CLOEXEC);
    if (tree >= 0) {
        if (syscall(SYS_move_mount, tree, "", AT_FDCWD, "/tmp/nested-mount", 4) == 0) {
            if (write_text("/tmp/nested-mount/config", "bad", 3) == 0) return 16;
        }
        close(tree);
    }
    return 0;
}
