use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const SGCC: &str = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
const SBIN: &str = "{in:binutils-x86-64-self}/bin";
const XGLIBC: &str = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";

pub fn recipe() -> Recipe {
    let steps = vec![
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
        Step::Run {
            argv: vec![
                SGCC.into(),
                "-static".into(),
                "-isystem".into(),
                format!("{XGLIBC}/include"),
                "-B".into(),
                format!("{SBIN}/"),
                "-B".into(),
                format!("{XGLIBC}/lib"),
                "-L".into(),
                format!("{XGLIBC}/lib"),
                "-o".into(),
                "{out}/bin/td-jail-seccomp-probe".into(),
                "{root}/probe/seccomp.c".into(),
            ],
            env: vec![("PATH".into(), SBIN.into())],
            dir: "{root}/probe".into(),
        },
        Step::Require {
            paths: vec!["{out}/bin/td-jail-seccomp-probe".into()],
            exec: true,
        },
        Step::assert_static(&["{out}/bin/td-jail-seccomp-probe"]),
    ];

    Recipe::mesboot("td-jail-seccomp-probe", "1.0")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check td-jail-seccomp-probe: build the static td-GCC helper used by host and target td-jail seccomp oracles"
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
#include <sched.h>
#include <signal.h>
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
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define TD_SYS_IO_URING_SETUP 425
#define TD_SYS_CLONE3 435
#define TD_SYS_PIDFD_GETFD 438
#define TD_SYS_OPEN_TREE_ATTR 467
#define TD_X32_SYSCALL_BIT 0x40000000UL
#define TD_PROBE_MARKER "TD-JAIL-SECCOMP-PROBE-OK"
#define TD_ALLOW_INHERITED_CONFINEMENT "--allow-inherited-confinement"
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

int main(int argc, char **argv) {
    struct sock_fprog program;
    const char *filter_path;
    int allow_inherited_confinement = 0;
    int initial_nnp;
    int initial_seccomp;
    int fd;
    long result;
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

    errno = 0;
    if (expect_errno(syscall(-1L), ENOSYS, "negative syscall number") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall((long)INT32_MIN), ENOSYS, "second negative syscall number") != 0)
        return 1;
    result = syscall(SYS_personality, 0xffffffffUL);
    if (result < 0)
        return fail("personality query was denied");
    errno = 0;
    if (expect_errno(syscall(SYS_personality, READ_IMPLIES_EXEC), EPERM,
                     "personality mutation") != 0)
        return 1;

    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return fail("allowed AF_UNIX socket");
    close(fd);
    errno = 0;
    if (expect_errno(socket(AF_PACKET, SOCK_RAW, 0), EAFNOSUPPORT,
                     "disallowed socket family") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ptrace, PTRACE_TRACEME, 0, 0, 0), EPERM,
                     "ptrace") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, TIOCSTI, 0), EPERM, "TIOCSTI") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, (1UL << 32) | TIOCSTI, 0), EPERM,
                     "TIOCSTI high-bit bypass") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_ioctl, -1, TIOCLINUX, 0), EPERM, "TIOCLINUX") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0), EPERM,
                     "clone user namespace") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_CLONE3, 0, 0), ENOSYS, "clone3 fallback") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_unshare, 0), EPERM, "unshare") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_mount, 0, 0, 0, 0, 0), EPERM, "mount") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(TD_SYS_OPEN_TREE_ATTR, 0, 0, 0, 0), EPERM,
                     "open_tree_attr") != 0)
        return 1;
    errno = 0;
    if (expect_errno(syscall(SYS_userfaultfd, 0), EPERM, "userfaultfd") != 0)
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
    if (expect_errno(syscall(SYS_process_vm_readv, getpid(), 0, 0, 0, 0, 0), EPERM,
                     "process_vm_readv") != 0)
        return 1;
    if (expect_x32_kill() != 0)
        return 1;

    puts(TD_PROBE_MARKER);
    return 0;
}
"#
}
