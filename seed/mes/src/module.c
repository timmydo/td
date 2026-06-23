/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2018,2019,2022 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
 * Copyright © 2022 Timothy Sample <samplet@ngyro.com>
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
make_initial_module (struct scm *a)     /*:((internal)) */
{
  struct scm *var;
  struct scm *module = make_hash_table_ (100);
  while (a->type == TPAIR)
    {
      var = make_variable (a->car->cdr);
      hashq_set_x (module, a->car->car, var);
      a = a->cdr;
    }
  return module;
}

struct scm *
initial_module ()
{
  return M0;
}

struct scm *
current_module ()
{
  return M1;
}

struct scm *
set_current_module (struct scm *module)
{
  struct scm *previous = M1;
  M1 = module;
  return previous;
}

struct scm *
current_module_variable (struct scm *name, struct scm *define_p)
{
  struct scm *module = current_module ();

  /* When '(current-module)' is false, that means the module system is
     not yet booted.  In that case, we lookup variables in the initial
     module hash table. */
  if (module == cell_f)
    {
      module = initial_module ();
      struct scm *variable = hashq_ref_ (module, name, cell_f);
      if (variable == cell_f && define_p != cell_f)
        return hashq_set_x (module, name, make_variable (cell_undefined));
      else
        return variable;
    }

  /* The module system is booted.  We can use the current module's
     'eval-closure' procedure.  We take it on faith that whatever is in
     'M1' is a module. */
  struct scm *eval_closure = struct_ref_ (module, MODULE_EVAL_CLOSURE);

  /* If the module's "eval-closure" is the standard one, we can save
     time by performing the lookup without calling into Scheme code. */
  if (eval_closure == cell_symbol_standard_eval_closure)
    return standard_eval_closure (name, define_p);
  else if (eval_closure == cell_symbol_standard_interface_eval_closure)
    return standard_interface_eval_closure (name, define_p);

  /* Otherwise, we assume it's a closure, and defer to it for the
     lookup. */
  struct scm *args = cell_nil;
  args = cons (define_p, args);
  args = cons (name, args);
  /* XXX: Calling 'apply' does not restore the registers properly.  We
     work around it here, but maybe it should be fixed in 'apply'. */
  gc_push_frame ();
  struct scm *result = apply (eval_closure, args, cell_nil);
  gc_pop_frame ();
  return result;
}

struct scm *
standard_eval_closure (struct scm *name, struct scm *define_p)
{
  if (define_p != cell_f)
    return module_make_local_var_x (M1, name);
  return module_variable (M1, name);
}

struct scm *
standard_interface_eval_closure (struct scm *name, struct scm *define_p)
{
  if (define_p != cell_f)
    return cell_f;
  return module_variable (M1, name);
}

struct scm *
module_make_local_var_x (struct scm *module, struct scm *name)
{
  struct scm *obarray = struct_ref_ (module, MODULE_OBARRAY);
  struct scm *variable = make_variable (cell_undefined);
  struct scm *handle = hashq_create_handle_x (obarray, name, variable);

  /* TODO: Call 'module-modified' to invoke obervers, but only if there
     are observers, since we are trying to avoid Scheme code. */

  return handle->cdr;
}

struct scm *
module_variable (struct scm *module, struct scm *name)
{
  struct scm *modules = cons (module, cell_nil);
  struct scm *obarray;
  struct scm *variable;
  struct scm *uses;
  while (modules->type == TPAIR)
    {
      module = modules->car;
      obarray = struct_ref_ (module, MODULE_OBARRAY);
      variable = hashq_ref_ (obarray, name, cell_f);
      if (variable != cell_f)
          return variable;

      /* TODO: Call binders. */

      uses = struct_ref_ (module, MODULE_USES);
      modules = append2 (uses, modules->cdr);
    }
  return cell_f;
}
