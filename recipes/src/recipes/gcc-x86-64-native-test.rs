use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const NGCC: &str = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
const NGPP: &str = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/g++";
const NBIN: &str = "{in:binutils-x86-64-native}/bin";
const XGLIBC: &str = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";

pub fn recipe() -> Recipe {
    Recipe::mesboot("gcc-x86-64-native-test", "1.0")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
        ])
        .steps(vec![
            Step::MkDir {
                path: "{root}/test".into(),
            },
            Step::WriteFile {
                path: "{root}/test/assert.c".into(),
                content: assert_source().into(),
                exec: false,
            },
            Step::WriteFile {
                path: "{root}/test/probe.c".into(),
                content: c_probe_source().into(),
                exec: false,
            },
            Step::WriteFile {
                path: "{root}/test/probe.cc".into(),
                content: cxx_probe_source().into(),
                exec: false,
            },
            compile_step(NGCC, "{root}/test/assert.c", "{root}/test/assert-tool", false),
            compile_step(NGCC, "{root}/test/probe.c", "{root}/test/probe-c", false),
            compile_step(NGPP, "{root}/test/probe.cc", "{root}/test/probe-cxx", true),
            Step::run("{root}/test", &["{root}/test/probe-c"]),
            Step::run("{root}/test", &["{root}/test/probe-cxx"]),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "elf64-x86_64", NGCC],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "elf64-x86_64", NGPP],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "elf64-x86_64",
                    "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/libexec/gcc/x86_64-pc-linux-gnu/14.3.0/cc1",
                ],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "elf64-x86_64",
                    "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/libexec/gcc/x86_64-pc-linux-gnu/14.3.0/cc1plus",
                ],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "elf64-x86_64",
                    &format!("{NBIN}/as"),
                ],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "elf64-x86_64",
                    &format!("{NBIN}/ld"),
                ],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "elf64-x86_64", "{root}/test/probe-c"],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "elf64-x86_64", "{root}/test/probe-cxx"],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "no-gnu-tree",
                    NGCC,
                ],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "no-gnu-tree", NGPP],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "no-gnu-tree",
                    "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/libexec/gcc/x86_64-pc-linux-gnu/14.3.0/cc1",
                ],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "no-gnu-tree",
                    "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/libexec/gcc/x86_64-pc-linux-gnu/14.3.0/cc1plus",
                ],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "no-gnu-tree", &format!("{NBIN}/as")],
            ),
            Step::run(
                "{root}/test",
                &["{root}/test/assert-tool", "no-gnu-tree", &format!("{NBIN}/ld")],
            ),
            Step::run(
                "{root}/test",
                &[
                    "{root}/test/assert-tool",
                    "no-gnu-tree",
                    &format!("{XGLIBC}/lib"),
                ],
            ),
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "gcc-x86-64-native recipe test passed\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::daily(
            r#"
echo "[td] running gcc-x86-64-native-test recipe check"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gcc-x86-64-native-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn compile_step(compiler: &str, source: &str, output: &str, cxx: bool) -> Step {
    let mut argv = vec![
        compiler.to_owned(),
        "-isystem".into(),
        format!("{XGLIBC}/include"),
        "-B".into(),
        format!("{NBIN}/"),
        "-B".into(),
        format!("{XGLIBC}/lib"),
        "-L".into(),
        format!("{XGLIBC}/lib"),
        "-static-libgcc".into(),
    ];
    if cxx {
        argv.push("-static-libstdc++".into());
    }
    argv.extend([
        "-Wl,--dynamic-linker".into(),
        format!("-Wl,{XGLIBC}/lib/ld-linux-x86-64.so.2"),
        "-Wl,--enable-new-dtags".into(),
        "-Wl,-rpath".into(),
        format!("-Wl,{XGLIBC}/lib"),
        "-o".into(),
        output.into(),
        source.into(),
    ]);
    Step::Run {
        argv,
        env: vec![("PATH".into(), NBIN.into())],
        dir: "{root}/test".into(),
    }
}

