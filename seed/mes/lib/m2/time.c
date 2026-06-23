/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2018,2019,2022,2023 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
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

#include <linux/syscall.h>
#include <arch/syscall.h>
#include <time.h>

char *__tv;

long
time (long* result)
{
  int r;
  if (__tv == 0)
    __tv = malloc (sizeof (struct timeval));
  struct timeval *tv = __tv;
  r = _sys_call2 (SYS_gettimeofday, tv, 0);
  if (r != 0)
    return -1;
  if (result != 0)
    result[0] = tv->tv_sec;
  return tv->tv_sec;
}
