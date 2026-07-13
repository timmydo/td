# td coreutils-5.0 Makefile (re #469) -- live-bootstrap's coreutils-5.0
# mk/main.mk (its pass1/LIBC=mes build) with td's tcc/mes store paths baked in.
# Driven by td's Make 3.80: every recipe line is metacharacter-free, so make's
# no-shell fast path execs tcc directly (the sandbox has no $(SHELL)). This is
# load-bearing -- make falls back to $(SHELL) (which does not exist here) the
# moment a recipe line contains a shell metacharacter, so keep every flag value
# metacharacter-free.
#
# Deviations from live-bootstrap's mk, all to stay shell-free:
#
#   * live-bootstrap's CFLAGS carry ~50 -D defines; ten of them are STRING- or
#     paren-valued (PACKAGE*, VERSION, GNU_PACKAGE, PACKAGE_BUGREPORT, LIBDIR,
#     LC_TIME, LC_COLLATE, DIR_TO_FD) whose escaped `"`/space/`()` ARE shell
#     metacharacters; those move into config.h (see coreutils-mesboot0-config.h),
#     reached via -DHAVE_CONFIG_H below. The other ~40 are quote-free and stay
#     on this command line, GLOBAL to every TU exactly as live-bootstrap has
#     them (so symbol-rename defines like my_strftime/mkstemp/major_t/minor_t
#     reach even the sources that never #include config.h).
#   * The false.c rule (`cp true.c false.c` + `sed -i`) is dropped: it needs
#     host cp/sed and metacharacter-laden sed scripts. The tarball already ships
#     src/false.c BYTE-IDENTICAL to that regeneration (verified), so `false` just
#     builds from the shipped false.c through the built-in %.o:%.c rule.
#   * The per-object sources compile through make's built-in %.o:%.c rule (as in
#     sed-mesboot0.mk / oyacc.mk), which passes CFLAGS.
#
# The engine expands the store-path placeholders below when it writes this file.

CC      = {in:tcc}/bin/tcc
LD      = {in:tcc}/bin/tcc
AR      = {in:tcc}/bin/tcc -ar
LDFLAGS = -static

bindir=$(PREFIX)/bin

# -DHAVE_CONFIG_H activates config.h (the ten metacharacter-bearing defines).
# -I. -Ilib FIRST so coreutils' own headers and the copied fnmatch.h/ftw.h/
# search.h resolve before mes's libc headers (live-bootstrap's `-I . -I lib`).
# The remaining defines are live-bootstrap's quote-free CFLAGS verbatim.
CFLAGS  = -DHAVE_CONFIG_H -I. -Ilib \
          -I{in:mes}/include -I{in:mes}/include/x86 \
          -DHAVE_LIMITS_H=1 \
          -DHAVE_DECL_FREE=1 \
          -DHAVE_DECL_MALLOC=1 \
          -DHAVE_MALLOC=1 \
          -DHAVE_STDLIB_H=1 \
          -DHAVE_REALLOC=1 \
          -DHAVE_DECL_REALLOC=1 \
          -DHAVE_DECL_GETENV=1 \
          -DHAVE_DIRENT_H=1 \
          -DHAVE_DECL___FPENDING=0 \
          -DSTDC_HEADERS=1 \
          -DHAVE_ALLOCA_H=1 \
          -DHAVE_STRUCT_TIMESPEC=1 \
          -DHAVE_STRING_H=1 \
          -DHAVE_SYS_TIME_H=1 \
          -DTIME_WITH_SYS_TIME=1 \
          -DHAVE_STDINT_H=1 \
          -DMB_LEN_MAX=16 \
          -DHAVE_DECL_WCWIDTH=0 \
          -DHAVE_SYS_STAT_H=1 \
          -DHAVE_INTTYPES_H=1 \
          -DHAVE_DECL_MEMCHR=1 \
          -DHAVE_MEMORY_H=1 \
          -DPENDING_OUTPUT_N_BYTES=1 \
          -DCHAR_MIN=0 \
          -DLOCALEDIR=NULL \
          -DHAVE_FCNTL_H=1 \
          -DEPERM=1 \
          -DHAVE_DECL_STRTOUL=1 \
          -DHAVE_DECL_STRTOULL=1 \
          -DHAVE_DECL_STRTOL=1 \
          -DHAVE_DECL_STRTOLL=1 \
          -DHAVE_RMDIR=1 \
          -DRMDIR_ERRNO_NOT_EMPTY=39 \
          -DENOTEMPTY=1 \
          -DLSTAT_FOLLOWS_SLASHED_SYMLINK=1 \
          -DHAVE_DECL_DIRFD=0 \
          -DHAVE_GETCWD=1 \
          -Dmy_strftime=nstrftime \
          -Dmkstemp=rpl_mkstemp \
          -DUTILS_OPEN_MAX=1000 \
          -Dmajor_t=unsigned \
          -Dminor_t=unsigned

