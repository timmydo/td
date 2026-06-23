;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2022 Timothy Sample <samplet@ngyro.com>
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

(define-module (ice-9 i18n)
  #:export (locale-string->integer))

(define* (locale-string->integer str #:optional (base 10))
  (define cn-zero (char->integer #\0))
  (define cn-nmax (min (- (+ cn-zero base) 1) (char->integer #\9)))
  (define cn-a1 (char->integer #\a))
  (define cn-a2 (char->integer #\A))
  (define cn-amax1 (- (+ cn-a1 base) 11))
  (define cn-amax2 (- (+ cn-a2 base) 11))

  (define (sign? c)
    (or (char=? c #\+) (char=? c #\-)))

  (define (digit? c)
    (let ((cn (char->integer c)))
      (or (<= cn-zero cn cn-nmax)
          (<= cn-a1 cn cn-amax1)
          (<= cn-a2 cn cn-amax2))))

  (let* ((start (string-index str (negate char-whitespace?)))
         (dstart (and start
                      (if (sign? (string-ref str start))
                          (+ start 1)
                          start)))
         (end (or (and dstart
                       (string-index str (negate digit?) dstart))
                  (string-length str))))
    (cond
     ((not start) (values #f 0))
     ((string->number (substring str start end) base)
      => (lambda (n) (values n (if n end 0))))
     (else (values #f 0)))))
