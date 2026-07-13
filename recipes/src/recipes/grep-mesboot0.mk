# td grep-2.4 Makefile (re #469) -- live-bootstrap's grep-2.4 mk/main.mk (its
# tcc + mes-libc build) with td's tcc/mes store paths baked in. Driven by td's
# Make 3.80: every recipe line is metacharacter-free, so make's stock no-shell
# fast path execs tcc directly (the sandbox has no $(SHELL)). This is
# load-bearing -- make routes any recipe line bearing a shell metacharacter to
# $(SHELL) (which does not exist here) -- so keep every flag value
# metacharacter-free.
#
# Deviations from live-bootstrap's mk, all to stay shell-free or host-tool-free:
#
#   * live-bootstrap's CFLAGS carry -DPACKAGE=\"grep\" -DVERSION=\"2.4\"; the
#     escaped `"` is a shell metacharacter, so those two STRING defines move into
#     config.h (see grep-mesboot0-config.h), reached via -DHAVE_CONFIG_H below.
#     The other four defines are quote-free and stay on this command line, GLOBAL
#     to every TU exactly as live-bootstrap has them.
#   * The `install:` target is dropped: live-bootstrap's runs host `install -D`
#     and `ln -sf`, neither of which exists in this sandbox (that is what the
#     bootstrap is building). grep-mesboot0.rs installs the one `grep` binary and
#     its egrep/fgrep symlinks with engine-native Steps instead.
#   * LDFLAGS = -static is a td addition: live-bootstrap's grep mk leaves LDFLAGS
#     empty, but this rung's AssertStatic (re #469) requires a fully static grep
#     (no host loader/libc at run time), so the link is forced static -- the same
#     -static LDFLAGS sed-mesboot0.mk / coreutils-mesboot0.mk carry.
#   * live-bootstrap's mk defines AR (`tcc -ar`) and LD (`tcc`), but grep-2.4
#     links its objects directly through $(CC) ($(CC) ... $^) and never archives,
#     so both AR and LD are dead here and omitted.
#   * The per-object sources compile through make's built-in %.o:%.c rule (as in
#     sed-mesboot0.mk / coreutils-mesboot0.mk), which passes CFLAGS.
#
# The engine expands the store-path placeholders below when it writes this file.

CC      = {in:tcc}/bin/tcc
LDFLAGS = -static

# -DHAVE_CONFIG_H activates config.h (the two string-valued PACKAGE/VERSION
# defines). -I. FIRST so grep's own config.h -- written to the build root at
# {src}/config.h, and the compile runs with cwd={src} -- resolves before mes's
# libc headers. The mes include dirs mirror coreutils-mesboot0.mk (tcc bakes the
# same paths, so this is explicit belt-and-suspenders). The remaining four defines
# are live-bootstrap's quote-free CFLAGS verbatim.
CFLAGS  = -DHAVE_CONFIG_H -I. \
          -I{in:mes}/include -I{in:mes}/include/x86 \
          -DHAVE_DIRENT_H=1 \
          -DHAVE_UNISTD_H=1 \
          -DHAVE_STRERROR=1 \
          -DREGEX_MALLOC=1

.PHONY: all

# live-bootstrap's GREP_SRC verbatim: the 11 objects it builds. grepmat.o
# supplies `char const *matcher = 0` (the plain-grep matcher table); grep-2.4
# selects egrep/fgrep behavior at COMPILE time via egrepmat.o/fgrepmat.o, which
# live-bootstrap does NOT build -- so egrep/fgrep are BRE-default symlinks to this
# one binary (functional ERE/fixed come from grep's -E/-F flags), and grep-mesboot0
# installs them as such. argv[0] never selects the matcher in this version.
GREP_SRC = grep dfa kwset obstack regex stpcpy savedir getopt getopt1 search grepmat
GREP_OBJECTS = $(addprefix src/, $(addsuffix .o, $(GREP_SRC)))

all: grep

grep: $(GREP_OBJECTS)
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@
