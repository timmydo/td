/* -*-comment-start: "//";comment-end:""-*-
 * GNU Mes --- Maxwell Equations of Software
 * Copyright © 2016,2017,2018,2019,2020,2022 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
 * Copyright © 2023 Timothy Sample <samplet@ngyro.com>
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

/** Commentary:
    Scheme library functions not used by the eval/apply core.
 */

/** Code: */

#include "mes/lib.h"
#include "mes/mes.h"

#include <stdlib.h>

struct scm *
type_ (struct scm *x)
{
  return make_number (x->type);
}

struct scm *
car_ (struct scm *x)
{
  struct scm *a = x->car;
  if (x->type == TPAIR || x->type == TBINDING)
    return a;
  return make_number (cast_scmp_to_long (a));
}

struct scm *
cdr_ (struct scm *x)
{
  struct scm *d = x->cdr;
  if (x->type == TPAIR || x->type == TCLOSURE)
    return d;
  return make_number (cast_scmp_to_long (d));
}

struct scm *
xassq (struct scm *x, struct scm *a)            /* For speed in core. */
{
  while (a != cell_nil)
    {
      if (x == a->car->cdr)
        return a->car;
      a = a->cdr;
    }
  return cell_f;
}

struct scm *
memq (struct scm *x, struct scm *a)
{
  int t = x->type;
  if (t == TCHAR || t == TNUMBER)
    {
      long v = x->value;
      while (a != cell_nil)
        {
          if (v == a->car->value)
            return a;
          a = a->cdr;
        }
      return cell_f;
    }
  if (t == TKEYWORD)
    {
      while (a != cell_nil)
        {
          if (a->car->type == TKEYWORD)
            if (string_equal_p (x, a->car) == cell_t)
              return a;
          a = a->cdr;
        }
      return cell_f;
    }
  while (a != cell_nil)
    {
      if (x == a->car)
        return a;
      a = a->cdr;
    }
  return cell_f;
}

struct scm *
equal2_p (struct scm *a, struct scm *b)
{
  long i;
  struct scm *ai;
  struct scm *bi;

equal2:
  if (a == b)
    return cell_t;
  if (a->type == TPAIR && b->type == TPAIR)
    {
      if (equal2_p (a->car, b->car) == cell_t)
        {
          a = a->cdr;
          b = b->cdr;
          goto equal2;
        }
      return cell_f;
    }
  if (a->type == TSTRING && b->type == TSTRING)
    return string_equal_p (a, b);
  if (a->type == TVECTOR && b->type == TVECTOR)
    {
      if (a->length != b->length)
        return cell_f;
      for (i = 0; i < a->length; i = i + 1)
        {
          ai = cell_ref (a->vector, i);
          bi = cell_ref (b->vector, i);
          if (ai->type == TREF)
            ai = ai->ref;
          if (bi->type == TREF)
            bi = bi->ref;
          if (equal2_p (ai, bi) == cell_f)
            return cell_f;
        }
      return cell_t;
    }
  return eq_p (a, b);
}

struct scm *
last_pair (struct scm *x)
{
  while (x != cell_nil)
    {
      if (x->cdr == cell_nil)
        return x;
      x = x->cdr;
    }
  return x;
}

struct scm *
pair_p (struct scm *x)
{
  if (x->type == TPAIR)
    return cell_t;
  return cell_f;
}

struct scm *
char_to_integer (struct scm *x)
{
  return make_number (x->value);
}

struct scm *
integer_to_char (struct scm *x)
{
  return make_char (x->value);
}

