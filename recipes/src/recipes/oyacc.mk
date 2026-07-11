# td oyacc-6.6 Makefile (re #469) — live-bootstrap's oyacc mk/main.mk with td's
# tcc/mes store paths baked in, and the host-tool `install`/`test`/`clean`
# targets dropped (the engine installs `yacc` and smoke-tests it). Driven by
# td's Make 3.80: every recipe line is metacharacter-free, so make's no-shell
# fast path execs tcc directly (the sandbox has no shell). The engine expands the
# store-path placeholders below when it writes this file.
CC =		{in:tcc}/bin/tcc
CFLAGS =	-D__dead= -D__unused= -I{in:mes}/include -I{in:mes}/include/x86
LDFLAGS =	-static -L{in:tcc}/lib
LIBS =		-lgetopt
PROG =		yacc

OBJS =	closure.o error.o lalr.o lr0.o main.o mkpar.o output.o reader.o \
	skeleton.o symtab.o verbose.o warshall.o portable.o

all: ${PROG}

${PROG}: ${OBJS}
	${CC} ${LDFLAGS} -o ${PROG} ${OBJS} ${LIBS}
