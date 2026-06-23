/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2016,2017,2018,2019,2020,2022,2025 Janneke Nieuwenhuizen <janneke@gnu.org>
 * Copyright © Timothy Sample 2022 <samplet@ngyro.com>
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

#include <string.h>

struct scm *
assert_defined (struct scm *x, struct scm *e)   /*:((internal)) */
{
  if (e == cell_undefined)
    return error (cell_symbol_unbound_variable, x);
  return e;
}

struct scm *
check_formals (struct scm *f, struct scm *formals, struct scm *args)    /*:((internal)) */
{
  long flen;
  if (formals->type == TNUMBER)
    flen = formals->value;
  else
    flen = length__ (formals);
  long alen = length__ (args);
  if (alen != flen && alen != -1 && flen != -1)
    {
      char *s = "apply: wrong number of arguments; expected: ";
      eputs (s);
      eputs (itoa (flen));
      eputs (", got: ");
      eputs (itoa (alen));
      eputs ("\n");
      write_error_ (f);
      struct scm *e = make_string0 (s);
      return error (cell_symbol_wrong_number_of_args, cons (e, f));
    }
  return cell_unspecified;
}

struct scm *
check_apply (struct scm *f, struct scm *e)      /*:((internal)) */
{
  char *type = 0;
  if (f == cell_f || f == cell_t)
    type = "bool";
  if (f == cell_nil)
    type = "nil";
  if (f == cell_unspecified)
    type = "*unspecified*";
  if (f == cell_undefined)
    type = "*undefined*";
  if (f->type == TCHAR)
    type = "char";
  if (f->type == TNUMBER)
    type = "number";
  if (f->type == TSTRING)
    type = "string";
  if (f->type == TSTRUCT && builtin_p (f) == cell_f)
    type = "#<...>";
  if (f->type == TBROKEN_HEART)
    type = "<3";

  if (type != 0)
    {
      char *s = "cannot apply: ";
      eputs (s);
      eputs (type);
      eputs ("[");
      write_error_ (e);
      eputs ("]\n");
      struct scm *e = make_string0 (s);
      return error (cell_symbol_wrong_type_arg, cons (e, f));
    }
  return cell_unspecified;
}

struct scm *
pairlis (struct scm *x, struct scm *y, struct scm *a)
{
  if (x == cell_nil)
    return a;
  if (x->type != TPAIR)
    return cons (cons (x, y), a);
  return cons (cons (car (x), car (y)), pairlis (cdr (x), cdr (y), a));
}

struct scm *
set_car_x (struct scm *x, struct scm *e)
{
  if (x->type != TPAIR)
    error (cell_symbol_not_a_pair, cons (x, cstring_to_symbol ("set-car!")));
  x->car = e;
  return cell_unspecified;
}

struct scm *
set_cdr_x (struct scm *x, struct scm *e)
{
  if (x->type != TPAIR)
    error (cell_symbol_not_a_pair, cons (x, cstring_to_symbol ("set-cdr!")));
  x->cdr = e;
  return cell_unspecified;
}

struct scm *
set_x (struct scm *x, struct scm *e, int define_p)
{
  struct scm *binding;

  if (x->type == TBINDING)
    binding = x;
  else
    binding = lookup_binding (x, cell_f);

  if (binding == cell_f)
    return error (cell_symbol_unbound_variable, x);

  if (binding->lexical_p != 0)
    set_cdr_x (binding->binding, e);
  else
    {
      struct scm *variable = binding->binding->cdr;
      if (define_p == 0 && variable_ref (variable) == cell_undefined)
        return error (cell_symbol_unbound_variable, binding->binding->car);
      variable_set_x (binding->binding->cdr, e);
    }

  return cell_unspecified;
}

struct scm *
call_lambda (struct scm *e, struct scm *x, struct scm *aa)       /*:((internal)) */
{
  struct scm *cl = cons (cons (cell_closure, x), x);
  R1 = e;
  R0 = cl;
  return cell_unspecified;
}

struct scm *
make_closure_ (struct scm *args, struct scm *body, struct scm *a)       /*:((internal)) */
{
  return make_cell (TCLOSURE, cell_f, cons (cons (cell_circular, a), cons (args, body)));
}

struct scm *
make_binding_ (struct scm *handle, long lexical_p)      /*:((internal)) */
{
  struct scm *binding = make_cell (TBINDING, handle, 0);
  binding->lexical_p = lexical_p;
  return binding;
}

