;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2016,2017,2018,2019,2020,2021,2022,2023,2024,2025 Janneke Nieuwenhuizen <janneke@gnu.org>
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

(define-module (mes repl)
  #:use-module (mes mes-0)
  #:use-module (srfi srfi-14)
  #:export (repl))

(define welcome
  (string-append "GNU Mes " %version "
Copyright (C) 2016,2017,2018,2019,2020,2021,2022,2023,2024,2025 Janneke Nieuwenhuizen <janneke@gnu.org>
Copyright (C) 2019,2020,2021 Danny Milosavljevic <dannym@scratchpost.org>
Copyright (C) 2021 Wladimir van der Laan <laanwj@protonmail.com>
Copyright (C) 2022,2023 Timothy Sample <samplet@ngyro.com>
Copyright (C) 2022,2023,2025 Ekaitz Zarraga <ekaitz@elenq.tech>
Copyright (C) 2023,2025 Andrius Štikonas <andrius@stikonas.eu>
and others.

GNU Mes comes with ABSOLUTELY NO WARRANTY; for details type `,show w'.
This program is free software, and you are welcome to redistribute it
under certain conditions; type `,show c' for details.

Enter `,help' for help.
"))

(define warranty
"GNU Mes is distributed WITHOUT ANY WARRANTY.  The following
sections from the GNU General Public License, version 3, should
make that clear.

  15. Disclaimer of Warranty.

  THERE IS NO WARRANTY FOR THE PROGRAM, TO THE EXTENT PERMITTED BY
APPLICABLE LAW.  EXCEPT WHEN OTHERWISE STATED IN WRITING THE COPYRIGHT
HOLDERS AND/OR OTHER PARTIES PROVIDE THE PROGRAM \"AS IS\" WITHOUT WARRANTY
OF ANY KIND, EITHER EXPRESSED OR IMPLIED, INCLUDING, BUT NOT LIMITED TO,
THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
PURPOSE.  THE ENTIRE RISK AS TO THE QUALITY AND PERFORMANCE OF THE PROGRAM
IS WITH YOU.  SHOULD THE PROGRAM PROVE DEFECTIVE, YOU ASSUME THE COST OF
ALL NECESSARY SERVICING, REPAIR OR CORRECTION.

  16. Limitation of Liability.

  IN NO EVENT UNLESS REQUIRED BY APPLICABLE LAW OR AGREED TO IN WRITING
WILL ANY COPYRIGHT HOLDER, OR ANY OTHER PARTY WHO MODIFIES AND/OR CONVEYS
THE PROGRAM AS PERMITTED ABOVE, BE LIABLE TO YOU FOR DAMAGES, INCLUDING ANY
GENERAL, SPECIAL, INCIDENTAL OR CONSEQUENTIAL DAMAGES ARISING OUT OF THE
USE OR INABILITY TO USE THE PROGRAM (INCLUDING BUT NOT LIMITED TO LOSS OF
DATA OR DATA BEING RENDERED INACCURATE OR LOSSES SUSTAINED BY YOU OR THIRD
PARTIES OR A FAILURE OF THE PROGRAM TO OPERATE WITH ANY OTHER PROGRAMS),
EVEN IF SUCH HOLDER OR OTHER PARTY HAS BEEN ADVISED OF THE POSSIBILITY OF
SUCH DAMAGES.

  17. Interpretation of Sections 15 and 16.

  If the disclaimer of warranty and limitation of liability provided
above cannot be given local legal effect according to their terms,
reviewing courts shall apply local law that most closely approximates
an absolute waiver of all civil liability in connection with the
Program, unless a warranty or assumption of liability accompanies a
copy of the Program in return for a fee.

See <http://www.gnu.org/licenses/gpl.html>, for more details.
")

(define copying
"GNU Mes is free software; you can redistribute it and/or modify it
under the terms of the GNU General Public License as published by
the Free Software Foundation; either version 3 of the License, or (at
your option) any later version.

GNU Mes is distributed in the hope that it will be useful, but
WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with GNU Mes.  If not, see <http://www.gnu.org/licenses/>.
")

(define help-commands
  "Help Commands:

  ,expand SEXP         - Expand SEXP
  ,help                - Show this help
  ,quit                - Quit this session
  ,show TOPIC          - Show info on TOPIC [c, w]
  ,use MODULE          - load MODULE
")

(define show-commands
  "Show commands:

  ,show c              - Show details on licensing; GNU GPLv3+
  ,show w              - Show details on the lack of warranty
")

(define (repl)
  (let ((count 0)
        (print-sexp? #t))

    (define (expand a)
      (lambda ()
        (let ((sexp (read)))
          (when #t print-sexp?
                (display "[sexp=")
                (display sexp)
                (display "]")
                (newline))
          (core:macro-expand sexp))))

    (define (load-env file-name a)
      (push! *input-ports* (current-input-port))
      (set-current-input-port
       (open-input-file file-name))
      (let ((x (core:eval (append2 (cons 'begin (read-input-file-env a))
                                   '((current-module)))
                          a)))
        (set-current-input-port (pop! *input-ports*))
        x))

    (define (mes-load-module-env module a)
      (push! *input-ports* (current-input-port))
      (set-current-input-port
       (open-input-file (string-append %moduledir (module->file module))))
      (let ((x (core:eval (append2 (cons 'begin (read-input-file-env a))
                                   '((current-module)))
                          a)))
        (set-current-input-port (pop! *input-ports*))
        x))

    (define (help . x) (display help-commands) *unspecified*)
    (define (show . x)
      (define topic-alist `((#\newline . ,show-commands)
                            (#\c . ,copying)
                            (#\w . ,warranty)))
      (let* ((word (read-env '()))
             (topic (find (negate char-whitespace?) (symbol->list word))))
        (display (assoc-ref topic-alist topic))
        *unspecified*))
    (define (quit . x)
      (exit 0))
    (define (use a)
      (lambda ()
        (let ((name (read)))
          (let ((mod (resolve-interface name)))
            (if mod (module-use! (current-module) mod)
                (simple-format (current-error-port)
                               "No such module: ~A~%" name)))
          name)))
    (define (meta command a)
      (let ((command-alist `((expand . ,(expand a))
                             (help . ,help)
                             (quit . ,quit)
                             (show . ,show)
                             (use . ,(use a)))))
        ((or (assoc-ref command-alist command)
             (lambda () #f)))))

    (display welcome)
    (let loop ((a (current-environment)))
      (display "mes> ")
      (force-output)
      (catch #t
        (lambda ()
          (let ((sexp (read-env a)))
            (when (not (eq? sexp '()))
              (when print-sexp?
                (display "[sexp=")
                (display sexp)
                (display "]")
                (newline))
              (cond
               ((and (pair? sexp) (eq? (car sexp) 'mes-use-module))
                (let ((module (cadr sexp)))
                  (mes-load-module-env module a)
                  (loop a)))
               ((and (pair? sexp) (memq (car sexp) '(include load)))
                (load-env (cadr sexp) a)
                (loop a))
               (else
                (let ((e (if (and (pair? sexp) (eq? (car sexp) (string->symbol "unquote")))
                             (meta (cadr sexp) a)
                             (core:eval sexp a))))
                  (if (eq? e *unspecified*) (loop a)
                      (let ((id (string->symbol (string-append "$" (number->string count)))))
                        (set! count (+ count 1))
                        (display id)
                        (display " = ")
                        (write e)
                        (newline)
                        (loop (acons id e a))))))))))
        (lambda (key . args)
          (if (defined? 'with-output-to-string)
              (simple-format (current-error-port) "exception: ~a: ~s\n" key args)
              (begin
                (display "exception: " (current-error-port))
                (display key (current-error-port))
                (display ": " (current-error-port))
                (write args (current-error-port))
                (newline (current-error-port))))
          (loop a))))))