.PHONY: all install

SRC_DIR=src

COREUTILS = basename cat chmod cksum csplit cut dirname echo expand expr factor false fmt fold head hostname id join kill link ln logname mkfifo mkdir mknod nl od paste pathchk pr printf ptx pwd readlink rmdir seq sleep sort split sum tail tee tr tsort unexpand uniq unlink wc whoami tac test touch true yes

BINARIES = $(addprefix $(SRC_DIR)/, $(COREUTILS))

ALL=$(BINARIES) $(SRC_DIR)/cp $(SRC_DIR)/ls $(SRC_DIR)/install $(SRC_DIR)/md5sum $(SRC_DIR)/mv $(SRC_DIR)/rm $(SRC_DIR)/sha1sum
all: $(BINARIES) $(SRC_DIR)/cp $(SRC_DIR)/ls $(SRC_DIR)/install $(SRC_DIR)/md5sum $(SRC_DIR)/mv $(SRC_DIR)/rm $(SRC_DIR)/sha1sum

LIB_DIR = lib
LIB_SRC = acl posixtm posixver strftime getopt getopt1 hash hash-pjw addext argmatch backupfile basename canon-host closeout cycle-check diacrit dirname dup-safer error exclude exitfail filemode __fpending file-type fnmatch fopen-safer full-read full-write gethostname getline getstr gettime hard-locale human idcache isdir imaxtostr linebuffer localcharset long-options makepath mbswidth md5 memcasecmp memcoll modechange offtostr path-concat physmem quote quotearg readtokens rpmatch safe-read safe-write same save-cwd savedir settime sha stpcpy stripslash strtoimax strtoumax umaxtostr unicodeio userspec version-etc xgetcwd xgethostname xmalloc xmemcoll xnanosleep xreadlink xstrdup xstrtod xstrtol xstrtoul xstrtoimax xstrtoumax yesno strnlen getcwd sig2str mountlist regex canonicalize mkstemp memrchr euidaccess ftw dirfd obstack strverscmp strftime tempname tsearch

LIB_OBJECTS = $(addprefix $(LIB_DIR)/, $(addsuffix .o, $(LIB_SRC)))

$(LIB_DIR)/libfettish.a: $(LIB_OBJECTS)
	$(AR) cr $@ $^

# live-bootstrap's static pattern rule verbatim (mk/main.mk): link each
# single-obj binary `src/X` from `src/X.o` + libfettish.a. td's Make 3.80 (a
# minimal mescc build) drives the `targets: tpattern: pprereq` static-pattern
# syntax correctly. This rule matches only the $(BINARIES) targets; the seven
# multi-obj binaries below (cp/install/ls/md5sum/mv/rm/sha1sum) are NOT in
# $(BINARIES) and are built by their own explicit rules.
$(BINARIES) : % : %.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/cp: $(SRC_DIR)/cp.o $(SRC_DIR)/copy.o $(SRC_DIR)/cp-hash.c $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/install: $(SRC_DIR)/install.o $(SRC_DIR)/copy.o $(SRC_DIR)/cp-hash.c $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/ls: $(SRC_DIR)/ls.o $(SRC_DIR)/ls-ls.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/md5sum: $(SRC_DIR)/md5.o $(SRC_DIR)/md5sum.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/mv: $(SRC_DIR)/mv.o $(SRC_DIR)/copy.o $(SRC_DIR)/remove.o $(SRC_DIR)/cp-hash.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/rm: $(SRC_DIR)/rm.o $(SRC_DIR)/remove.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

$(SRC_DIR)/sha1sum: $(SRC_DIR)/sha1sum.o $(SRC_DIR)/md5sum.o $(LIB_DIR)/libfettish.a
	$(CC) $(CFLAGS) $^ $(LDFLAGS) -o $@

install: $(ALL)
	$(SRC_DIR)/install $^ $(bindir)