struct scm *
make_binding (struct scm *name, struct scm *variable)
{
  return make_binding_ (cons (name, variable), 0);
}

struct scm *
macro_get_handle (struct scm *name)     /*:((internal)) */
{
  if (name->type == TSYMBOL)
    return hashq_get_handle (g_macros, name);
  return cell_f;
}

struct scm *
get_macro (struct scm *name)            /*:((internal)) */
{
  struct scm *m = macro_get_handle (name);
  if (m != cell_f)
    {
      struct scm *d = m->cdr;
      return d->macro;
    }
  return cell_f;
}

struct scm *
macro_set_x (struct scm *name, struct scm *value)       /*:((internal)) */
{
  return hashq_set_x (g_macros, name, value);
}

struct scm *
push_cc (struct scm *p1, struct scm *p2, struct scm *a, struct scm *c)  /*:((internal)) */
{
  struct scm *x = R3;
  R3 = c;
  R2 = p2;
  gc_push_frame ();
  R1 = p1;
  R0 = a;
  R3 = x;
  return cell_unspecified;
}

struct scm *
lookup_binding (struct scm *name, struct scm *define_p)
{
  struct scm *handle = assq (name, R0);
  if (handle != cell_f)
    return make_binding_ (handle, 1);

  struct scm *variable = current_module_variable (name, define_p);
  if (variable != cell_f)
    return make_binding_ (cons (name, variable), 0);

  return cell_f;
}

struct scm *
lookup_value (struct scm *name)
{
  struct scm *binding = lookup_binding (name, cell_f);
  if (binding != cell_f)
    {
      if (binding->lexical_p != 0)
        return binding->binding->cdr;
      else
        return variable_ref (binding->binding->cdr);
    }
  return cell_undefined;
}

struct scm *
add_formals (struct scm *formals, struct scm *x)
{
  while (x->type == TPAIR)
    {
      formals = cons (x->car, formals);
      x = x->cdr;
    }
  if (x->type == TSYMBOL)
    formals = cons (x, formals);
  return formals;
}

int
formal_p (struct scm *x, struct scm *formals)   /*:((internal)) */
{
  if (formals->type == TSYMBOL)
    {
      if (x == formals)
        return 1;
      else
        return 0;
    }
  while (formals->type == TPAIR)
    {
      if (formals->car == x)
        break;
      formals = formals->cdr;
    }
  if (formals->type == TSYMBOL)
    return formals == x;
  return formals->type == TPAIR;
}

void
expand_variable_ (int top_p)        /*:((internal)) */
{
  struct scm *a;
  struct scm *f;
  struct scm *v;

  /* First add all internal definitions to the current list of
     formals. */
  v = R1;
  while (v->type == TPAIR)
    {
      a = v->car;
      if (a == cell_symbol_quote)
        break;
      if (a->type == TPAIR)
        {
          if (a->car == cell_symbol_define || a->car == cell_symbol_define_macro)
            {
              if (a->cdr->car->type == TPAIR)
                R2 = cons (a->cdr->car->car, R2);
              else
                R2 = cons (a->cdr->car, R2);
            }
        }
      v = v->cdr;
    }

  /* Now do the expansion. */
  while (R1->type == TPAIR)
    {
      a = R1->car;
      if (a->type == TPAIR)
        R3 = cons (cons (a, R2), R3);
      else
        {
          if (a == cell_symbol_lambda)
            {
              f = R1->cdr->car;
              R2 = add_formals (R2, f);
              R1 = R1->cdr;
            }
          else if (a == cell_symbol_define || a == cell_symbol_define_macro)
            {
              f = R1->cdr->car;
              if (top_p != 0)
                {
                  if (f->type == TPAIR)
                    f = f->cdr;
                  else
                    f = cell_nil;
                }
              R2 = add_formals (R2, f);
              R1 = R1->cdr;
            }
          else if (a == cell_symbol_quote)
            return;
          else if (a->type == TSYMBOL
                   && a != cell_symbol_current_environment
                   && formal_p (a, R2) == 0)
            {
              v = lookup_binding (a, cell_f);
              if (v != cell_f)
                R1->car = v;
              else
                {
                  /* Lookup the binding again, but this time make a
                     local definition in case the name becomes bound in
                     the future. */
                  R1->car = lookup_binding (a, cell_t);
                }
            }
        }
      R1 = R1->cdr;
      top_p = 0;
    }
}

