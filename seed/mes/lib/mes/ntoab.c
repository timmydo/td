/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2016,2017,2018,2019,2020,2022 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
 * Copyright © 2024 Michael Forney <mforney@mforney.org>
 *
 * This file is part of GNU Mes.
 *
 * GNU Mes is free software; you can redistribute it and/or modify it
 * under the terms of the GNU General Public License as published by
 * the Free Software Foundation; either version 3 of the License, or (at
 * your option) any later version.
 *
 * GNU Mes is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with GNU Mes.  If not, see <http://www.gnu.org/licenses/>.
 */

#include <mes/lib.h>
#include <assert.h>
#include <stdlib.h>
#include <string.h>

#if __MESC__ && __arm__
#define __MESC__and__arm__
#endif

#if __TINYC__ && __arm__ && BOOTSTRAP
#define __TINYC__and__arm__and__BOOTSTRAP
#endif

#define __not__MESC__arm__and__not__TINYC__arm__BOOTSTRAP !defined (__MESC__and__arm__) && !defined (__TINYC__and__arm__and__BOOTSTRAP)

// FIXME: M2-Planet 1.10.0 crashes on this...
// #if __M2__ || (!defined (__MESC__and__arm__) && !defined (__TINYC__and__arm__and__BOOTSTRAP))
#if __M2__ || __not__MESC__arm__and__not__TINYC__arm__BOOTSTRAP
size_t
__mesabi_uldiv (size_t a, size_t b, size_t *remainder)
{
  remainder[0] = a % b;
  return a / b;
}
#endif

char *__itoa_buf;

char *
ntoab (long x, unsigned base, int signed_p)
{
  if (__itoa_buf == 0)
    __itoa_buf = malloc (24);
  char *p = __itoa_buf + 23;

  assert_msg (base >= 8, "base >= 8");

  int sign_p = 0;
  size_t i;
  size_t u;
  size_t b = base;
  if (signed_p != 0 && x < 0)
    {
      sign_p = 1;
      /* Avoid LONG_MIN */
      u = (-(x + 1));
      u = u + 1;
    }
  else
    u = x;

  p[0] = 0;
  do
    {
      p = p - 1;
      u = __mesabi_uldiv (u, b, &i);
      if (i > 9)
        p[0] = 'a' + i - 10;
      else
        p[0] = '0' + i;
    }
  while (u != 0);

  if (sign_p && p[0] != '0')
    {
      p = p - 1;
      p[0] = '-';
    }

  return p;
}
