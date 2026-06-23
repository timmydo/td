;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2022, 2023 Timothy Sample <samplet@ngyro.com>
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

(define-module (srfi srfi-9 gnu)
  #:export (define-immutable-record-type
            set-field
            set-fields))

(include-from-path "srfi/srfi-9/gnu.mes")

(define-macro (%set-fields record field-path value . rest)
  (if (null? rest) `(set-field ,record ,field-path ,value)
      (let ((record* (gensym)))
        `(let ((,record* (set-field ,record ,field-path ,value)))
           (set-fields ,record* ,@rest)))))

(define-syntax-rule (set-fields record ((field sub-field ...) value) . rest)
  (%set-fields record (field sub-field ...) value . rest))