struct scm *
make_bytevector (struct scm *args)
{
  struct scm *size;
  struct scm *init;
  int init_p = 0;

  if (args->type != TPAIR)
    error (cell_symbol_wrong_number_of_args,
           cstring_to_symbol ("make-bytevector"));

  size = args->car;
  args = args->cdr;

  if (size->type != TNUMBER)
    error (cell_symbol_wrong_type_arg,
           cons (size, cstring_to_symbol ("make-bytevector")));

  if (args->type == TPAIR)
    {
      init = args->car;
      args = args->cdr;

      if (init->type != TNUMBER)
        error (cell_symbol_wrong_type_arg,
               cons (size, cstring_to_symbol ("make-bytevector")));
      if (init->value < 0 || 256 <= init->value)
        error (cell_symbol_system_error,
               cons (make_string0 ("make-bytevector: value out of range"), init));

      init_p = 1;
    }

  if (args != cell_nil)
    error (cell_symbol_wrong_number_of_args,
           cstring_to_symbol ("make-bytevector"));

  struct scm *result = make_bytes (size->value);
  char *p = cell_bytes (result);
  long i;
  if (size->value == 0)
    p[0] = 0;
  else if (init_p)
    for (i = 0; i < size->value; i = i + 1)
      p[i] = init->value;

  return result;
}

struct scm *
bytevector_u8_ref (struct scm *bv, struct scm *k)
{
  char *p;
  if (bv->type != TBYTES)
    error (cell_symbol_wrong_type_arg,
           cons (bv, cstring_to_symbol ("bytevector-u8-ref")));
  if (k->type != TNUMBER)
    error (cell_symbol_wrong_type_arg,
           cons (k, cstring_to_symbol ("bytevector-u8-ref")));
  if (k->value < 0 || bv->length <= k->value)
    error (cell_symbol_system_error,
           cons (make_string0 ("bytevector-u8-ref: index out of range"), k));
  p = cell_bytes (bv);

  /* Try to be portable across signed and unsigned char (while
     respecting the limitions of M2 and MesCC, which precludes using
     'unsigned char' or 'uint8_t'). */
  long i = p[k->value];
  if (i < 0)
    return make_number (256 + i);
  else
    return make_number (i);
}

struct scm *
bytevector_u8_set_x (struct scm *bv, struct scm *k, struct scm *value)
{
  char *p;
  if (bv->type != TBYTES)
    error (cell_symbol_wrong_type_arg,
           cons (bv, cstring_to_symbol ("bytevector-u8-set!")));
  if (k->type != TNUMBER)
    error (cell_symbol_wrong_type_arg,
           cons (k, cstring_to_symbol ("bytevector-u8-set!")));
  if (k->value < 0 || bv->length <= k->value)
    error (cell_symbol_system_error,
           cons (make_string0 ("bytevector-u8-set!: index out of range"), k));
  if (value->type != TNUMBER)
    error (cell_symbol_wrong_type_arg,
           cons (value, cstring_to_symbol ("bytevector-u8-set!")));
  if (value->value < 0 || 256 <= value->value)
    error (cell_symbol_system_error,
           cons (make_string0 ("bytevector-u8-set!: value out of range"), value));
  p = cell_bytes (bv);

  /* Try to be portable across signed and unsigned char (while
     respecting the limitions of M2 and MesCC, which precludes using
     'unsigned char' or 'uint8_t'). */
  char c = -1;
  if (c < 255 && value->value >= 128)
    p[k->value] = value->value - 256;
  else
    p[k->value] = value->value;

  return cell_unspecified;
}

#if 0
void
assert_type (long type, char const *name_name, struct scm *x)
{
  if (x->type != type)
    {
      eputs (name);
      eputs (": ");
      error (cell_wrong_type_arg, cons (x, cell_nil));
    }
}
#endif

void
assert_num (long pos, struct scm *x)
{
  if (x->type != TNUMBER)
    error (cell_symbol_wrong_type_arg,
           cons (cell_type_number, cons (make_number (pos), x)));
}

void
assert_struct (long pos, struct scm *x)
{
  if (x->type != TSTRUCT)
    error (cell_symbol_wrong_type_arg,
           cons (cell_type_struct, cons (make_number (pos), x)));
}

void
assert_range (int assert, long i)
{
  if (assert == 0)
    {
      eputs ("value out of range: ");
      eputs (ltoa (i));
      eputs (": ");
      assert_msg (assert, "value out of range");
    }
}
