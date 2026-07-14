/* td gawk-3.0.4 config.h (re #469).

   live-bootstrap builds gawk-3.0.4 under tcc + mes libc with NO config.h at all
   (steps/gawk-3.0.4/mk/main.mk passes every feature macro on the tcc command
   line). All but one of those -D are quote-free and stay on td's CFLAGS command
   line unchanged (see gawk-mesboot0.mk). The one below cannot: DEFPATH is
   STRING-valued (live-bootstrap writes -DDEFPATH=\"$(PREFIX)/share/awk\"), and
   the escaped `"` is a shell metacharacter. td's Make 3.80 drives tcc through
   its stock no-shell fast path (the sandbox has no $(SHELL)); a `"` on a recipe
   line would force the nonexistent shell. td therefore moves DEFPATH into
   config.h -- the same string-define move grep-mesboot0 / sed-mesboot0 /
   coreutils-mesboot0 make.

   gawk's sources reach config.h through the autoconf guard
   `#ifdef HAVE_CONFIG_H / #include <config.h>` (awk.h:36, and the bundled
   getopt.c / getopt1.c / regex.c / dfa.c / alloca.c use the same guard);
   gawk-mesboot0.mk's CFLAGS set -DHAVE_CONFIG_H, so the include fires and
   config.h reaches every translation unit. HAVE_CONFIG_H's ONLY effect in these
   sources is that include -- no other code is guarded on it -- so activating it
   is behaviorally identical to live-bootstrap's command-line -D.

   DEFPATH is the compiled-in default AWKPATH (the search path gawk uses to
   resolve `-f name` / `@include` library scripts); posix/gawkmisc.c reads it as
   `char *defpath = DEFPATH;`. This bootstrap gawk ships no awk library scripts
   (the later native gawk rebuild provides the full awklib), and its only
   consumer -- autoconf config.status running an inline `-f subs.awk` with an
   absolute path -- never searches AWKPATH, so "." (the current directory) is a
   correct, host-path-free default. */

#define DEFPATH "."
