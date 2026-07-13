# td sed-4.0.9 Makefile (re #469) — live-bootstrap's sed-4.0.9 mk/main.mk
# (its LIBC=mes branch) with td's tcc/mes store paths baked in, and the
# host-tool install/checksum steps dropped (the engine installs sed/sed and
# smoke-tests it). Driven by td's Make 3.80: every recipe line is
# metacharacter-free, so make's no-shell fast path execs tcc directly (the
# sandbox has no $(SHELL)). This is load-bearing — make falls back to $(SHELL)
# (which does not exist here) the moment a recipe line contains a shell
# metacharacter, so keep the CC command word first and every flag value
# metacharacter-free.
#
# Two deviations from live-bootstrap's mk, both to stay shell-free:
#
#   * Its CPPFLAGS carry three STRING-valued defines (VERSION, PACKAGE,
#     SED_FEATURE_VERSION) whose escaped quotes ARE a metacharacter; those move
#     into config.h, which sed.h / lib include under -DHAVE_CONFIG_H (set in
#     CFLAGS below). See sed-mesboot0-config.h. Only the quote-free defines stay
#     on the command line.
#   * All compile flags live in CFLAGS (not split into CPPFLAGS). The per-object
#     sources compile through make's built-in %.o:%.c rule — as in oyacc.mk,
#     which relies on the same built-in — and that rule is guaranteed to pass
#     CFLAGS. (It also passes CPPFLAGS, left empty here.)
#
# The engine expands the store-path placeholders below when it writes this file.
CC =		{in:tcc}/bin/tcc
AR =		{in:tcc}/bin/tcc -ar

# HAVE_CONFIG_H activates config.h: sed.h / lib gate `#include "config.h"` on it
# (as autoconf'd trees do), and config.h carries the three string defines moved
# off the command line — the same `DEFS = -DHAVE_CONFIG_H` patch-mesboot.mk sets.
# Quote-free feature defines + include paths follow. -I. and -Ilib FIRST so sed's
# own headers and the generated lib/regex.h (see the recipe's regex.h symlink)
# resolve before mes's libc headers, exactly as live-bootstrap's `-I . -I lib`
# intends. ENABLE_NLS=0 compiles out sed.c's setlocale/bindtextdomain/textdomain
# (mes libc has no locale/gettext); HAVE_FCNTL_H/HAVE_ALLOCA_H match mes libc.
CFLAGS =	-DHAVE_CONFIG_H -DENABLE_NLS=0 -DHAVE_FCNTL_H -DHAVE_ALLOCA_H -I. -Ilib -I{in:mes}/include -I{in:mes}/include/x86

# -L. -lsed pulls the just-built libsed.a; -static because mes libc is
# static-only and the rung asserts a fully static ELF. tcc finds its own
# crt/libc via its baked store prefix, so no crt copy or -B is needed.
LDFLAGS =	-L. -lsed -static

# live-bootstrap's LIBC=mes LIB_SRC: getline (mes libc lacks GNU getline) plus
# the gnulib replacements mes libc does not provide (getopt1/getopt/utils/
# regex/obstack/strverscmp/mkstemp). regex.o is the single TU that #includes
# regcomp.c/regexec.c/regex_internal.c.
LIB_OBJ =	lib/getline.o lib/getopt1.o lib/getopt.o lib/utils.o lib/regex.o lib/obstack.o lib/strverscmp.o lib/mkstemp.o

# live-bootstrap's SED_SRC.
SED_OBJ =	sed/compile.o sed/execute.o sed/regexp.o sed/fmt.o sed/sed.o

all: sed/sed

libsed.a: ${LIB_OBJ}
	${AR} cr libsed.a ${LIB_OBJ}

sed/sed: libsed.a ${SED_OBJ}
	${CC} libsed.a ${SED_OBJ} ${LDFLAGS} -o sed/sed
