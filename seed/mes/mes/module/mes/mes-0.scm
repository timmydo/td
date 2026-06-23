;;; -*-scheme-*-

;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2016,2018,2021 Jan (janneke) Nieuwenhuizen <janneke@gnu.org>
;;; Copyright © 2022 Timothy Sample <samplet@ngyro.com>
;;;
;;; mes-0.scm: This file is part of GNU Mes.
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

;;; mes-0.scm is the first file being loaded into Guile.  It provides
;;; non-standard definitions that Mes modules and tests depend on.

;;; Code:

(define-module (mes mes-0)
  #:re-export (mes?
               guile?)
  #:export (mes-use-module
            guile-1.8?
            guile-2?))

(define-macro (mes-use-module . rest) #t)

(define guile-1.8? #f)
(define guile-2? #f)
