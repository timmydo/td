use crate::shared::{
    build_root_link_map_check, build_root_static_cc, LinkedLibc, BUILD_ROOT_PATH as PATH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-jail-seccomp-probe: a static C helper the host and target td-jail
// seccomp oracles run. It builds in the Debian build root: its gcc compiles
// it against td glibc, statically, with gcc-runtime's start files and
// libgcc, and the link map proves libc.a and libgcc.a are td's.
const PROBE_INPUTS: &[&str] = &["gcc-runtime"];

pub fn recipe() -> Recipe {
    let mut steps = vec![
        Step::MkDir {
            path: "{root}/probe".into(),
        },
        Step::WriteFile {
            path: "{root}/probe/seccomp.c".into(),
            content: source().into(),
            exec: false,
        },
        Step::MkDir {
            path: "{out}/bin".into(),
        },
    ];
    steps.extend(build_root_static_cc());
    // Compiled apart, so the link map loads the object from the build
    // directory rather than gcc's temporary.
    steps.push(
        Step::run(
            "{root}/probe",
            &[
                "{root}/wb/cc",
                "-c",
                "-o",
                "{root}/probe/seccomp.o",
                "seccomp.c",
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(
        Step::run(
            "{root}/probe",
            &[
                "{root}/wb/cc",
                "-o",
                "{out}/bin/td-jail-seccomp-probe",
                "{root}/probe/seccomp.o",
            ],
        )
        .env("PATH", PATH)
        .env("TD_LINK_MAP", "{root}/probe/seccomp.map"),
    );
    steps.push(build_root_link_map_check(
        "{root}/probe",
        "{root}/probe/seccomp.map",
        "td-jail-seccomp-probe",
        PROBE_INPUTS,
        LinkedLibc::Archive,
    ));
    steps.extend([
        Step::Require {
            paths: vec!["{out}/bin/td-jail-seccomp-probe".into()],
            exec: true,
        },
        Step::assert_static(&["{out}/bin/td-jail-seccomp-probe"]),
    ]);

    Recipe::mesboot("td-jail-seccomp-probe", "1.0")
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check td-jail-seccomp-probe: build the static helper, compiled in the Debian build root against td glibc, used by host and target td-jail seccomp oracles"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-seccomp-probe 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}

pub(crate) fn source() -> &'static str {
    r#"#define _GNU_SOURCE
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/personality.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TD_SYS_IO_URING_SETUP 425
#define TD_SYS_CLONE3 435
#define TD_SYS_PIDFD_GETFD 438
#define TD_SYS_OPEN_TREE_ATTR 467
#define TD_SYS_FCHROOT 472
#define TD_SYS_ABOVE_CEILING 473
#define TD_X32_SYSCALL_BIT 0x40000000UL
#define TD_PROBE_MARKER "TD-JAIL-SECCOMP-PROBE-OK"
#define TD_APPLICATION_MARKER "TD-JAIL-APPLICATION-FILTER-PROBE-OK"
#define TD_APPLICATION_FILTER "--application-filter"
#define TD_ALLOW_INHERITED_CONFINEMENT "--allow-inherited-confinement"
#define TD_PROBE_INHERITED_EXEC "--probe-inherited-exec"
#define TD_AUDIT_MARKER "--audit-marker"
#define TD_AUDIT_BEGIN "begin"
#define TD_AUDIT_END "end"
#define TD_HOST_SKIP_MARKER "TD-JAIL-SECCOMP-PROBE-HOST-SKIPPED"

struct td_filter_header {
    unsigned char magic[4];
    uint16_t len;
    uint16_t reserved;
};

_Static_assert(sizeof(struct sock_filter) == 8, "unexpected sock_filter ABI");
_Static_assert(sizeof(struct sock_fprog) == 16, "unexpected sock_fprog ABI");
_Static_assert(sizeof(struct td_filter_header) == 8, "unexpected filter header ABI");
_Static_assert(SECCOMP_SET_MODE_FILTER == 1, "unexpected seccomp operation");
_Static_assert(SECCOMP_FILTER_FLAG_LOG == 2, "unexpected seccomp log flag");
_Static_assert(PR_SET_NO_NEW_PRIVS == 38, "unexpected no-new-privileges operation");
_Static_assert(PR_GET_NO_NEW_PRIVS == 39, "unexpected no-new-privileges readback");
_Static_assert(CLONE_NEWUSER == 0x10000000, "unexpected clone namespace flag");
_Static_assert(TIOCSTI == 0x5412, "unexpected TIOCSTI value");
_Static_assert(TIOCLINUX == 0x541c, "unexpected TIOCLINUX value");

static int fail(const char *what) {
    fprintf(stderr, "td-jail-seccomp-probe: %s\n", what);
    return 1;
}

static int expect_errno(long result, int wanted, const char *what) {
    if (result != -1 || errno != wanted) {
        fprintf(stderr,
                "td-jail-seccomp-probe: %s returned %ld errno %d, expected -1 errno %d\n",
                what, result, errno, wanted);
        return -1;
    }
    return 0;
}

static int load_filter(const char *path, struct sock_fprog *program) {
    FILE *file = fopen(path, "rb");
    struct td_filter_header header;
    struct sock_filter *filter;
    int trailing;
    if (file == NULL)
        return fail("open compiled filter");
    if (fread(&header, sizeof(header), 1, file) != 1 ||
        memcmp(header.magic, "TDB1", 4) != 0 || header.reserved != 0 ||
        header.len == 0 || header.len > 4096) {
        fclose(file);
        return fail("invalid compiled filter header");
    }
    filter = calloc(header.len, sizeof(*filter));
    if (filter == NULL) {
        fclose(file);
        return fail("allocate compiled filter");
    }
    if (fread(filter, sizeof(*filter), header.len, file) != header.len) {
        free(filter);
        fclose(file);
        return fail("short compiled filter");
    }
    trailing = fgetc(file);
    if (trailing != EOF || ferror(file)) {
        free(filter);
        fclose(file);
        return fail("compiled filter has trailing bytes");
    }
    fclose(file);
    program->len = header.len;
    program->filter = filter;
    return 0;
}

static int wait_success(pid_t child, const char *what) {
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
        return fail(what);
    return 0;
}

static int prove_nnp_is_required(const struct sock_fprog *program, int initial_nnp,
                                 int allow_inherited_confinement) {
    pid_t child;
    if (initial_nnp == 1 && allow_inherited_confinement)
        return 0;
    if (initial_nnp != 0)
        return fail("initial no-new-privileges state prevented the negative probe");
    child = fork();
    if (child < 0)
        return fail("fork no-new-privileges negative probe");
    if (child == 0) {
        errno = 0;
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, program) == -1 &&
            errno == EACCES)
            _exit(0);
        _exit(1);
    }
    return wait_success(child, "filter installation without no-new-privileges did not fail EACCES");
}

static int read_status(int *nnp, int *seccomp) {
    FILE *file = fopen("/proc/self/status", "r");
    char line[256];
    *nnp = -1;
    *seccomp = -1;
    if (file == NULL)
        return fail("open /proc/self/status");
    while (fgets(line, sizeof(line), file) != NULL) {
        if (strcmp(line, "NoNewPrivs:\t0\n") == 0)
            *nnp = 0;
        if (strcmp(line, "NoNewPrivs:\t1\n") == 0)
            *nnp = 1;
        if (strcmp(line, "Seccomp:\t0\n") == 0)
            *seccomp = 0;
        if (strcmp(line, "Seccomp:\t1\n") == 0)
            *seccomp = 1;
        if (strcmp(line, "Seccomp:\t2\n") == 0)
            *seccomp = 2;
    }
    fclose(file);
    if (*nnp < 0 || *seccomp < 0)
        return fail("read initial no-new-privileges and seccomp state");
    return 0;
}

static int require_status(void) {
    int nnp;
    int seccomp;
    if (read_status(&nnp, &seccomp) != 0)
        return 1;
    if (nnp != 1 || seccomp != 2)
        return fail("kernel did not read back NoNewPrivs: 1 and Seccomp: 2");
    return 0;
}

static int prove_i386_boundary(int confined) {
    pid_t child = fork();
    int status;
    if (child < 0)
        return fail("fork i386 ABI probe");
    if (child == 0) {
        /* i386 exit(0): no pointers cross the compatibility boundary. */
        long number = 1;
        __asm__ volatile ("int $0x80" : "+a"(number) : "b"(0) : "memory", "cc");
        _exit(1);
    }
    if (waitpid(child, &status, 0) != child)
        return fail("wait for i386 ABI probe");
    if (confined) {
        if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSYS)
            return fail("i386 syscall was not killed with SIGSYS");
    } else if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail("the kernel cannot execute the i386 ABI: td-jail's probe needs IA32 emulation (DEVELOPMENT.md, Host kernel)");
    }
    return 0;
}

static int expect_x32_kill(void) {
    pid_t child = fork();
    int status;
    if (child < 0)
        return fail("fork x32 kill probe");
    if (child == 0) {
        syscall(TD_X32_SYSCALL_BIT | SYS_write, -1, 0, 0);
        _exit(1);
    }
    if (waitpid(child, &status, 0) != child || !WIFSIGNALED(status) ||
        WTERMSIG(status) != SIGSYS)
        return fail("x32 syscall was not killed with SIGSYS");
    return 0;
}

static int install_audit_marker_filter(uint32_t action) {
    struct sock_filter filter[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, action),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = {
        .len = sizeof(filter) / sizeof(filter[0]),
        .filter = filter,
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0)
        return fail("set no-new-privileges for audit marker");
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER,
                SECCOMP_FILTER_FLAG_LOG, &program) != 0)
        return fail("install audit marker filter");
    return 0;
}

