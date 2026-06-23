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

(define-module (rnrs io ports)
  #:use-module (ice-9 optargs)
  #:use-module (rnrs bytevectors)
  #:export (get-bytevector-n
            get-bytevector-n!
            get-bytevector-all
            put-bytevector))

(define (eof-object)
  (integer->char -1))

(define (get-bytevector-n port count)
  (let* ((bv (make-bytevector count))
         (n (get-bytevector-n! port bv 0 count)))
    (cond
     ((eof-object? n) n)
     ((< n count) (let ((bv* (make-bytevector n)))
                    (bytevector-copy! bv 0 bv* 0 n)
                    bv*))
     (else bv))))

(define (get-bytevector-n! port bv start count)
  (with-input-from-port port
    (lambda ()
      (let loop ((k start) (n 0))
        (if (>= n count) n
            (let ((b (read-byte)))
              (cond ((< b 0)
                     ;; end of file for 'read-byte' is '-1'.
                     (if (zero? n) (eof-object)
                      n))
                    (else
                     (bytevector-u8-set! bv k b)
                     (loop (+ k 1) (+ n 1))))))))))

(define (get-bytevector-all port)
  (define (bytevector-concatenate-reverse total-length bvs)
    (let ((tbv (make-bytevector total-length)))
      (let loop ((k total-length) (bvs bvs))
        (if (null? bvs) tbv
            (let* ((sbv (car bvs))
                   (slen (bytevector-length sbv)))
              (bytevector-copy! sbv 0 tbv (- k slen) slen)
              (loop (- k slen) (cdr bvs)))))))
  (let loop ((len 0) (acc '()))
    (let ((bv (get-bytevector-n port 65536)))
      (if (eof-object? bv) (bytevector-concatenate-reverse len acc)
          (loop (+ len (bytevector-length bv)) (cons bv acc))))))

(define* (put-bytevector port bv #:optional (start 0)
                         (count (- (bytevector-length bv) start)))
  (with-output-to-port port
    (lambda ()
      (let loop ((k start) (n 0))
        (when (< n count)
          (write-byte (bytevector-u8-ref bv k))
          (loop (+ k 1) (+ n 1)))))))
