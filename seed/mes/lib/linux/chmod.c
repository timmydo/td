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

#include <mes/lib.h>
#include <linux/syscall.h>
#include <arch/syscall.h>
#include <sys/stat.h>
#include <fcntl.h>

int
chmod (char const *file_name, mode_t mask)
{
  long long_file_name = cast_charp_to_long (file_name);
  long long_mask = cast_int_to_long (mask);
#if defined (SYS_chmod)
  return _sys_call2 (SYS_chmod, long_file_name, long_mask);
#elif defined (SYS_fchmodat)
  return _sys_call3 (SYS_fchmodat, AT_FDCWD, long_file_name, long_mask);
#else
#error No usable chmod syscall
#endif
}