static int emit_audit_marker(int begin) {
    pid_t child;
    int status;
    if (getuid() != 0 || getgid() != 0)
        return fail("audit marker requires root");
    if (begin) {
        child = fork();
        if (child < 0)
            return fail("fork audit begin marker");
        if (child == 0) {
            if (install_audit_marker_filter(SECCOMP_RET_KILL_PROCESS) != 0)
                _exit(1);
            syscall(SYS_getppid);
            _exit(1);
        }
        if (waitpid(child, &status, 0) != child || !WIFSIGNALED(status) ||
            WTERMSIG(status) != SIGSYS)
            return fail("audit begin marker was not killed with SIGSYS");
        return 0;
    }
    if (install_audit_marker_filter(SECCOMP_RET_ERRNO | EPERM) != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_getppid), EPERM, "audit end marker") != 0)
        return 1;
    return 0;
}

static int prove_denied_calls(void) {
    errno = 0;
    if (expect_errno(syscall(SYS_personality, READ_IMPLIES_EXEC), EPERM,
                     "personality mutation") != 0)
        return 1;
    errno = 0;
    if (expect_errno(socket(AF_PACKET, SOCK_RAW, 0), EAFNOSUPPORT,
                     "disallowed socket family") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0), EPERM,
                     "ptrace") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, TIOCSTI, 0), EPERM,
                     "TIOCSTI") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, (1UL << 32) | TIOCSTI, 0), EPERM,
                     "TIOCSTI high-bit bypass") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, TIOCLINUX, 0), EPERM,
                     "TIOCLINUX") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0),
                     EPERM, "clone user namespace") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_CLONE3, 0, 0), ENOSYS,
                     "clone3 fallback") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_unshare, 0), EPERM, "unshare") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_mount, 0, 0, 0, 0, 0), EPERM,
                     "mount") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_OPEN_TREE_ATTR, 0, 0, 0, 0), EPERM,
                     "open_tree_attr") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_FCHROOT, -1, 0), EPERM, "fchroot") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_ABOVE_CEILING, -1, 0), ENOSYS,
                     "above the reviewed ceiling") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_userfaultfd, 0), EPERM,
                     "userfaultfd") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_bpf, 0, 0, 0), EPERM, "bpf") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_IO_URING_SETUP, 1, 0), EPERM,
                     "io_uring_setup") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_PIDFD_GETFD, -1, -1, 0), EPERM,
                     "pidfd_getfd") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_process_vm_readv, getpid(), 0, 0, 0, 0, 0),
                     EPERM, "process_vm_readv") != 0)
        return 1;
    return expect_x32_kill();
}

