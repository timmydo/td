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

(define-module (srfi srfi-132)
  #:use-module (srfi srfi-43)
  #:export (list-sort
            vector-sort!))

(define (list-sort < lis)
  (vector->list (vector-sort! < (list->vector lis))))

(define (vector-sort! < v)
  (let ((t (vector-copy v)))
    (%vector-sort! < v t 0 (vector-length v))
    v))

(define (%vector-sort! < v1 v2 start end)
  (let ((len (- end start)))
    (unless (<= len 1)
      (let ((mid (+ start (quotient len 2))))
        (%vector-sort! < v2 v1 start mid)
        (%vector-sort! < v2 v1 mid end)
        (%vector-merge! < v1 v2 v2 start start mid mid end)))))

(define (%vector-merge! < to from1 from2 start
                        start1 end1 start2 end2)
  (let loop ((start start) (start1 start1) (start2 start2))
    (cond
     ((= start1 end1) (vector-copy! to start from2 start2 end2))
     ((= start2 end2) (vector-copy! to start from1 start1 end1))
     (else (let ((x1 (vector-ref from1 start1))
                 (x2 (vector-ref from2 start2)))
             (if (< x2 x1)
                 (begin
                   (vector-set! to start x2)
                   (loop (+ start 1) start1 (+ start2 1)))
                 (begin
                   (vector-set! to start x1)
                   (loop (+ start 1) (+ start1 1) start2))))))))
