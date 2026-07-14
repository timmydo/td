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
#     prototypes, the third context.c/diff.c their ctime()/time() declarations.
#     mes's ctime is a STUB that returns the literal "now" WITHOUT ctime's
#     trailing newline (mes-0.27.1 lib/stub/ctime.c). It feeds only the
#     `diff -c`/`-u` context/unified header timestamp (context.c:49-53,
#     `fprintf(..., "%s %s\t%s", mark, name, ctime(&mtime))`), so under those two
#     formats a differing file's `*** `/`--- `/`+++ ` header line loses its
#     newline and the next line runs on -- a cosmetic malformation of `-c`/`-u`
#     output on DIFFERING inputs only. It never affects the comparison RESULT
#     (the exit code), plain `diff`/`diff -q`/`cmp` (which never call ctime), or
#     any acceptance test here (all compare EQUAL inputs, so no header prints).
#     It is also off the bootstrap's critical path: the consumers gate on
#     `cmp`/plain-`diff` exit codes and apply pre-made patches with `patch`
#     rather than generating `-u`/`-c` diffs. (Flagged by the cross-model review;
#     a real fix is a newline-terminated mes ctime, a separate mes change.)
#   * LDFLAGS = -static is a td addition: live-bootstrap leaves LDFLAGS empty, but
#     this rung's AssertStatic (re #469) requires fully static cmp/diff (no host
#     loader/libc at run time) -- the same -static grep/sed/coreutils/gawk carry.
#   * alloca is DROPPED from DIFF_SRC. The operative reason is duplicate-symbol:
#     mes-0.27.1 already provides the GNU C alloca, so linking diffutils's own
#     alloca.c would define `alloca` twice and fail the link (as gawk's did).
#     Dropping it is safe because NO compiled diff/cmp source actually references
#     alloca: -DREGEX_MALLOC selects regex.c's malloc path (regex.c:197, so the
#     `#else` alloca branch at :202-230 is compiled out), and regex.c's only
#     alloca-based cleanup (:908) is additionally gated on `emacs`/`REL_ALLOC`,
#     neither of which is defined here, so it too compiles out; none of the other
#     DIFF_SRC/CMP_SRC files call alloca. So alloca.o is an unused, conflict-prone
#     object -- exactly what upstream's configure omits (leaving $(ALLOCA) empty)
#     when the libc supplies alloca. (The EXIT=0 static link confirms no alloca
#     symbol is left unresolved.)
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
