;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2016,2017,2018,2019,2024 Janneke Nieuwenhuizen <janneke@gnu.org>
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

(define-module (mes main)
  #:use-module (mes getopt-long)
  #:use-module (mes mes-0)
  #:use-module (mes repl)
  #:use-module (srfi srfi-1)
  #:export (%main
            top-main))


(define %main #f)
(define (top-main)
  (let ((tty? (isatty? 0)))
    (define (parse-opts args)
      (let* ((option-spec
              '((no-auto-compile)
                (command (single-char #\c) (value #t))
                (compiled-path (single-char #\C) (value #t))
                (help (single-char #\h))
                (load-path (single-char #\L) (value #t))
                (main (single-char #\e) (value #t))
                (source (single-char #\s) (value #t))
                (version (single-char #\V)))))
        (getopt-long args option-spec #:stop-at-first-non-option #t)))
    (define (source-arg? o)
      (equal? "-s" o))
    (let* ((s-index (list-index source-arg? %argv))
           (args (if s-index (list-head %argv (+ s-index 2)) %argv))
           (options (parse-opts args))
           (command (option-ref options 'command #f))
           (main (option-ref options 'main #f))
           (source (option-ref options 'source #f))
           (files (if s-index (list-tail %argv (+ s-index 1))
                      (option-ref options '() '())))
           (help? (option-ref options 'help #f))
           (usage? #f)
           (version? (option-ref options 'version #f)))
      (or
       (and version?
            (display (string-append "mes (GNU Mes) " %version "\n"))
            (exit 0))
       (and (or help? usage?)
            (display "Usage: mes [OPTION]... [FILE]...
Evaluate code with Mes, interactively or from a script.

  [-s] FILE            load source code from FILE, and exit
  -c EXPR              evalute expression EXPR, and exit
  --                   stop scanning arguments; run interactively

The above switches stop argument processing, and pass all
remaining arguments as the value of (command-line).

  -e, --main=MAIN      after reading script, apply MAIN to command-line arguments
  -h, --help           display this help and exit
  -L, --load-path=DIR  add DIR to the front of the module load path
  -v, --version        display version information and exit

Ignored for Guile compatibility:
  --auto-compile
  --fresh-auto-compile
  --no-auto-compile
  -C, --compiled-path=DIR

Report bugs to: bug-mes@gnu.org
GNU Mes home page: <http://gnu.org/software/mes/>
General help using GNU software: <http://gnu.org/gethelp/>
" (or (and usage? (current-error-port)) (current-output-port)))
            (exit (or (and usage? 2) 0)))
       options)
      (and=> (option-ref options 'load-path #f)
             (lambda (dir)
               (setenv "GUILE_LOAD_PATH" (string-append dir ":" (getenv "GUILE_LOAD_PATH")))))
      (when command
        (let* ((prev (set-current-input-port (open-input-string command)))
               (expr (cons 'begin (read-input-file-env (current-module))))
               (set-current-input-port prev))
          (primitive-eval expr)
          (exit 0)))
      (when main
        (let ((proc-name (string->symbol main)))
          (set! %main (lambda () (apply proc-name (command-line) '())))))
      (cond ((pair? files)
             (let ((file (car files)))
               (set! %argv files)
               (if (equal? file "-") (primitive-load 0)
                   (primitive-load file))))
            ((and (null? files) tty?)

             (mes-use-module (mes repl))
             (set-current-input-port 0)
             (repl))
            (else #t))
      (when %main
        (%main)))))
