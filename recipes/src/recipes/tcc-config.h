/* td tcc rung config.h (re #469).
 *
 * The fixed layout `configure --cc=mescc` emits (no host gcc detected, so
 * GCC_MAJOR/GCC_MINOR are empty; TCC_VERSION from the tarball's VERSION file),
 * EXTENDED with the CONFIG_TCC_* string search paths that the tarball's
 * bootstrap.sh/boot.sh otherwise pass as -D on the command line.
 *
 * Only the STRING-valued defines live here: kaem strips the embedded C
 * string-literal quotes a WriteFile preserves, so they cannot survive as -D on
 * the kaem command line. tcc.h #ifndef-guards each CONFIG_* default
 * (tcc.h:171,186,...) and includes this file first (tcc.h:25), so these win
 * over the in-tree defaults, exactly as a command-line -D would.
 *
 * The quote-free bootstrap flags (ONE_SOURCE, CONFIG_TCCBOOT, CONFIG_TCC_STATIC,
 * CONFIG_USE_LIBGCC, TCC_MES_LIBC, inline=) are NOT here: tcc.c tests
 * `#ifdef ONE_SOURCE` at its very top (tcc.c:21) to decide whether to #include
 * libtcc.c, BEFORE it includes tcc.h/config.h — so a config.h ONE_SOURCE would
 * arrive too late, tcc.c would take the #else branch, omit libtcc.c, and leave
 * tcc_realloc undefined at link. build.kaem passes all of them as -D on every
 * tcc.c compile instead. {in:mes}/{out} are td-expanded at stage time; {B} is
 * upstream's runtime crt-search placeholder, left verbatim.
 */
#define GCC_MAJOR
#define GCC_MINOR
#define TCC_VERSION "0.9.27"
#define CONFIG_TCCDIR "{out}/lib/tcc"
#define CONFIG_TCC_CRTPREFIX "{out}/lib:{B}/lib:."
#define CONFIG_TCC_ELFINTERP "/lib/mes-loader"
#define CONFIG_TCC_LIBPATHS "{out}/lib:{B}/lib:."
#define CONFIG_TCC_SYSINCLUDEPATHS "{in:mes}/include:{out}/include:{B}/include"
#define TCC_LIBGCC "{out}/lib/libc.a"
#define TCC_LIBTCC1_MES "libtcc1-mes.a"
