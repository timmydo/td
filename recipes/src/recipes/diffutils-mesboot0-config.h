/* td config.h for diffutils-2.7 under tcc + mes libc (re #469).

   diffutils-2.7's system.h (line 27) and version.c (line 3) #include <config.h>
   UNCONDITIONALLY -- the core sources carry no HAVE_CONFIG_H guard -- so this
   file is mandatory on the -I. path, not optional (unlike grep-2.4, which built
   with no config.h).

   Its sole job is the one string-valued define live-bootstrap's mk passes as
   -DNULL_DEVICE=\"/dev/null\": the escaped `"` is a shell metacharacter td's
   no-shell make cannot carry on a recipe line, so NULL_DEVICE moves here. cmp.c
   (its only user, cmp.c:229) reaches it via system.h. Every other diffutils
   define is quote-free and stays on the tcc command line (see diffutils-mesboot0.mk).

   HAVE_CONFIG_H is deliberately NOT defined. The gnulib-style sources
   (error/getopt/getopt1/regex/fnmatch/xmalloc) that guard their config.h include
   on HAVE_CONFIG_H therefore skip it and take their feature macros from the .mk's
   -D flags instead; the unconditional includers (system.h, version.c) reach this
   file regardless. Nothing here needs to reach the gnulib files, so the split is
   correct. */
#define NULL_DEVICE "/dev/null"
