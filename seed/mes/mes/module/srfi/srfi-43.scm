;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2016,2018,2020 Janneke Nieuwenhuizen <janneke@gnu.org>
;;; Copyright © 2023, 2023 Timothy Sample <samplet@ngyro.com>
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

;;; Commentary:

;;; Minimal implementation of srfi-43, for nyacc.

;;; Code:

(define-module (srfi srfi-43)
  #:export (vector-map
            vector-for-each
            vector-fold
            vector-copy!)
  #:re-export (vector-copy))

(define (vector-map f v)
  (let* ((k (vector-length v))
         (n (make-vector k)))
    (let loop ((i 0))
      (if (= i k) n
          (begin
            (vector-set! n i (f i (vector-ref v i)))
            (loop (+ i 1)))))))

(define (vector-for-each f v)
  (let ((k (vector-length v)))
    (let loop ((i 0))
      (if (< i k)
          (begin
            (f i (vector-ref v i))
            (loop (+ i 1)))))))

(define (vector-fold kons knil vec)
  (let loop ((k 0) (acc knil))
    (if (>= k (vector-length vec)) acc
        (loop (+ k 1) (kons k acc (vector-ref vec k))))))

(define (vector-copy! target tstart source sstart send)
  (let loop ((tk tstart) (sk sstart))
    (when (< sk send)
      (vector-set! target tk (vector-ref source sk))
      (loop (+ tk 1) (+ sk 1)))))