/* getppid, unshare and FIONREAD succeed on any kernel without the filter, so
 * these answers can only be the filter's. The rest also hold it to its list,
 * though a kernel built without one of those calls would answer the same. */
static int expect_refused(long number, long a, long b, const char *what) {
    errno = 0;
    return expect_errno(syscall(number, a, b, 0, 0, 0, 0), ENOSYS, what);
}

static void *probe_thread(void *argument) { return argument; }

static int prove_application_filter(int mail, int web) {
    struct timeval timeout = {.tv_sec = 1, .tv_usec = 0};
    pthread_t thread;
    void *joined = NULL;
    char *spawn_argv[] = {(char *)"td-jail-probe", NULL};
    char *spawn_envp[] = {NULL};
    pid_t spawned;
    int fds[2];
    int one = 1;
    int fd;
    if (expect_refused(SYS_getppid, 0, 0, "getppid") != 0 ||
        expect_refused(SYS_unshare, 0, 0, "unshare") != 0 ||
        expect_refused(SYS_epoll_create1, 0, 0, "epoll_create1") != 0 ||
        expect_refused(SYS_eventfd2, 0, 0, "eventfd2") != 0 ||
        expect_refused(SYS_timerfd_create, CLOCK_MONOTONIC, 0, "timerfd_create") != 0 ||
        expect_refused(SYS_msgget, 0, 0, "msgget") != 0 ||
        expect_refused(SYS_shmget, 0, 0, "shmget") != 0 ||
        expect_refused(SYS_mq_unlink, 0, 0, "mq_unlink") != 0 ||
        expect_refused(SYS_setsid, 0, 0, "setsid") != 0 ||
        expect_refused(SYS_setuid, 0, 0, "setuid") != 0 ||
        expect_refused(SYS_personality, 0xffffffffL, 0, "personality query") != 0 ||
        expect_refused(SYS_bind, -1, 0, "bind") != 0 ||
        expect_refused(SYS_ptrace, PTRACE_TRACEME, 0, "ptrace") != 0 ||
        expect_refused(SYS_mount, 0, 0, "mount") != 0 ||
        expect_refused(TD_SYS_FCHROOT, -1, 0, "fchroot") != 0 ||
        expect_refused(TD_SYS_IO_URING_SETUP, 1, 0, "io_uring_setup") != 0 ||
        expect_refused(TD_SYS_CLONE3, 0, 0, "clone3") != 0 ||
        expect_refused(-1L, 0, 0, "negative number") != 0)
        return 1;
    errno = 0;
    if (expect_errno(socket(AF_INET, SOCK_STREAM, 0), EAFNOSUPPORT, "AF_INET") != 0)
        return 1;
    errno = 0;
    if (expect_errno(socket(AF_INET6, SOCK_STREAM, 0), EAFNOSUPPORT, "AF_INET6") != 0)
        return 1;
    errno = 0;
    if (expect_errno(socket(AF_NETLINK, SOCK_RAW, 0), EAFNOSUPPORT, "AF_NETLINK") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0),
                     EPERM, "clone user namespace") != 0)
        return 1;
    errno = 0;
    if (expect_errno(socketpair(AF_UNIX, SOCK_DGRAM, 0, fds), EPERM,
                     "datagram socketpair") != 0)
        return 1;
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, fds) != 0)
        return fail("stream socketpair was refused");
    if (ioctl(fds[0], FIONBIO, &one) != 0)
        return fail("FIONBIO was refused");
    if (setsockopt(fds[0], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0)
        return fail("SO_RCVTIMEO was refused");
    if (setsockopt(fds[0], SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)) != 0)
        return fail("SO_SNDTIMEO was refused");
    /* SO_REUSEADDR succeeds on a socket on any kernel, and an empty
     * SO_ATTACH_FILTER answers EINVAL: only the filter says ENOPROTOOPT. */
    errno = 0;
    if (expect_errno(setsockopt(fds[0], SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)),
                     ENOPROTOOPT, "SO_REUSEADDR") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_setsockopt, fds[0], SOL_SOCKET, SO_ATTACH_FILTER, 0, 0),
                     ENOPROTOOPT, "SO_ATTACH_FILTER") != 0)
        return 1;
    /* Each of these succeeds on a socket on any kernel. */
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, fds[0], FIOCLEX, 0), ENOTTY, "FIOCLEX") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, fds[0], FIONREAD, &one), ENOTTY, "FIONREAD") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, fds[0], (1UL << 32) | FIONREAD, &one), ENOTTY,
                     "FIONREAD high-bit bypass") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, TIOCSTI, 0), ENOTTY, "TIOCSTI") != 0)
        return 1;
    close(fds[0]);
    close(fds[1]);
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return fail("AF_UNIX socket was refused");
    close(fd);
    if (syscall(SYS_kill, getpid(), 0) != 0)
        return fail("kill was refused");
    /* glibc reaps a spawn whose exec failed with waitid: refuse it and nothing
     * fails here, but a zombie stays, charged to the jail's pid limit. */
    if (posix_spawn(&spawned, "/nonexistent/td-jail-probe", NULL, NULL, spawn_argv,
                    spawn_envp) == 0)
        return fail("a spawn of no program succeeded");
    errno = 0;
    if (expect_errno(waitpid(-1, NULL, WNOHANG), ECHILD, "failed spawn's reap") != 0)
        return 1;
    /* glibc starts a thread with clone3 first: that answers ENOSYS here, and
     * the clone it falls back to must start one. */
    if (pthread_create(&thread, NULL, probe_thread, &one) != 0 ||
        pthread_join(thread, &joined) != 0 || joined != &one)
        return fail("a thread could not start under the application filter");
    /* td-kv's fdatasync is mail's and news's: on no descriptor any kernel
     * answers EBADF, so ENOSYS can only be the web filter's. */
    if (web) {
        if (expect_refused(SYS_fdatasync, -1, 0, "fdatasync") != 0)
            return 1;
    } else {
        errno = 0;
        if (expect_errno(syscall(SYS_fdatasync, -1), EBADF, "fdatasync") != 0)
            return 1;
    }
    if (mail) {
        fd = (int)syscall(SYS_memfd_create, "td-jail-probe", 0);
        if (fd < 0)
            return fail("the mail filter refused memfd_create");
        close(fd);
    } else if (expect_refused(SYS_memfd_create, 0, 0, "memfd_create") != 0) {
        return 1;
    }
    return expect_x32_kill();
}

