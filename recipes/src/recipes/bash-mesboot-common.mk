# td bash-2.05b common.mk (re #469) — live-bootstrap's mk/common.mk with td's
# tcc/mes store paths baked in. Two differences from upstream, both forced by
# td's no-shell sandbox (make-mesboot0 has no $(SHELL) to fall back to, so every
# recipe line must stay metacharacter-free — see bash-mesboot.mk):
#   * CC/LD/AR are the absolute baked tcc (upstream's bare `tcc` needs a PATH).
#   * The ~45 -D config macros upstream lists here are moved into config.h
#     (bash-mesboot-config.h) — the string/paren defines would otherwise put a
#     `"`/`(` on a recipe line and force the (nonexistent) shell. Every bash .c
#     already pulls config.h in (`#include <config.h>`, found via the -I paths
#     below and each Makefile's -I. / -I..), but two files (test.c, xmalloc.c)
#     gate that include behind `#if defined (HAVE_CONFIG_H)`. So COMMON_CFLAGS
#     also defines HAVE_CONFIG_H — a bare `-DNAME` with no value, hence
#     metacharacter-free — to fire those guarded includes. That makes config.h
#     reach EVERY translation unit, exactly as upstream's global -D set does.
CC = {in:tcc}/bin/tcc
LD = {in:tcc}/bin/tcc
AR = {in:tcc}/bin/tcc -ar

COMMON_CFLAGS = -DHAVE_CONFIG_H -I{in:mes}/include -I{in:mes}/include/x86

BUILTINS_DEF_FILES = alias bind break builtin cd colon command complete declare \
	echo enable eval exec exit fc fg_bg hash history jobs kill let read return \
	set setattr shift source suspend test times trap type ulimit umask wait \
	getopts pushd shopt printf
