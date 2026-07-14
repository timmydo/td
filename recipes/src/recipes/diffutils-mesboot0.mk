# td diffutils-2.7 Makefile (re #469) -- live-bootstrap's diffutils-2.7 mk/main.mk
# (its tcc + mes-libc build) with td's tcc/mes store paths baked in and the mes-
# libc deltas below. Driven by td's Make 3.80: every recipe line is
# metacharacter-free, so make's stock no-shell fast path execs tcc directly (the
# sandbox has no $(SHELL)) -- load-bearing, so keep every flag value
# metacharacter-free.
#
# Deviations from live-bootstrap's mk, all to stay shell-free, host-tool-free, or
# to use td's mes libc correctly:
#
#   * live-bootstrap's CFLAGS carry -DNULL_DEVICE=\"/dev/null\"; the escaped `"`
#     is a shell metacharacter, so that one STRING define moves into config.h
#     (see diffutils-mesboot0-config.h). diffutils-2.7's system.h/version.c
#     #include <config.h> unconditionally, so cmp.c reaches NULL_DEVICE with no
#     -DHAVE_CONFIG_H needed. Every other define is quote-free and stays here.
#   * -DHAVE_STRING_H=1 is ADDED (live-bootstrap sets neither STRING_H nor
#     STDC_HEADERS). Without it, system.h:169-185 and regex.c take the pre-ANSI
#     `#else` branch: `#define strchr index`, `#define strrchr rindex`,
#     `#define memcmp bcmp`, `#define memcpy bcopy` -- i.e. they depend on
#     index/rindex/bcmp/bcopy, which td's mes-0.27.1 does NOT provide (it has the
#     ANSI strchr/strrchr/memchr/memcmp/memcpy). HAVE_STRING_H routes both to
#     <string.h> and emits `#define bzero(s,n) memset(s,0,n)` (mes has memset),
#     resolving every string/mem reference to a symbol mes actually ships.
#   * -Dvfork=fork is ADDED. diff's paginator path (util.c:197, taken only under
#     HAVE_FORK) calls vfork(); td's mes has fork() but no vfork(). fork is a
#     safe substitute here (the child execs immediately), and the define rewrites
#     only the identifier, not the "vfork" diagnostic string. The path is never
#     reached at run time (only `diff -l`/paginate uses it), but the symbol must
#     resolve at link. mes provides the rest of that path (pipe/execl/dup2/
#     fdopen/waitpid); PR_PROGRAM defaults to "/bin/pr" via util.c:22's #ifndef
#     (live-bootstrap does not pass -DPR_PROGRAM, whose upstream form is also
#     quote-escaped), and is likewise never exec'd here.
#   * -DHAVE_SYS_WAIT_H=1 / -DHAVE_STDLIB_H=1 / -DHAVE_TIME_H=1 are ADDED
#     (correctness, mes ships all three headers): the first gives util.c the
#     correct WEXITSTATUS for its waitpid, the second proper malloc/free/exit
#     prototypes, the third context.c/diff.c their ctime()/time() declarations
#     (mes's ctime is a stub -- fine, it feeds only diff's context-header
#     timestamps, never the comparison result).
#   * LDFLAGS = -static is a td addition: live-bootstrap leaves LDFLAGS empty, but
#     this rung's AssertStatic (re #469) requires fully static cmp/diff (no host
#     loader/libc at run time) -- the same -static grep/sed/coreutils/gawk carry.
#   * alloca is DROPPED from DIFF_SRC. mes-0.27.1 provides the GNU C alloca, and
#     diffutils's alloca.c would duplicate-define it (as gawk's did). With
#     -DREGEX_MALLOC the bundled regex allocates with malloc; its only residual
#     alloca(0) cleanup calls resolve to mes's alloca. This mirrors upstream's
#     configure leaving $(ALLOCA) empty when the libc has alloca.
#   * The `install:` target is dropped: live-bootstrap's runs host `install`,
#     absent in this sandbox (that is what the bootstrap is building).
#     diffutils-mesboot0.rs installs the `cmp` and `diff` binaries with engine-
#     native Steps instead (two independent binaries -- no symlink/alias).
#
# The per-object sources compile through make's built-in %.o:%.c rule (as in
# grep/sed/gawk-mesboot0.mk), which passes CFLAGS. The engine expands the store-
# path placeholders below when it writes this file.

CC      = {in:tcc}/bin/tcc
LDFLAGS = -static

# -I. FIRST so config.h (written to {src}/config.h, compile cwd={src}) resolves
# before mes's libc headers. The mes include dirs mirror grep/gawk-mesboot0.mk
# (tcc bakes the same paths; explicit belt-and-suspenders). The remaining defines
# are live-bootstrap's quote-free CFLAGS verbatim plus the mes-libc deltas above.
CFLAGS  = -I. \
          -I{in:mes}/include -I{in:mes}/include/x86 \
          -DHAVE_STRERROR=1 \
          -DREGEX_MALLOC=1 \
          -DHAVE_DIRENT_H \
          -DHAVE_DUP2=1 \
          -DHAVE_FORK=1 \
          -DHAVE_STRING_H=1 \
          -DHAVE_SYS_WAIT_H=1 \
          -DHAVE_STDLIB_H=1 \
          -DHAVE_TIME_H=1 \
          -Dvfork=fork

.PHONY: all

# live-bootstrap's CMP_SRC verbatim -> the `cmp` binary.
CMP_SRC = cmp cmpbuf error getopt getopt1 xmalloc version
CMP_OBJ = $(addsuffix .o, $(CMP_SRC))

# live-bootstrap's DIFF_SRC, less `alloca` (see the header note) -> the `diff`
# binary. cmpbuf/getopt/getopt1/version are shared with cmp; make builds each .o
# once and links it into both.
DIFF_SRC = diff analyze cmpbuf dir io util context ed ifdef normal side fnmatch \
           getopt getopt1 regex version
DIFF_OBJ = $(addsuffix .o, $(DIFF_SRC))

all: cmp diff

cmp: $(CMP_OBJ)
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

diff: $(DIFF_OBJ)
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@
