;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2023 Timothy Sample <samplet@ngyro.com>
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

(define-module (rnrs bytevectors)
  #:export (bytevector-copy!
            u8-list->bytevector
            bytevector->u8-list))

(define (bytevector-copy! source sstart target tstart count)
  (let loop ((sk sstart) (tk tstart) (n 0))
    (if (< n count)
        (begin
          (bytevector-u8-set! target tk (bytevector-u8-ref source sk))
          (loop (+ sk 1) (+ tk 1) (+ n 1))))))

(define (u8-list->bytevector lst)
  (let ((bv (make-bytevector (length lst))))
    (let loop ((k 0) (lst lst))
      (if (null? lst)
          bv
          (begin
            (bytevector-u8-set! bv k (car lst))
            (loop (+ k 1) (cdr lst)))))))

(define (bytevector->u8-list bv)
  (let loop ((k (- (bytevector-length bv) 1)) (lst '()))
    (if (< k 0)
        lst
        (loop (- k 1) (cons (bytevector-u8-ref bv k) lst)))))
