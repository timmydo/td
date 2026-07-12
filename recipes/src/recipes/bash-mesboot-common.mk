# td bash-2.05b common.mk (re #469) — live-bootstrap's mk/common.mk with td's
# tcc/mes store paths baked in. Two differences from upstream, both forced by
# td's no-shell sandbox (make-mesboot0 has no $(SHELL) to fall back to, so every
# recipe line must stay metacharacter-free — see bash-mesboot.mk):
#   * CC/LD/AR are the absolute baked tcc (upstream's bare `tcc` needs a PATH).
#   * The ~45 -D config macros upstream lists here are moved into config.h
#     (bash-mesboot-config.h) — the string/paren defines would otherwise put a
#     `"`/`(` on a recipe line and force the (nonexistent) shell. COMMON_CFLAGS
#     therefore carries only the mes libc header search paths.
CC = {in:tcc}/bin/tcc
LD = {in:tcc}/bin/tcc
AR = {in:tcc}/bin/tcc -ar

COMMON_CFLAGS = -I{in:mes}/include -I{in:mes}/include/x86

BUILTINS_DEF_FILES = alias bind break builtin cd colon command complete declare \
	echo enable eval exec exit fc fg_bg hash history jobs kill let read return \
	set setattr shift source suspend test times trap type ulimit umask wait \
	getopts pushd shopt printf
