/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2016,2017,2018,2019,2022 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
 * Copyright © 2021 W. J. van der Laan <laanwj@protonmail.com>
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

int
dup2 (int old, int new)
{
  long long_old = old;
  long long_new = new;
#if defined (SYS_dup2)
  return _sys_call2 (SYS_dup2, long_old, long_new);
#elif defined (SYS_dup3)
  return _sys_call3 (SYS_dup3, long_old, long_new, 0);
#else
#error No usable dup2 syscall
#endif
}