struct scm *
expand_variable (struct scm *x, struct scm *formals)    /*:((internal)) */
{
  gc_push_frame ();

  R1 = x;
  R2 = formals;
  R3 = cell_nil;
  expand_variable_ (1);

  while (R3->type == TPAIR)
    {
      R1 = R3->car->car;
      R2 = R3->car->cdr;
      R3 = R3->cdr;
      expand_variable_ (0);
    }

  gc_pop_frame ();

  return cell_unspecified;
}

struct scm *
apply_builtin (struct scm *fn, struct scm *x)   /*:((internal)) */
{
  struct scm *a = builtin_arity (fn);
  struct scm *d;
  int arity = a->value;
  if ((arity > 0 || arity == -1) && x != cell_nil)
    {
      a = x->car;
      if (a->type == TVALUES)
        x = cons (a->cdr->car, x->cdr);
    }
  if ((arity > 1 || arity == -1) && x != cell_nil)
    {
      a = x->car;
      d = x->cdr;
      if (d->type == TPAIR)
        if (d->car->type == TVALUES)
          x = cons (a, cons (d->car->cdr->car, d));
    }

  if (arity == 0)
    return apply_builtin0 (fn);
  if (arity == 1)
    return apply_builtin1 (fn, x->car);
  else if (arity == 2)
    return apply_builtin2 (fn, x->car, x->cdr->car);
  else if (arity == 3)
    return apply_builtin3 (fn, x->car, x->cdr->car, x->cdr->cdr->car);
  else if (arity == -1)
    return apply_builtin1 (fn, x);

  return cell_unspecified;
}

