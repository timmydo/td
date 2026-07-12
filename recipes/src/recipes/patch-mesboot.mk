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
# CC carries -static -L. and the mes include paths exactly as configure's CC
# line does; tcc finds its crt/libc/libtcc1 via the store paths baked into it
# (no -B), and the recipe copies crt/libc beside the sources to satisfy the -L.
# (mirroring configure's Makefile). -I{mes}/include/x86 is a no-op for mes 0.27.1
# (no such subdir) but is kept verbatim from configure's CC for faithfulness.
CC = {in:tcc}/bin/tcc -static -L. -I{in:mes}/include -I{in:mes}/include/x86

CFLAGS = -g
CPPFLAGS =
DEFS = -DHAVE_CONFIG_H

# configure's OBJS for tcc/mes. mes libc supplies malloc/memchr/mkdir/realloc/
# rmdir/strcasecmp, so configure's LIBOBJS reduces to the two functions mes
# lacks (error, strncasecmp); the rest is patch's own translation units.
OBJS = error.o strncasecmp.o \
	addext.o argmatch.o backupfile.o basename.o dirname.o \
	getopt.o getopt1.o inp.o maketime.o partime.o \
	patch.o pch.o quote.o quotearg.o quotesys.o \
	util.o version.o xmalloc.o

# configure's own compile rule (COMPILE macro + .c.o suffix rule), verbatim
# except the -Ded_PROGRAM=\"$(ed_PROGRAM)\" term (now in config.h). srcdir is '.'
# so its -I$(srcdir) collapses into the -I. kept here.
COMPILE = $(CC) -c $(CPPFLAGS) $(DEFS) -I. $(CFLAGS)

.c.o:
	$(COMPILE) $<

# configure's link rule (LDFLAGS/LIBS are empty for this target).
patch: $(OBJS)
	$(CC) -o patch $(CFLAGS) $(OBJS)
