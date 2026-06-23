/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © Timothy Sample <samplet@ngyro.com>
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

#include "mes/lib.h"
#include "mes/mes.h"

struct scm *
make_variable_type ()            /*:((internal)) */
{
  if (scm_variable_type == 0)
    {
      struct scm *record_type = cell_symbol_record_type;
      struct scm *fields = cell_nil;
      fields = cons (cstring_to_symbol ("value"), fields);
      fields = cons (fields, cell_nil);
      fields = cons (cell_symbol_variable, fields);
      scm_variable_type = make_struct (record_type, fields, cell_unspecified);
    }
  return scm_variable_type;
}

struct scm *
make_variable (struct scm *value)
{
  struct scm *type = make_variable_type ();
  struct scm *values = cell_nil;
  values = cons (value, values);
  values = cons (cell_symbol_variable, values);
  return make_struct (type, values, cstring_to_symbol ("variable-printer"));
}

struct scm *
variable_p (struct scm *x)
{
  struct scm *type = make_variable_type ();
  if (x->type == TSTRUCT)
    if (struct_ref_ (x, 0) == type)
      return cell_t;
  return cell_f;
}

struct scm *
variable_ref (struct scm *var)
{
  return struct_ref_ (var, 3);
}

struct scm *
variable_set_x (struct scm *var, struct scm *val)
{
  return struct_set_x_ (var, 3, val);
}

struct scm *
variable_printer (struct scm *var)
{
  fdputs ("#<variable ", __stdout);
  display_ (variable_ref (var));
  fdputc ('>', __stdout);
  return cell_unspecified;
}