int main(int argc, char **argv) {
    struct sock_fprog program;
    const char *filter_path;
    int allow_inherited_confinement = 0;
    int application = 0;
    int mail = 0;
    int web = 0;
    int initial_nnp;
    int initial_seccomp;
    int fd;
    long result;
    if (argc == 3 && strcmp(argv[1], TD_AUDIT_MARKER) == 0) {
        if (strcmp(argv[2], TD_AUDIT_BEGIN) == 0)
            return emit_audit_marker(1);
        if (strcmp(argv[2], TD_AUDIT_END) == 0)
            return emit_audit_marker(0);
        return fail("audit marker must be begin or end");
    }
    if (argc >= 3 && strcmp(argv[1], TD_PROBE_INHERITED_EXEC) == 0) {
        if (require_status() != 0 || prove_denied_calls() != 0)
            return 1;
        execv(argv[2], &argv[2]);
        return fail("exec application after inherited filter probe");
    }
    if (argc >= 4 && strcmp(argv[1], TD_APPLICATION_FILTER) == 0) {
        application = 1;
        if (strcmp(argv[2], "mail") == 0)
            mail = 1;
        else if (strcmp(argv[2], "web") == 0)
            web = 1;
        else if (strcmp(argv[2], "news") != 0)
            return fail("application filter must be mail, news or web");
        argc -= 2;
        argv += 2;
    }
    if (argc == 3 && strcmp(argv[1], TD_ALLOW_INHERITED_CONFINEMENT) == 0) {
        allow_inherited_confinement = 1;
        filter_path = argv[2];
    } else if (argc == 2) {
        filter_path = argv[1];
    } else {
        return fail("usage: td-jail-seccomp-probe [--allow-inherited-confinement] FILTER");
    }
    if (load_filter(filter_path, &program) != 0)
        return 1;
    if (read_status(&initial_nnp, &initial_seccomp) != 0)
        return 1;
    if (initial_seccomp == 2 && allow_inherited_confinement) {
        free(program.filter);
        puts(TD_HOST_SKIP_MARKER);
        return 0;
    }
    if (initial_seccomp != 0)
        return fail("initial seccomp state prevented an isolated policy probe");
    if (prove_i386_boundary(0) != 0)
        return 1;
    if (prove_nnp_is_required(&program, initial_nnp,
                              allow_inherited_confinement) != 0)
        return 1;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
        prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1)
        return fail("set and read back no-new-privileges");
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program) != 0)
        return fail("install compiled seccomp filter");
    free(program.filter);
    if (require_status() != 0)
        return 1;
    if (application) {
        if (prove_application_filter(mail, web) != 0 || prove_i386_boundary(1) != 0)
            return 1;
        puts(TD_APPLICATION_MARKER);
        return 0;
    }

    errno = 0;
    if (expect_errno(syscall(-1L), ENOSYS, "negative syscall number") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall((long)INT32_MIN), ENOSYS, "second negative syscall number") != 0)
        return 1;
    result = syscall(SYS_personality, 0xffffffffUL);
    if (result < 0)
        return fail("personality query was denied");
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return fail("allowed AF_UNIX socket");
    close(fd);
    if (prove_denied_calls() != 0 || prove_i386_boundary(1) != 0)
        return 1;

    puts(TD_PROBE_MARKER);
    return 0;
}
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    // A kernel series this policy has not been read against reds here rather
    // than landing unreviewed: its new syscalls would answer ENOSYS, which is
    // safe, but nobody would have decided that. The values below are a
    // reviewer's statement that the table was read; no test can check that
    // without the kernel's tree, so raising one means doing what
    // APPLICATIONS.md, "The seccomp filter", says.
    #[test]
    fn the_pinned_kernel_is_within_the_series_seccomp_was_reviewed_against() {
        const REVIEWED_SERIES: (u32, u32) = (7, 3);
        const CEILING: u32 = 472;
        const HOLE: (u32, u32) = (337, 424);
        let seccomp = include_str!("../../../td-jail/src/seccomp.rs");
        assert!(seccomp.contains(&format!("const HIGHEST_REVIEWED_SYSCALL: u32 = {CEILING};")));
        assert!(seccomp.contains(&format!(
            "const FIRST_UNASSIGNED_SYSCALL: u32 = {};",
            HOLE.0
        )));
        assert!(seccomp.contains(&format!(
            "const FIRST_ASSIGNED_AFTER_HOLE: u32 = {};",
            HOLE.1
        )));
        assert!(source().contains(&format!("#define TD_SYS_ABOVE_CEILING {}\n", CEILING + 1)));
        let pins = include_str!("../source_pins.rs");
        let version = pins
            .split("key: \"linux-kernel-source\"")
            .nth(1)
            .and_then(|pin| pin.split("version: \"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .expect("the kernel pin names its version");
        let mut parts = version.split('.').map(|part| part.parse::<u32>().unwrap());
        let series = (parts.next().unwrap(), parts.next().unwrap());
        assert!(
            series <= REVIEWED_SERIES,
            "Linux {version} is newer than the {REVIEWED_SERIES:?} series seccomp was reviewed \
             against: review its syscall_64.tbl (APPLICATIONS.md, \"The seccomp filter\")"
        );
    }

    #[test]
    fn it_builds_in_the_root_against_td_glibc_and_gcc_runtime() {
        let recipe = recipe();
        crate::shared::assert_build_root_static_c(&recipe);
        crate::shared::assert_build_root_test(&recipe, &["glibc-x86-64", "gcc-runtime"], &[]);
        // The link writes its map, and the check that reads it admits
        // gcc-runtime alone beside the build's own files.
        let map = "{root}/probe/seccomp.map";
        let steps = recipe.steps.unwrap_or_default();
        let link = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, env, .. }
                    if argv.iter().any(|arg| arg == "{out}/bin/td-jail-seccomp-probe")
                        && env.iter().any(|(key, value)| key == "TD_LINK_MAP" && value == map))
            })
            .expect("the link writes its map");
        let check = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.iter().any(|arg| arg.contains(&format!("m='{map}'"))
                        && arg.contains("= 'td-jail-seccomp-probe'")
                        && arg.contains(r#""$(realpath -m '{src}')" "$(realpath -m '{in:gcc-runtime}')"; rt="#)))
            })
            .expect("the map is checked, admitting gcc-runtime alone");
        assert!(link < check);
    }

    #[test]
    fn inherited_mode_probes_the_installed_filter_then_execs_in_place() {
        let source = source();
        assert_eq!(source.matches("#define TD_PROBE_INHERITED_EXEC").count(), 1);
        assert_eq!(source.matches("execv(argv[2], &argv[2]);").count(), 1);
        assert_eq!(source.matches("prove_denied_calls() != 0").count(), 2);
        let inherited = source.find("if (argc >= 3").unwrap();
        let status = source[inherited..].find("require_status()").unwrap();
        let probes = source[inherited..].find("prove_denied_calls()").unwrap();
        let exec = source[inherited..]
            .find("execv(argv[2], &argv[2]);")
            .unwrap();
        assert!(status < probes && probes < exec);
        let denied = source
            .split_once("static int prove_denied_calls(void)")
            .unwrap()
            .1
            .split_once("/* getppid, unshare and FIONREAD succeed")
            .unwrap()
            .0;
        for call in [
            "SYS_personality",
            "AF_PACKET",
            "SYS_ptrace",
            "TIOCSTI",
            "TIOCLINUX",
            "SYS_clone",
            "TD_SYS_CLONE3",
            "SYS_unshare",
            "SYS_mount",
            "TD_SYS_OPEN_TREE_ATTR",
            "TD_SYS_FCHROOT",
            "TD_SYS_ABOVE_CEILING",
            "SYS_userfaultfd",
            "SYS_bpf",
            "TD_SYS_IO_URING_SETUP",
            "TD_SYS_PIDFD_GETFD",
            "SYS_process_vm_readv",
            "expect_x32_kill",
        ] {
            assert!(denied.contains(call), "inherited probe omitted {call}");
        }
        assert_eq!(denied.matches("SYS_ioctl").count(), 3);
        assert_eq!(denied.matches("expect_errno(").count(), 18);
        assert_eq!(denied.matches("return expect_x32_kill();").count(), 1);
    }

    #[test]
    fn the_application_mode_proves_witnesses_after_installing_the_named_filter() {
        let source = source();
        assert!(source.contains(&format!(
            "#define TD_APPLICATION_MARKER \"{}\"",
            crate::shared::TD_JAIL_APPLICATION_FILTER_PROBE_MARKER
        )));
        let main = source.split("int main(int argc").nth(1).unwrap();
        let install = main
            .find("syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0")
            .unwrap();
        let prove = main.find("prove_application_filter(mail, web)").unwrap();
        assert!(install < prove);
        let witnesses = source
            .split_once("static int prove_application_filter(int mail, int web)")
            .unwrap()
            .1
            .split_once("int main(")
            .unwrap()
            .0;
        // Answers no kernel gives by itself for these calls, so the filter
        // must be what gave them.
        for witness in [
            "SYS_getppid",
            "SYS_unshare",
            "SYS_epoll_create1",
            "SYS_setsid",
            "FIONREAD",
            "(1UL << 32) | FIONREAD",
            "SO_ATTACH_FILTER",
            "SO_REUSEADDR",
            "failed spawn's reap",
            "AF_INET6",
            "AF_NETLINK",
            "SOCK_DGRAM",
            "SYS_memfd_create",
            "SYS_fdatasync",
            "pthread_create",
            "expect_x32_kill",
        ] {
            assert!(
                witnesses.contains(witness),
                "application probe omitted {witness}"
            );
        }
    }

    #[test]
    fn standalone_probe_requires_i386_execution_before_filter_and_kill_after() {
        let source = source();
        let main = source.split("int main(int argc").nth(1).unwrap();
        let before = main.find("prove_i386_boundary(0)").unwrap();
        let install = main
            .find("syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0")
            .unwrap();
        let after = main.find("prove_i386_boundary(1)").unwrap();
        assert!(before < install && install < after);
        assert!(source.contains("WIFEXITED(status) || WEXITSTATUS(status) != 0"));
        assert!(source.contains("WIFSIGNALED(status) || WTERMSIG(status) != SIGSYS"));
        let inherited = source
            .split("if (argc >= 3")
            .nth(1)
            .unwrap()
            .split("if (argc == 3")
            .next()
            .unwrap();
        assert!(!inherited.contains("prove_i386_boundary"));
    }

    #[test]
    fn audit_barriers_use_one_logged_syscall_with_distinct_actions() {
        let source = source();
        for required in [
            "SECCOMP_FILTER_FLAG_LOG, &program",
            "SYS_getppid",
            "install_audit_marker_filter(SECCOMP_RET_KILL_PROCESS)",
            "install_audit_marker_filter(SECCOMP_RET_ERRNO | EPERM)",
            "getuid() != 0 || getgid() != 0",
        ] {
            assert!(
                source.contains(required),
                "audit barrier omitted {required}"
            );
        }
        assert_eq!(source.matches("return emit_audit_marker(").count(), 2);
    }
}