fn c_probe_source() -> &'static str {
    r#"#include <unistd.h>

int main(void) {
    return access("/gnu/store", F_OK) == 0 ? 10 : 0;
}
"#
}

fn cxx_probe_source() -> &'static str {
    r#"#include <unistd.h>
#include <vector>

int main(void) {
    std::vector<int> values;
    for (int i = 0; i < 64; ++i) {
        values.push_back(i);
    }
    if (access("/gnu/store", F_OK) == 0) {
        return 10;
    }
    return values[42] == 42 ? 0 : 11;
}
"#
}

fn assert_source() -> &'static str {
    r#"#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char needle[] = "/gnu/store";

static int scan_buffer(const unsigned char *buf, long len, unsigned long *matched) {
    unsigned long nlen = sizeof(needle) - 1;
    for (long i = 0; i < len; ++i) {
        if (buf[i] == (unsigned char)needle[*matched]) {
            *matched += 1;
            if (*matched == nlen) {
                return 1;
            }
        } else {
            *matched = buf[i] == (unsigned char)needle[0] ? 1 : 0;
        }
    }
    return 0;
}

static int scan_file(const char *path) {
    unsigned char buf[4096];
    unsigned long matched = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    for (;;) {
        long n = (long)read(fd, buf, sizeof(buf));
        if (n < 0) {
            close(fd);
            return -1;
        }
        if (n == 0) {
            close(fd);
            return 0;
        }
        if (scan_buffer(buf, n, &matched)) {
            close(fd);
            return 1;
        }
    }
}

static int scan_path(const char *path) {
    struct stat st;
    if (lstat(path, &st) != 0) {
        return -1;
    }
    if (S_ISLNK(st.st_mode)) {
        char target[4096];
        long n = (long)readlink(path, target, sizeof(target) - 1);
        unsigned long matched = 0;
        if (n < 0) {
            return -1;
        }
        target[n] = '\0';
        return scan_buffer((const unsigned char *)target, n, &matched);
    }
    if (S_ISREG(st.st_mode)) {
        return scan_file(path);
    }
    if (!S_ISDIR(st.st_mode)) {
        return 0;
    }

    DIR *dir = opendir(path);
    if (dir == NULL) {
        return -1;
    }
    for (;;) {
        struct dirent *ent = readdir(dir);
        char child[4096];
        int n;
        int r;
        if (ent == NULL) {
            closedir(dir);
            return 0;
        }
        if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
            continue;
        }
        n = snprintf(child, sizeof(child), "%s/%s", path, ent->d_name);
        if (n < 0 || (unsigned long)n >= sizeof(child)) {
            closedir(dir);
            return -1;
        }
        r = scan_path(child);
        if (r != 0) {
            closedir(dir);
            return r;
        }
    }
}

static int elf64_x86_64(const char *path) {
    unsigned char hdr[20];
    int fd = open(path, O_RDONLY);
    long n;
    unsigned int machine;
    if (fd < 0) {
        return 0;
    }
    n = (long)read(fd, hdr, sizeof(hdr));
    close(fd);
    if (n != (long)sizeof(hdr)) {
        return 0;
    }
    machine = (unsigned int)hdr[18] | ((unsigned int)hdr[19] << 8);
    return hdr[0] == 0x7f && hdr[1] == 'E' && hdr[2] == 'L' && hdr[3] == 'F'
        && hdr[4] == 2 && machine == 62;
}

int main(int argc, char **argv) {
    int r;
    if (argc != 3) {
        return 64;
    }
    if (strcmp(argv[1], "elf64-x86_64") == 0) {
        return elf64_x86_64(argv[2]) ? 0 : 65;
    }
    if (strcmp(argv[1], "no-gnu-tree") == 0) {
        r = scan_path(argv[2]);
        if (r < 0) {
            return 66;
        }
        return r == 0 ? 0 : 67;
    }
    return 68;
}
"#
}
