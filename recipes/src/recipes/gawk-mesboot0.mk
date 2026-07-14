# td gawk-3.0.4 Makefile (re #469) -- live-bootstrap's gawk-3.0.4 mk/main.mk (its
# tcc + mes-libc build) with td's tcc/mes store paths baked in. Driven by td's
# Make 3.80: every recipe line is metacharacter-free, so make's stock no-shell
# fast path execs tcc directly (the sandbox has no $(SHELL)). This is
# load-bearing -- make routes any recipe line bearing a shell metacharacter to
# $(SHELL) (which does not exist here) -- so keep every flag value
# metacharacter-free.
#
# Deviations from live-bootstrap's mk, all to stay shell-free, host-tool-free, or
# to use td's (newer) mes libc correctly:
#
#   * live-bootstrap's CFLAGS carry -DDEFPATH=\"$(PREFIX)/share/awk\"; the escaped
#     `"` is a shell metacharacter, so that one STRING define moves into config.h
#     (see gawk-mesboot0-config.h), reached via -DHAVE_CONFIG_H below. Every other
#     define is quote-free and stays on this command line, exactly as
#     live-bootstrap has them.
#   * -I vms is DROPPED. live-bootstrap adds gawk's VMS include dir so its older
#     mes (which lacked <fcntl.h>) resolves io.c's `#include <fcntl.h>` from
#     vms/fcntl.h -- but vms/fcntl.h carries VMS O_* values (O_CREAT=0x0200,
#     O_APPEND=8) that are wrong for Linux. td's mes-0.27.1 ships its own
#     <fcntl.h> with correct Linux values (O_CREAT=0x40, O_APPEND=0x400), so the
#     VMS dir must NOT shadow it. Every other header gawk includes for a POSIX
#     build (assert/ctype/float/math/std*/sys/{mman,param,types,wait}/unistd) is
#     present in mes; the remaining vms/ headers (unixlib.h, windows.h) are
#     reached only under VMS/WIN32 guards this i386 build never takes.
#   * -DGAWK is DROPPED (as live-bootstrap drops it): its only effect in this
#     source set is in missing/strftime.c, which -DHAVE_STRFTIME=1 keeps out of
#     the build (gawk uses mes's strftime), so it is a no-op here.
#   * LDFLAGS = -static is a td addition: live-bootstrap's gawk mk leaves LDFLAGS
#     empty, but this rung's AssertStatic (re #469) requires a fully static gawk
#     (no host loader/libc at run time), so the link is forced static -- the same
#     -static LDFLAGS grep-mesboot0.mk / sed-mesboot0.mk / coreutils-mesboot0.mk
#     carry.
#   * The `install:` target is dropped: live-bootstrap's runs host `install -D`
#     and `ln -s`, neither of which exists in this sandbox (that is what the
#     bootstrap is building). gawk-mesboot0.rs installs the one `gawk` binary and
#     its `awk` symlink with engine-native Steps instead.
#   * awktab.c (the Bison-1.25 parser gawk ships pre-generated) is used AS-IS.
#     live-bootstrap `rm`s it and regenerates with host bison; td ships no host
#     bison and follows its own established pattern (grep/sed/binutils use their
#     shipped generated parsers, re #468). The shipped awktab.c compiles under
#     tcc: its alloca preamble only reaches <malloc.h>/<alloca.h> on
#     sparc/sgi/MSDOS/AIX, never on i386, and its alloca() resolves to mes libc's
#     alloca (see the GAWK_SRC note on why alloca.o is NOT linked here).
#
# The per-object sources compile through make's built-in %.o:%.c rule (as in
# grep-mesboot0.mk / sed-mesboot0.mk), which passes CFLAGS. The engine expands
# the store-path placeholders below when it writes this file.

CC      = {in:tcc}/bin/tcc
LDFLAGS = -static

# -DHAVE_CONFIG_H activates config.h (the one string-valued DEFPATH define). -I.
# FIRST so gawk's own config.h -- written to the build root at {src}/config.h, and
# the compile runs with cwd={src} -- resolves before mes's libc headers. The mes
# include dirs mirror grep-mesboot0.mk (tcc bakes the same paths, so this is
# explicit belt-and-suspenders). The remaining defines are live-bootstrap's
# quote-free CFLAGS verbatim: mes-libc feature flags, C_ALLOCA (declare
# `extern void *alloca()` for awktab.c's yacc stack; it binds to mes libc's alloca
# -- see the GAWK_SRC note), REGEX_MALLOC (heap-allocate the bundled regex, as
# grep/sed do), and the return-type/typedef macros autoconf would otherwise probe.
CFLAGS  = -DHAVE_CONFIG_H -I. \
          -I{in:mes}/include -I{in:mes}/include/x86 \
          -DC_ALLOCA=1 \
          -DGETGROUPS_T=gid_t \
          -DGETPGRP_VOID=1 \
          -DHAVE_MMAP=1 \
          -DSTDC_HEADERS=1 \
          -DREGEX_MALLOC=1 \
          -DRETSIGTYPE=void \
          -DSPRINTF_RET=int \
          -DHAVE_VPRINTF=1 \
          -DHAVE_STDARG_H=1 \
          -DHAVE_SYSTEM=1 \
          -DHAVE_TZSET=1 \
          -DHAVE_LIMITS_H=1 \
          -DHAVE_LOCALE_H=1 \
          -DHAVE_MEMORY_H=1 \
          -DHAVE_MEMCMP=1 \
          -DHAVE_MEMCPY=1 \
          -DHAVE_MEMSET=1 \
          -DHAVE_STRERROR=1 \
          -DHAVE_STRCHR=1 \
          -DHAVE_STRFTIME=1 \
          -DHAVE_STRING_H=1 \
          -DHAVE_STRTOD=1 \
          -DHAVE_SYS_PARAM_H=1 \
          -DHAVE_UNISTD_H=1 \
          -DBITOPS=1

.PHONY: all

# live-bootstrap's GAWK_SRC, less `alloca`: AWKOBJS (array..version) + the shipped
# awktab parser + the bundled library routines (getopt/getopt1/regex/dfa/random).
# gawkmisc.c dispatches to posix/gawkmisc.c on this non-VMS/non-PC build.
#
# `alloca` is DROPPED from the object list (live-bootstrap keeps it). td's
# mes-0.27.1 libc already provides the GNU C `alloca` -- the exact
# garbage-collecting implementation alloca.c compiles -- so linking gawk's own
# alloca.o too makes tcc red `'alloca' defined twice`. -DC_ALLOCA=1 stays: it only
# DECLARES `extern void *alloca()` (awk.h) for awktab.c's yacc stack, which then
# resolves to mes libc's alloca. (live-bootstrap's older mes lacked alloca, so it
# had to compile alloca.o; td's mes provides it.) missing/strchr.c (strchr AND
# strrchr) is likewise skipped via -DHAVE_STRCHR=1 above -- mes has both -- while
# -DHAVE_STRNCASECMP is NOT set, so missing.c compiles missing/strncasecmp.c into
# missing.o, the one string helper mes libc does not provide.
GAWK_SRC = array awktab builtin dfa eval field getopt getopt1 gawkmisc \
           io main missing msg node random re regex version
GAWK_OBJ = $(addsuffix .o, $(GAWK_SRC))

all: gawk

gawk: $(GAWK_OBJ)
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@
