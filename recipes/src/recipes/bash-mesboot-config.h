/* td bash-2.05b config.h (re #469).
 *
 * live-bootstrap builds bash-2.05b with an EMPTY config.h and supplies all ~45
 * configuration macros as `-D` flags on tcc's command line (its mk/common.mk
 * COMMON_CFLAGS). td cannot: many of those defines carry shell metacharacters
 * (`"` in the string literals, `()` in the two function-macro overrides), and a
 * make recipe line containing a shell metacharacter makes GNU Make fall back to
 * $(SHELL) — which does not exist in td's sandbox (re #469, the same no-shell
 * fast-path constraint oyacc.mk documents). So td moves EVERY define here into
 * config.h, which bash `#include`s unconditionally at the top of all 36 .c files
 * (shell.c:27, before any code). The baked Makefiles then carry only `-I`/`-L`
 * paths, keeping every recipe line metacharacter-free.
 *
 * Each macro below is the exact live-bootstrap `-D` flag transcribed: bare
 * feature macros define to 1 (tcc's `-DNAME` == `-DNAME=1`); valued/string/
 * function macros keep their literal value.
 */

/* Headers / struct feature tests */
#define HAVE_DIRENT_H 1
#define STRUCT_DIRENT_HAS_D_INO 1
#define HAVE_STDINT_H 1
#define HAVE_LIMITS_H 1
#define HAVE_STRING_H 1
#define HAVE_INTTYPES_H 1

/* Signals / tty */
#define RETSIGTYPE void
#define VOID_SIGHANDLER 1
#define HAVE_POSIX_SIGNALS 1
#define HAVE_SYS_SIGLIST 1
#define TERMIO_TTY_DRIVER 1

/* Misc build knobs */
#define HUGE_VAL 10000000000.0
#define PREFER_STDARG 1
#define HAVE_DECL_STRTOL 1
#define HAVE_DECL_STRTOLL 1
#define HAVE_DECL_STRTOUL 1
#define HAVE_DECL_STRTOULL 1
#define HAVE_TZNAME 1
#define PIPESIZE 4096
#define GETGROUPS_T int
#define COND_COMMAND 1

/* Paths / prompts / identity strings */
#define DEFAULT_PATH_VALUE "/bin"
#define STANDARD_UTILS_PATH "/bin"
#define PPROMPT "$ "
#define SPROMPT "$ "
#define CONF_MACHTYPE "bootstrap"
#define CONF_HOSTTYPE "i386"
#define CONF_OSTYPE "linux"
#define DEFAULT_MAIL_DIRECTORY "/fake-mail"

/* Version strings (empty include/version.h; values come from here) */
#define DISTVERSION "2.05b"
#define BUILDVERSION "0"
#define SCCSVERSION "2.05b"

/* Locale is stubbed out (mes libc has none; see the locale/snprintf patches) */
#define LC_ALL "C"

/* libc feature tests */
#define HAVE_STRERROR 1
#define HAVE_MEMSET 1
#define HAVE_DUP2 1
#define HAVE_STRTOUL 1
#define HAVE_STRTOULL 1
#define HAVE_STRCHR 1
#define HAVE_BCOPY 1
#define HAVE_BZERO 1
#define HAVE_GETCWD 1
#define HAVE_RENAME 1

/* Stub the functions mes/tcc libc lacks */
#define endpwent(x) 0
#define enable_hostname_completion(on_or_off) 0
