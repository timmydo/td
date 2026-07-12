/* td (re #469): GNU patch 2.5.9's generated <stdbool.h> — the file its
   ./configure + make produce for the mes-libc target, i.e. stdbool.h.in with
   the single autoconf substitution @HAVE__BOOL@ -> 1 (config.h has HAVE__BOOL
   defined, HAVE_STDBOOL_H undef: tcc has _Bool but mes's <stdbool.h> is the
   nonconforming 'typedef int bool'). Makefile.in's rule is
     sed -e 's/@HAVE__BOOL@/$(HAVE__BOOL)/g' <stdbool.h.in >stdbool.h .
   patch's common.h does #include <stdbool.h>; the baked Makefile puts -I.
   before the mes includes so THIS file wins (as configure's DEFAULT_INCLUDES
   does), giving bool == _Bool (C99) instead of mes's 'typedef int bool'. */
/* Copyright (C) 2001-2002 Free Software Foundation, Inc.
   Written by Bruno Haible <haible@clisp.cons.org>, 2001.

   This program is free software; you can redistribute it and/or modify
   it under the terms of the GNU General Public License as published by
   the Free Software Foundation; either version 2, or (at your option)
   any later version.

   This program is distributed in the hope that it will be useful,
   but WITHOUT ANY WARRANTY; without even the implied warranty of
   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
   GNU General Public License for more details.

   You should have received a copy of the GNU General Public License
   along with this program; if not, write to the Free Software Foundation,
   Inc., 59 Temple Place - Suite 330, Boston, MA 02111-1307, USA.  */

#ifndef _STDBOOL_H
#define _STDBOOL_H

/* ISO C 99 <stdbool.h> for platforms that lack it.  */

/* 7.16. Boolean type and values */

/* BeOS <sys/socket.h> already #defines false 0, true 1.  We use the same
   definitions below, but temporarily we have to #undef them.  */
#ifdef __BEOS__
# undef false
# undef true
#endif

/* For the sake of symbolic names in gdb, define _Bool as an enum type.  */
#ifndef __cplusplus
# if !1
typedef enum { false = 0, true = 1 } _Bool;
# endif
#else
typedef bool _Bool;
#endif
#define bool _Bool

/* The other macros must be usable in preprocessor directives.  */
#define false 0
#define true 1
#define __bool_true_false_are_defined 1

#endif /* _STDBOOL_H */
