;;; -*- scheme -*-

;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2020 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
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

(define-module (ice-9 rdelim)
  #:export (read-line))

;;; Commentary:

(define (%read-line handle-delim)
  (let loop ((acc '()))
    (define c (read-char))
    (cond
     ((eof-object? c)
      (if (null? acc) c (list->string (reverse! acc))))
     ((eq? c #\newline)
      (case handle-delim
        ((trim) (list->string (reverse! acc)))
        ((concat) (list->string (reverse! (cons c acc))))
        ((peek) (begin (unget-char c) (list->string (reverse! acc))))
        ((split) (cons (list->string (reverse! acc)) c))
        (else (error "read-line: Invalid handle-delim" handle-delim))))
     (else
      (loop (cons c acc))))))

(define* (read-line #:optional (port (current-input-port))
                    (handle-delim 'trim))
  (if (eq? port (current-input-port))
      (%read-line handle-delim)
      (with-input-from-port port
        (lambda ()
          (%read-line handle-delim)))))
