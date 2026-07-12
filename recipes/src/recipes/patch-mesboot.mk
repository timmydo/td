# td patch-2.5.9 Makefile (re #469) — the object list + compile/link rules GNU
# patch's ./configure emits for the tcc + mes-libc target, reduced to a minimal
# metacharacter-free Makefile with td's tcc/mes store paths baked in (the host
# install/dist/dependency-tracking targets configure also emits are dropped —
# the engine installs `patch`). Driven by td's Make 3.80: every recipe line is
# metacharacter-free, so make's no-shell fast path execs tcc directly (the
# sandbox has no $(SHELL)). This is load-bearing — make falls back to $(SHELL)
# (which does not exist here) the moment a recipe line contains a shell
# metacharacter, so keep the CC command word first and every flag value
# metacharacter-free too. The generated compile rule embeds -Ded_PROGRAM=\"ed\",
# whose escaped quotes ARE such a metacharacter; td moves ed_PROGRAM into
# config.h (#define ed_PROGRAM "ed") so the compile line stays clean — the same
# move oyacc.mk / make-mesboot0-config.h make for their string-valued -D. The
# engine expands the store-path placeholders below when it writes this file.
#
# tcc finds its crt/libc/libtcc1 via the store paths baked into it (no -B), and
# the recipe copies crt/libc beside the sources to satisfy the -L. (mirroring
# configure's Makefile). The link rule (LDFLAGS/LIBS empty for this target)
# keeps -static -L. on CC.
CC = {in:tcc}/bin/tcc -static -L.

CFLAGS = -g
CPPFLAGS =
DEFS = -DHAVE_CONFIG_H

# Header search path, -I. FIRST — this reproduces configure's DEFAULT_INCLUDES
# (`-I. -I$(srcdir)`), which always PRECEDES the compiler's system include path.
# It is load-bearing for faithfulness: patch ships its OWN <stdbool.h> (generated
# from stdbool.h.in — bool == _Bool) and <getopt.h>, and mes's include dir ALSO
# has stdbool.h (the nonconforming `typedef int bool`) + getopt.h. With -I. first,
# patch's own headers win, exactly as the real configure+make build resolves them;
# putting the mes -I first (as an earlier draft did) silently shadowed both with
# mes's, diverging patch's `bool` from the pinned configure result. -I{mes}/include/x86
# is a no-op for mes 0.27.1 (no such subdir) but kept verbatim from configure's CC.
INCLUDES = -I. -I{in:mes}/include -I{in:mes}/include/x86

# configure's OBJS for tcc/mes. patch's only AC_REPLACE_FUNCS are `mkdir` and
# `strncasecmp`, plus gnulib's error/memchr/rmdir/malloc/realloc replacements;
# config.h reports HAVE_MKDIR/MEMCHR/RMDIR/MALLOC/REALLOC=1 (mes libc supplies
# them) and HAVE_STRNCASECMP undef, and error() is absent — so LIBOBJS collapses
# to exactly the two functions mes lacks (error, strncasecmp). The rest is patch's
# own translation units.
OBJS = error.o strncasecmp.o \
	addext.o argmatch.o backupfile.o basename.o dirname.o \
	getopt.o getopt1.o inp.o maketime.o partime.o \
	patch.o pch.o quote.o quotearg.o quotesys.o \
	util.o version.o xmalloc.o

# configure's own compile rule (COMPILE macro + .c.o suffix rule), verbatim
# except the -Ded_PROGRAM=\"$(ed_PROGRAM)\" term (now in config.h) and with
# INCLUDES (-I. first) in place of configure's DEFAULT_INCLUDES + package -I.
COMPILE = $(CC) -c $(INCLUDES) $(CPPFLAGS) $(DEFS) $(CFLAGS)

.c.o:
	$(COMPILE) $<

# configure's link rule (LDFLAGS/LIBS are empty for this target).
patch: $(OBJS)
	$(CC) -o patch $(CFLAGS) $(OBJS)
