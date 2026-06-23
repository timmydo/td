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

;;; Commentary:
;;;
;;; MesCC uses the '(mes guile)' module to provide compatibility shims
;;; so that Guile looks more like Mes.  We stub it out here because Mes
;;; already is Mes.  We could re-export symbols, but MesCC doesn't care
;;; one way or the other.
;;;
;;; Code:

(define-module (mes guile))
