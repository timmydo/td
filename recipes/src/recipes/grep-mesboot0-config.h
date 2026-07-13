/* td grep-2.4 config.h (re #469).

   live-bootstrap builds grep-2.4 under tcc + mes libc with NO config.h at all
   (steps/grep-2.4/mk/main.mk passes every feature macro on the tcc command
   line). Four of its six -D are quote-free and stay on td's CFLAGS command line
   unchanged (HAVE_DIRENT_H / HAVE_UNISTD_H / HAVE_STRERROR / REGEX_MALLOC -- see
   grep-mesboot0.mk). The two below cannot: PACKAGE and VERSION are STRING-valued
   (live-bootstrap writes -DPACKAGE=\"grep\" -DVERSION=\"2.4\"), and the escaped
   `"` is a shell metacharacter. td's Make 3.80 drives tcc through its stock
   no-shell fast path (the sandbox has no $(SHELL)); a `"` on a recipe line would
   force the nonexistent shell. td therefore moves these two into config.h -- the
   same string-define move sed-mesboot0 / coreutils-mesboot0 / bash-mesboot make.

   grep-2.4's sources reach config.h through the autoconf guard
   `#ifdef HAVE_CONFIG_H / # include <config.h>` (lib/savedir.c uses the
   equivalent `#if HAVE_CONFIG_H`); grep-mesboot0.mk's CFLAGS set -DHAVE_CONFIG_H,
   so the include fires and config.h reaches every translation unit that
   references either macro. HAVE_CONFIG_H's ONLY effect in these sources is that
   include -- no other code is guarded on it -- so activating it is behaviorally
   identical to live-bootstrap's command-line -D. VERSION is read unconditionally
   (src/grep.c's --version banner); PACKAGE is read only under `#if ENABLE_NLS`,
   which is off here (mes libc has no gettext), so PACKAGE is defined for fidelity
   with live-bootstrap but is not otherwise consumed. */

#define PACKAGE "grep"
#define VERSION "2.4"
