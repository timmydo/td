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

(define-module (srfi srfi-9)
  #:re-export (make-record-type
               record-type?
               record-type-name
               record-type-descriptor
               record-type-fields

               record-constructor
               record-predicate
               record-accessor
               record-modifier)
  #:export (define-record-type))

;; Macros have their own namespace, so this one cannot be simply be
;; re-exported.  Nyacc specifically imports this symbol, so we need to
;; provide it.  Because macros have their own namespace, it doesn't
;; matter what we bind it to.
(define define-record-type #f)