struct scm *
eval_apply ()
{
  struct scm *aa;
  struct scm *args;
  struct scm *body;
  struct scm *cl;
  struct scm *entry;
  struct scm *expanders;
  struct scm *formals;
  struct scm *input;
  struct scm *name;
  struct scm *macro;
  struct scm *p;
  struct scm *program;
  struct scm *sc_expand;
  struct scm *v;
  struct scm *x;
  int global_p;
  int macro_p;
  struct scm *a;
  struct scm *c;
  struct scm *d;
  int t;
  long i;

eval_apply:
  if (R3 == cell_vm_evlis2)
    goto evlis2;
  else if (R3 == cell_vm_evlis3)
    goto evlis3;
  else if (R3 == cell_vm_eval_check_func)
    goto eval_check_func;
  else if (R3 == cell_vm_eval2)
    goto eval2;
  else if (R3 == cell_vm_apply2)
    goto apply2;
  else if (R3 == cell_vm_if_expr)
    goto if_expr;
  else if (R3 == cell_vm_begin_eval)
    goto begin_eval;
  else if (R3 == cell_vm_eval_set_x)
    goto eval_set_x;
  else if (R3 == cell_vm_macro_expand_car)
    goto macro_expand_car;
  else if (R3 == cell_vm_return)
    goto vm_return;
  else if (R3 == cell_vm_macro_expand_cdr)
    goto macro_expand_cdr;
  else if (R3 == cell_vm_eval_define)
    goto eval_define;
  else if (R3 == cell_vm_macro_expand)
    goto macro_expand;
  else if (R3 == cell_vm_macro_expand_lambda)
    goto macro_expand_lambda;
  else if (R3 == cell_vm_begin_expand_macro)
    goto begin_expand_macro;
  else if (R3 == cell_vm_macro_expand_define)
    goto macro_expand_define;
  else if (R3 == cell_vm_begin_expand_eval)
    goto begin_expand_eval;
  else if (R3 == cell_vm_call_with_current_continuation2)
    goto call_with_current_continuation2;
  else if (R3 == cell_vm_macro_expand_set_x)
    goto macro_expand_set_x;
  else if (R3 == cell_vm_macro_expand_define_macro)
    goto macro_expand_define_macro;
  else if (R3 == cell_vm_evlis)
    goto evlis;
  else if (R3 == cell_vm_apply)
    goto apply;
  else if (R3 == cell_vm_eval)
    goto eval;
  else if (R3 == cell_vm_eval_macro_expand_eval)
    goto eval_macro_expand_eval;
  else if (R3 == cell_vm_eval_macro_expand_expand)
    goto eval_macro_expand_expand;
  else if (R3 == cell_vm_begin)
    goto begin;
  else if (R3 == cell_vm_begin_expand)
    goto begin_expand;
  else if (R3 == cell_vm_if)
    goto vm_if;
  else if (R3 == cell_vm_call_with_values2)
    goto call_with_values2;
  else if (R3 == cell_unspecified)
    return R1;
  else
    assert_msg (0, "eval/apply unknown continuation");

evlis:
  if (R1 == cell_nil)
    goto vm_return;
  if (R1->type != TPAIR)
    goto eval;
  push_cc (R1->car, R1, R0, cell_vm_evlis2);
  goto eval;
evlis2:
  push_cc (R2->cdr, R1, R0, cell_vm_evlis3);
  goto evlis;
evlis3:
  R1 = cons (R2, R1);
  goto vm_return;

apply:
  g_stack_array[g_stack + GC_FRAME_PROCEDURE] = R1->car;
  a = R1->car;
  t = a->type;
  if (t == TSTRUCT && builtin_p (R1->car) == cell_t)
    {
      check_formals (R1->car, builtin_arity (R1->car), R1->cdr);
      R1 = apply_builtin (R1->car, R1->cdr);
      goto vm_return;
    }
  else if (t == TCLOSURE)
    {
      cl = R1->car->closure;
      body = cl->cdr->cdr;
      formals = cl->cdr->car;
      args = R1->cdr;
      aa = cl->car->cdr;
      aa = aa->cdr;
      check_formals (R1->car, formals, R1->cdr);
      p = pairlis (formals, args, aa);
      call_lambda (body, p, aa);
      goto begin;
    }
  else if (t == TCONTINUATION)
    {
      a = R1->car;
      v = a->continuation;
      if (v->length != 0)
        {
          for (i = 0; i < v->length; i = i + 1)
            g_stack_array[STACK_SIZE - v->length + i] = vector_ref_ (v, i);
          g_stack = STACK_SIZE - v->length;
        }
      R1 = R1->cdr->car;
      goto vm_return;
    }
  else if (t == TSPECIAL)
    {
      c = R1->car;
      if (c == cell_vm_apply)
        {
          push_cc (cons (R1->cdr->car, R1->cdr->cdr->car), R1, R0, cell_vm_return);
          goto apply;
        }
      else if (c == cell_vm_eval)
        {
          push_cc (R1->cdr->car, R1, R1->cdr->cdr->car, cell_vm_return);
          goto eval;
        }
      else if (c == cell_vm_begin_expand)
        {
          push_cc (cons (R1->cdr->car, cell_nil), R1, R1->cdr->cdr->car, cell_vm_return);
          goto begin_expand;
        }
      else
        check_apply (cell_f, R1->car);
    }
  else if (t == TSYMBOL)
    {
      c = R1->car;
      if (c == cell_symbol_call_with_current_continuation)
        {
          R1 = R1->cdr;
          goto call_with_current_continuation;
        }
      if (c == cell_symbol_call_with_values)
        {
          R1 = R1->cdr;
          goto call_with_values;
        }
      if (c == cell_symbol_current_environment)
        {
          R1 = R0;
          goto vm_return;
        }
    }
  else if (t == TPAIR)
    {
      if (R1->car->car == cell_symbol_lambda)
        {
          formals = R1->car->cdr->car;
          args = R1->cdr;
          body = R1->car->cdr->cdr;
          p = pairlis (formals, R1->cdr, R0);
          check_formals (R1, formals, args);
          call_lambda (body, p, p);
          goto begin;
        }
    }
  push_cc (R1->car, R1, R0, cell_vm_apply2);
  goto eval;
apply2:
  check_apply (R1, R2->car);
  R1 = cons (R1, R2->cdr);
  goto apply;

eval:
  t = R1->type;
  if (t == TPAIR)
    {
      c = R1->car;

      /* If the car is a bound identifier, we want to dispatch on the
         value to which it is bound. */
      if (c->type == TBINDING)
        {
          if (c->lexical_p != 0)
            R1->car = c->binding->cdr;
          else
            R1->car = variable_ref (c->binding->cdr);

          if (R1->car == cell_undefined)
            return error (cell_symbol_unbound_variable, c->binding->car);
        }

      c = R1->car;
      if (c == cell_symbol_quote)
        {
          R1 = R1->cdr->car;
          goto vm_return;
        }
      else if (c == cell_symbol_begin)
        goto begin;
      else if (c == cell_symbol_lambda)
        {
          R1 = make_closure_ (R1->cdr->car, R1->cdr->cdr, R0);
          goto vm_return;
        }
      else if (c == cell_symbol_if)
        {
          R1 = R1->cdr;
          goto vm_if;
        }
      else if (c == cell_symbol_set_x)
        {
          push_cc (R1->cdr->cdr->car, R1, R0, cell_vm_eval_set_x);
          goto eval;
        eval_set_x:
          R1 = set_x (R2->cdr->car, R1, 0);
          goto vm_return;
        }
      else if (c == cell_vm_macro_expand)
        {
          push_cc (R1->cdr->car, R1, R0, cell_vm_eval_macro_expand_eval);
          goto eval;
        eval_macro_expand_eval:
          push_cc (R1, R2, R0, cell_vm_eval_macro_expand_expand);
          goto macro_expand;
        eval_macro_expand_expand:
          goto vm_return;
        }
      else
        {
          if (R1->type == TPAIR)
            if (R1->car == cell_symbol_define || R1->car == cell_symbol_define_macro)
              {
                global_p = 0;
                if (R0->car->car != cell_closure)
                  global_p = 1;
                macro_p = 0;
                if (R1->car == cell_symbol_define_macro)
                  macro_p = 1;
                if (global_p != 0)
                  {
                    name = R1->cdr->car;
                    aa = R1->cdr->car;
                    if (aa->type == TPAIR)
                      name = name->car;
                    if (macro_p != 0)
                      {
                        entry = macro_get_handle (name);
                        if (entry == cell_f)
                          macro_set_x (name, cell_f);
                      }
                    else
                      /* Ensure this name is bound in the current
                         module. */
                      lookup_binding (name, cell_t);
                  }
                R2 = R1;
                aa = R1->cdr->car;
                if (aa->type != TPAIR)
                  {
                    push_cc (R1->cdr->cdr->car, R2, cons (cons (R1->cdr->car, R1->cdr->car), R0), cell_vm_eval_define);
                    goto eval;
                  }
                else
                  {
                    formals = R1->cdr->car->cdr;
                    body = R1->cdr->cdr;

                    if (macro_p != 0 || global_p != 0)
                      expand_variable (body, formals);

                    /* The GC may have moved 'formals' and 'body' during
                       variable expansion, so get fresh pointers. */
                    formals = R1->cdr->car->cdr;
                    body = R1->cdr->cdr;
                    p = pairlis (R1->cdr->car, R1->cdr->car, R0);

                    R1 = cons (cell_symbol_lambda, cons (formals, body));
                    push_cc (R1, R2, p, cell_vm_eval_define);
                    goto eval;
                  }
              eval_define:
                /* These may have been clobbered by an inline define
                   during evaluation, so they must be recomputed. */
                global_p = 0;
                if (R0->car->car != cell_closure)
                  global_p = 1;
                macro_p = 0;
                if (R2->car == cell_symbol_define_macro)
                  macro_p = 1;

                name = R2->cdr->car;
                aa = R2->cdr->car;
                if (aa->type == TPAIR)
                  name = name->car;
                if (macro_p != 0)
                  {
                    entry = macro_get_handle (name);
                    R1 = make_macro (name, R1);
                    set_cdr_x (entry, R1);
                  }
                else if (global_p != 0)
                  {
                    set_x (name, R1, 1);
                  }
                else
                  {
                    entry = cons (name, R1);
                    aa = cons (entry, cell_nil);
                    /* Push the definition onto the current lexical
                       environment, but keep the first element (named
                       '*closure*') pointing to the rest of the
                       environment. */
                    set_cdr_x (aa, cdr (R0));
                    set_cdr_x (R0, aa);
                    set_cdr_x (car (R0), aa);
                  }
                R1 = cell_unspecified;
                goto vm_return;
              }
          push_cc (R1->car, R1, R0, cell_vm_eval_check_func);
          gc_check ();
          goto eval;
        eval_check_func:
          push_cc (R2->cdr, R2, R0, cell_vm_eval2);
          goto evlis;
        eval2:
          R1 = cons (R2->car, R1);
          goto apply;
        }
    }
  else if (t == TSYMBOL)
    {
      if (R1 == cell_symbol_current_environment)
        goto vm_return;
      if (R1 == cell_symbol_begin)
        goto vm_return;
      if (R1 == cell_symbol_call_with_current_continuation)
        goto vm_return;
      R1 = assert_defined (R1, lookup_value (R1));
      goto vm_return;
    }
  else if (t == TBINDING)
    {
      name = R1->binding->car;

      if (R1->lexical_p != 0)
        R1 = R1->binding->cdr;
      else
        R1 = variable_ref (R1->binding->cdr);

      if (R1 == cell_undefined)
        return error (cell_symbol_unbound_variable, name);

      goto vm_return;
    }
  else if (t == TBROKEN_HEART)
    error (cell_symbol_system_error, R1);
  else
    goto vm_return;

macro_expand:
  if (R1->type != TPAIR || R1->car == cell_symbol_quote)
    goto vm_return;

  if (R1->car == cell_symbol_lambda)
    {
      push_cc (R1->cdr->cdr, R1, R0, cell_vm_macro_expand_lambda);
      goto macro_expand;
    macro_expand_lambda:
      R2->cdr->cdr = R1;
      R1 = R2;
      goto vm_return;
    }

  if (R1->car == cell_symbol_define || R1->car == cell_symbol_define_macro)
    {
      push_cc (R1->cdr->cdr, R1, R0, cell_vm_macro_expand_define);
      goto macro_expand;
    macro_expand_define:
      R2->cdr->cdr = R1;
      R1 = R2;
      if (R1->car == cell_symbol_define_macro)
        {
          push_cc (R1, R1, R0, cell_vm_macro_expand_define_macro);
          goto eval;
        macro_expand_define_macro:
          R1 = R2;
        }
      goto vm_return;
    }

  if (R1->car == cell_symbol_set_x)
    {
      push_cc (R1->cdr->cdr, R1, R0, cell_vm_macro_expand_set_x);
      goto macro_expand;
    macro_expand_set_x:
      R2->cdr->cdr = R1;
      R1 = R2;
      goto vm_return;
    }

  macro = get_macro (R1->car);
  if (macro != cell_f)
    {
      R1 = cons (macro, R1->cdr);
      push_cc (R1, cell_nil, R0, cell_vm_macro_expand);
      goto apply;
    }
  a = R1->car;
  if (a->type == TSYMBOL && a != cell_symbol_begin)
    {
      macro = macro_get_handle (cell_symbol_portable_macro_expand);
      if (macro != cell_f)
        {
          expanders = lookup_value (cell_symbol_sc_expander_alist);
          if (expanders != cell_undefined)
            {
              macro = assq (R1->car, expanders);
              if (macro != cell_f)
                {
                  sc_expand = lookup_value (cell_symbol_macro_expand);
                  R2 = R1;
                  if (sc_expand != cell_undefined && sc_expand != cell_f)
                    {
                      R1 = cons (sc_expand, cons (R1, cell_nil));
                      goto apply;
                    }
                }
            }
        }
    }

  push_cc (R1->car, R1, R0, cell_vm_macro_expand_car);
  goto macro_expand;

macro_expand_car:
  R2->car = R1;
  R1 = R2;
  if (R1->cdr == cell_nil)
    goto vm_return;

  push_cc (R1->cdr, R1, R0, cell_vm_macro_expand_cdr);
  goto macro_expand;

macro_expand_cdr:
  R2->cdr = R1;
  R1 = R2;

  goto vm_return;

begin:
  x = cell_unspecified;
  while (R1 != cell_nil)
    {
      gc_check ();
      if (R1->type == TPAIR)
        {
          a = R1->car;
          if (a->type == TPAIR)
            {
              if (a->car == cell_symbol_begin)
                R1 = append2 (a->cdr, R1->cdr);
            }
        }
      if (R1->cdr == cell_nil)
        {
          R1 = R1->car;
          goto eval;
        }
      push_cc (R1->car, R1, R0, cell_vm_begin_eval);
      goto eval;
    begin_eval:
      x = R1;
      R1 = R2->cdr;
    }
  R1 = x;
  goto vm_return;


begin_expand:
  x = cell_unspecified;
  while (R1 != cell_nil)
    {
    begin_expand_while:
      gc_check ();

      if (R1->type == TPAIR)
        {
          a = R1->car;
          if (a->type == TPAIR)
            if (R1->car->car == cell_symbol_begin)
              R1 = append2 (R1->car->cdr, R1->cdr);
        }

      push_cc (R1->car, R1, R0, cell_vm_begin_expand_macro);
      goto macro_expand;
    begin_expand_macro:
      if (R1 != R2->car)
        {
          R2->car = R1;
          R1 = R2;
          goto begin_expand_while;
          continue; /* FIXME: M2-PLanet */
        }
      R1 = R2;

      /* If we expand a 'define' form that refers to itself, and it also
         reuses a name higher up in the module chain, the self-reference
         will be bound to the variable from the other module.  We hack
         around this by binding the name in the current module before
         expansion.  */
      if (R1->type == TPAIR
          && R1->car->type == TPAIR
          && R1->car->car == cell_symbol_define)
        {
          /* Split the '&&' expression up to prevent a segfault when
             compiled with M2-Planet. */
          if (R1->car->cdr->type == TPAIR
              && R1->car->cdr->car->type == TPAIR)
            lookup_binding (R1->car->cdr->car->car, cell_t);
        }


      expand_variable (R1->car, cell_nil);
      push_cc (R1->car, R1, R0, cell_vm_begin_expand_eval);
      goto eval;
    begin_expand_eval:
      x = R1;
      R1 = R2->cdr;
    }
  R1 = x;
  goto vm_return;

vm_if:
  push_cc (R1->car, R1, R0, cell_vm_if_expr);
  goto eval;
if_expr:
  x = R1;
  R1 = R2;
  if (x != cell_f)
    {
      R1 = R1->cdr->car;
      goto eval;
    }
  if (R1->cdr->cdr != cell_nil)
    {
      R1 = R1->cdr->cdr->car;
      goto eval;
    }
  R1 = cell_unspecified;
  goto vm_return;

call_with_current_continuation:
  x = make_continuation (g_continuations);
  g_continuations = g_continuations + 1;
  v = make_vector_ (STACK_SIZE - g_stack, cell_unspecified);
  for (i = g_stack; i < STACK_SIZE; i = i + 1)
    vector_set_x_ (v, i - g_stack, g_stack_array[i]);
  x->continuation = v;
  push_cc (cons (R1->car, cons (x, cell_nil)), x, R0, cell_vm_call_with_current_continuation2);
  goto apply;
call_with_current_continuation2:
  v = make_vector_ (STACK_SIZE - g_stack, cell_unspecified);
  for (i = g_stack; i < STACK_SIZE; i = i + 1)
    vector_set_x_ (v, i - g_stack, g_stack_array[i]);
  R2->continuation = v;
  goto vm_return;

call_with_values:
  push_cc (cons (R1->car, cell_nil), R1, R0, cell_vm_call_with_values2);
  goto apply;
call_with_values2:
  if (R1->type == TVALUES)
    R1 = R1->cdr;
  else
    R1 = cons (R1, cell_nil);
  R1 = cons (R2->cdr->car, R1);
  goto apply;

vm_return:
  x = R1;
  gc_pop_frame ();
  R1 = x;
  goto eval_apply;
}

