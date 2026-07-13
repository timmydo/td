/* td sed-4.0.9 config.h (re #469).

   live-bootstrap builds sed-4.0.9 under tcc + mes libc with an EMPTY config.h,
   passing every feature macro on the compiler command line. Three of those are
   STRING-valued: -DVERSION=\"4.0.9\" -DPACKAGE=\"sed\"
   -DSED_FEATURE_VERSION=\"4.0\". Their escaped quotes are a shell
   metacharacter, and td's Make 3.80 drives tcc via its NO-SHELL fast path (the
   sandbox has no $(SHELL)); a `\"` on a recipe line would force the nonexistent
   shell. td therefore moves the three string defines into this config.h — the
   same move oyacc.mk / patch-mesboot-config.h make for their string-valued -D —
   and leaves only the quote-free defines on the command line (see
   sed-mesboot0.mk CFLAGS).

   sed.h #includes "config.h" unconditionally (sed.h:21), and every sed/*.c
   #includes "sed.h", so all three defines reach the two files that use them:
   PACKAGE (sed.c usage banner), VERSION (sed.c --version), and
   SED_FEATURE_VERSION (compile.c's script-version check). */
#define VERSION "4.0.9"
#define PACKAGE "sed"
#define SED_FEATURE_VERSION "4.0"
