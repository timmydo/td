;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2020 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
;;; Copyright © 2022,2023 Timothy Sample <samplet@ngyro.com>
;;;
;;; This file is part of GNU Mes.
;;;
;;; GNU Mes is free software; you can redistribute it and/or modify it
;;; under the terms of the GNU General Public License as published by
;;; the Free Software Foundation; either version 3 of the License, or (at
;;; your option) any later version.
;;;
;;; GNU Mes is distributed in the hope that it will be useful, but
;;; WITHOUT ANY WARRANTY; without even the implied warranty of
;;; MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
;;; GNU General Public License for more details.
;;;
;;; You should have received a copy of the GNU General Public License
;;; along with GNU Mes.  If not, see <http://www.gnu.org/licenses/>.

(define-module (srfi srfi-14)
  #:use-module (srfi srfi-1)
  #:re-export (char-set
               char-set?
               char-set=
               char-set:whitespace
               char-set:digit
               char-set:upper-case
               list->char-set
               string->char-set
               string->char-set!
               char-set-adjoin
               char-set-contains?
               char-set-complement
               char-whitespace?
               char-set-copy
               char-upcase
               char-downcase)
  #:export (char-set:ascii
            char-set:full
            char-set:letter+digit
            char-set:letter
            char-set:blank
            char-set:punctuation
            char-set:symbol
            char-set:graphic
            char-set-complement!
            char-set-union
            char-set-intersection
            char-set-difference))

(define char-set:ascii (apply char-set (map integer->char (iota 128))))

(define char-set:full char-set:ascii)

(define char-set:letter+digit
  (apply char-set
         (append (map (lambda (x)
                        (integer->char (+ x (char->integer #\0))))
                      (iota 10))
                 (map (lambda (x)
                        (integer->char (+ x (char->integer #\A))))
                      (iota 26))
                 (map (lambda (x)
                        (integer->char (+ x (char->integer #\a))))
                      (iota 26)))))

(define char-set:letter
  (apply char-set
         (append (map (lambda (x)
                        (integer->char (+ x (char->integer #\A))))
                      (iota 26))
                 (map (lambda (x)
                        (integer->char (+ x (char->integer #\a))))
                      (iota 26)))))

(define char-set:blank
  (char-set #\tab #\space))

(define char-set:punctuation
  (string->char-set "!\"#%&'()*,-./:;?@[\\]_{}"))

(define char-set:symbol
  (string->char-set "$+<=>^`|~"))

(define char-set-complement! char-set-complement)

(define (char-set-union . char-sets)
  (apply char-set
         (apply lset-union
                char=?
                (map (lambda (cs)
                       (unless (char-set? cs)
                         (error "char-set-union: not a char-set: " cs))
                       (cdr cs))
                     char-sets))))

(define (char-set-intersection . char-sets)
  (apply char-set
         (apply lset-intersection
                char=?
                (map (lambda (cs)
                       (unless (char-set? cs)
                         (error "char-set-intersection: not a char-set: " cs))
                       (cdr cs))
                     char-sets))))

(define (char-set-difference . char-sets)
  (apply char-set
         (apply lset-difference
                char=?
                (map (lambda (cs)
                       (unless (char-set? cs)
                         (error "char-set-difference: not a char-set: " cs))
                       (cdr cs))
                     char-sets))))

(define char-set:graphic
  (char-set-union char-set:letter
                  char-set:digit
                  char-set:punctuation
                  char-set:symbol))
