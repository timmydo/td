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

(define-module (ice-9 regex)
  #:use-module (srfi srfi-9)
  #:export (make-regexp
            regexp?

            regexp/icase
            regexp/newline
            regexp/basic
            regexp/extended

            regexp-match?
            match:substring
            match:start
            match:end
            match:prefix
            match:suffix
            match:count
            match:string

            regexp/notbol
            regexp/noteol

            string-match
            regexp-exec
            fold-matches
            list-matches

            regexp-quote))

(include-from-path "ice-9/pregexp.upstream.scm")


;;; Patterns

(define-record-type <regexp>
  (tree->regexp tree)
  regexp?
  (tree regexp-tree))

(define (make-regexp pat . flags)
  (let loop ((flags flags) (icase #f) (newline #f) (type 'extended))
    (if (null? flags)
        (let ((pat* (if (eq? type 'basic) (bre->ere pat) pat)))
          (tree->regexp (pregexp pat*)))
        (cond
         ((eq? (car flags) regexp/icase)
          (error "make-regexp: Flag not implemented" (car flags)))
         ((eq? (car flags) regexp/newline)
          (error "make-regexp: Flag not implemented" (car flags)))
         ((eq? (car flags) regexp/basic)
          (loop (cdr flags) icase newline 'basic))
         ((eq? (car flags) regexp/extended)
          (loop (cdr flags) icase newline 'extended))
         (else (error "make-regexp: Invalid flag" (car flags)))))))

(define regexp/icase (list 'regexp/icase))
(define regexp/newline (list 'regexp/newline))
(define regexp/basic (list 'regexp/basic))
(define regexp/extended (list 'regexp/extended))


;;; Matches

(define-record-type <regexp-match>
  (make-regexp-match source positions)
  regexp-match?
  (source regexp-match-source)
  (positions regexp-match-positions))

(define* (match:substring m #:optional (n 0))
  (let* ((str (regexp-match-source m))
         (pos (list-ref (regexp-match-positions m) n))
         (start (car pos))
         (end (cdr pos)))
    (substring str start end)))

(define* (match:start m #:optional (n 0))
  (let ((pos (list-ref (regexp-match-positions m) n)))
    (car pos)))

(define* (match:end m #:optional (n 0))
  (let ((pos (list-ref (regexp-match-positions m) n)))
    (cdr pos)))

(define (match:prefix m)
  (let* ((str (regexp-match-source m))
         (pos (car (regexp-match-positions m)))
         (start (car pos)))
    (substring str 0 start)))

(define (match:suffix m)
  (let* ((str (regexp-match-source m))
         (pos (car (regexp-match-positions m)))
         (end (cdr pos)))
    (substring str end)))

(define (match:count m)
  (length (regexp-match-positions m)))

(define (match:string m)
  (regexp-match-source m))


;;; Searching

(define regexp/notbol 1)
(define regexp/noteol 2)

(define* (string-match pattern str #:optional (start 0))
  (let ((positions (pregexp-match-positions pattern str start)))
    (and positions
         (make-regexp-match str positions))))

(define* (regexp-exec rx str #:optional (start 0) (flags 0))
  (if (not (regexp? rx))
      (error "regexp-exec: Not a regular expression" rx))
  (let ((positions (pregexp-match-positions (regexp-tree rx) str start
                                            (string-length str) flags)))
    (and positions
         (make-regexp-match str positions))))

(define* (fold-matches regexp str init proc #:optional (flags 0))
  (let ((rx (if (regexp? regexp)
                regexp
                (make-regexp regexp))))
    (let loop ((k 0) (acc init))
      (define flags*
        (if (<= k 0) flags
            (logior flags regexp/notbol)))
      (if (> k (string-length str)) acc
          (let ((m (regexp-exec rx str k flags)))
            (if (not m) acc
                (if (= (match:start m) (match:end m))
                    (loop (+ k 1) (proc m acc))
                    (loop (match:end m) (proc m acc)))))))))

(define* (list-matches regexp str #:optional (flags 0))
  (reverse (fold-matches regexp str '() cons flags)))


;;; Convert BRE to ERE

(define* (bracket-end str #:optional (start 0) (end (string-length str)))
  (if (not (char=? (string-ref str start) #\[)) start
      (let loop ((k (+ start 1)) (depth 1))
        (cond
         ((zero? depth) k)
         ((>= k end) end)
         (else (let ((chr (string-ref str k)))
                 (cond
                  ((char=? chr #\\) (loop (+ k 2) depth))
                  ((char=? chr #\[)
                   (let ((nk (+ k 1)))
                     (if (>= nk end) end
                         (let ((nchr (string-ref str nk)))
                           (if (or (char=? nchr #\.)
                                   (char=? nchr #\=)
                                   (char=? nchr #\:))
                               (loop (+ nk 1) (+ depth 1))
                               (loop (+ k 1) depth))))))
                  ((char=? chr #\]) (loop (+ k 1) (- depth 1)))
                  (else (loop (+ k 1) depth)))))))))

(define ere-special?
  (let ((specials '(#\( #\) #\{ #\} #\| #\+ #\?)))
    (lambda (chr) (memv chr specials))))

(define ere-special-escape?
  (let ((specials '(#\( #\) #\{ #\})))
    (lambda (chr) (memv chr specials))))

(define* (bre->ere str #:optional (start 0) (end (string-length str)))
  (let loop ((k start) (acc '()))
    (if (>= k end) (list->string (reverse acc))
        (let ((chr (string-ref str k)))
          (cond
           ((char=? chr #\[)
            (let ((bend (bracket-end str k end)))
              (loop bend (string-fold cons acc str k bend))))
           ((char=? chr #\^)
            (if (= k start) (loop (+ k 1) (cons chr acc))
                (loop (+ k 1) (cons chr (cons #\\ acc)))))
           ((char=? chr #\$)
            (if (= (+ k 1) end) (loop (+ k 1) (cons chr acc))
                (loop (+ k 1) (cons chr (cons #\\ acc)))))
           ((char=? chr #\\)
            (let ((nk (+ k 1)))
              (if (>= nk end) (list->string (reverse (cons #\\ acc)))
                  (let ((nchr (string-ref str nk)))
                    (if (ere-special-escape? nchr)
                        (loop (+ nk 1) (cons nchr acc))
                        (loop (+ nk 1) (cons nchr (cons #\\ acc))))))))
           ((ere-special? chr)
            (loop (+ k 1) (cons chr (cons #\\ acc))))
           (else (loop (+ k 1) (cons chr acc))))))))


;;; Quoting

(define regexp-quote pregexp-quote)
