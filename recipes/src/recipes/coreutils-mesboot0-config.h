/* td coreutils-5.0 config.h (re #469).

   live-bootstrap builds coreutils-5.0 under tcc + mes libc with an EMPTY
   config.h (`catm config.h`), passing every feature macro on the tcc command
   line (mk/main.mk CFLAGS). Most of those are quote/paren-free and stay on td's
   CFLAGS command line unchanged (global, exactly as live-bootstrap has them --
   see coreutils-mesboot0.mk). The TEN below cannot: each carries a shell
   metacharacter (an escaped `"`, an escaped space, or escaped `()`), and td's
   Make 3.80 drives tcc through its NO-SHELL fast path (the sandbox has no
   $(SHELL)); a `"` or `(` on a recipe line would force the nonexistent shell.
   td therefore moves these into config.h -- the same move sed-mesboot0 /
   bash-mesboot make for their string-valued -D.

   Every coreutils-5.0 source file reaches config.h either by an unconditional
   `#include <config.h>` (e.g. src/ls.c, src/true.c, lib/mkstemp.c) or by the
   autoconf `#if HAVE_CONFIG_H / # include <config.h>` guard (e.g. lib/savedir.c,
   lib/strftime.c, lib/version-etc.c); coreutils-mesboot0.mk's CFLAGS set
   -DHAVE_CONFIG_H, so BOTH forms fire and config.h reaches every translation
   unit that references it. The consumers of these ten macros were verified to be
   config.h-including files only: the version/usage banners (PACKAGE*, VERSION,
   GNU_PACKAGE, PACKAGE_BUGREPORT) via lib/version-etc.c + each util's usage();
   DIR_TO_FD via lib/dirfd.c; LC_TIME/LC_COLLATE via config.h-including
   collation files (the one non-config.h consumer, lib/fnmatch_loop.c, is
   #included by lib/fnmatch.c, which includes config.h); LIBDIR via
   lib/localcharset.c. So moving them here is behaviorally identical to
   live-bootstrap's global -D. */

#define PACKAGE "coreutils"
#define PACKAGE_NAME "GNU coreutils"
#define GNU_PACKAGE "GNU coreutils"
#define PACKAGE_BUGREPORT "bug-coreutils@gnu.org"
#define PACKAGE_VERSION "5.0"
#define VERSION "5.0"

/* live-bootstrap: -DLIBDIR=\"$(PREFIX)/lib/mes\". Only lib/localcharset.c reads
   it (to locate a charset.alias that does not exist under mes libc, so the
   lookup fails harmlessly); the value is cosmetic but must be a valid string.
   {out} is template-expanded to td's own store path when the engine writes this
   file -- the same pattern make-mesboot0-config.h uses for its LIBDIR. */
#define LIBDIR "{out}/lib/mes"

/* mes libc has no locale support; live-bootstrap substitutes the "C" locale
   name for these two category constants (-DLC_TIME=\"C\" -DLC_COLLATE=\"C\"). */
#define LC_TIME "C"
#define LC_COLLATE "C"

/* live-bootstrap: -DDIR_TO_FD\(Dir_p\)=-1 (a function-like macro whose escaped
   parens are the metacharacter). lib/dirfd.c uses it; mes libc has no dirfd. */
#define DIR_TO_FD(Dir_p) -1