struct scm *
apply (struct scm *f, struct scm *x, struct scm *a)     /*:((internal)) */
{
  push_cc (cons (f, x), cell_unspecified, a, cell_unspecified);
  R3 = cell_vm_apply;
  return eval_apply ();
}

struct scm *
primitive_load (struct scm *filename)     /*:((arity . 1))*/
{
  struct scm *input;

  if ((filename->type == TNUMBER) && filename->value == 0)
    input = current_input_port ();
  else if (filename->type == TSTRING)
    input = set_current_input_port (open_input_file (filename));
  else if (filename->type == TPORT)
    input = set_current_input_port (filename);
  else
    {
      eputs ("primitive_load failed, filename=");
      display_error_ (filename);
      assert_msg (0, "primitive-load-boom 0");
    }

  struct scm *forms = read_input_file_env (cell_nil);
  forms = cons (cell_symbol_begin, forms);
  set_current_input_port (input);

  struct scm *env = cell_nil;
  env = cons (cons (cell_symbol_program, forms), env);

  gc_push_frame ();
  /* Store 'input' in R2 so it does not get GCed during evaluation. */
  push_cc (forms, cell_unspecified, env, cell_unspecified);
  R3 = cell_vm_begin_expand;
  struct scm *result = eval_apply ();
  input = R2;
  gc_pop_frame ();

  return result;
}
