/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2016,2017,2018,2019,2022 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
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
#include <sys/types.h>
#include <sys/resource.h>

pid_t
wait4 (pid_t pid, int *status_ptr, int options, struct rusage *rusage)
{
  long long_pid = pid;
  long long_status_ptr = cast_voidp_to_long (status_ptr);
  long long_options = options;
  long long_rusage = cast_voidp_to_long (rusage);
  return _sys_call4 (SYS_wait4, long_pid, long_status_ptr, long_options,
                     long_rusage);
}
