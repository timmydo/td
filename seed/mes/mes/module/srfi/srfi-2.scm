;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2021 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
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

;;; Commentary:

;;; srfi-2.scm is included but not used by Nyacc

;;; Code:

(define-module (srfi srfi-2)
  #:export (and-let*))

(define-macro (and-let* bindings . body)
  (if (null? bindings)
      `(begin . ,body)
      (let ((binding (car bindings)))
        (cond
         ((pair? (cdr binding))
          (let ((name (car binding))
                (value (cadr binding))
                (rest (cdr bindings)))
            `((lambda (,name)
                (if ,name
                    (and-let* ,rest . ,body)
                    #f))
              ,value)))
         (else
          (let ((value (car binding))
                (rest (cdr bindings)))
            `((if ,value
                  (and-let* ,rest . ,body)
                  #f))))))))
