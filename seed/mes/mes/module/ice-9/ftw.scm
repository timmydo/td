;;; GNU Mes --- Maxwell Equations of Software
;;; Copyright © 2022,2023 Timothy Sample <samplet@ngyro.com>
;;; Copyright (C) 2002, 2003, 2006, 2011,
;;;     2012, 2014, 2016, 2018 Free Software Foundation, Inc.
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

(define-module (ice-9 ftw)
  #:use-module (ice-9 optargs)
  #:use-module (srfi srfi-1)
  #:use-module (srfi srfi-132)
  #:export (scandir
            file-system-fold))

(define* (scandir name #:optional (select? (lambda _ #t)) (entry<? string<))
  (let ((dir (opendir name)))
    (let loop ((acc '()))
      (cond
       ((readdir dir)
        => (lambda (n)
             (loop (if (select? n)
                       (cons n acc)
                       acc))))
       (else (closedir dir)
             (list-sort entry<? acc))))))

;;; The following 'file-system-fold' was adapted from Guile 3.0.

(define-syntax-rule (errno-if-exception expr)
  (catch 'system-error
    (lambda ()
      expr)
    (lambda args
      ;; Pffft.  We don't have errnos.  Return 95, which on GNU/Linux is
      ;; ENOTSUP.  It should be something that nobody will check for by
      ;; accident, I guess.
      95)))

(define (file->key name st)
  (cons (stat:dev st)
        (if (= 0 (stat:ino st))
            hash
            (stat:ino st))))

(define* (file-system-fold enter? leaf down up skip error init file-name
                           #:optional (stat lstat))
  "Traverse the directory at FILE-NAME, recursively.  Enter
sub-directories only when (ENTER? PATH STAT RESULT) returns true.  When
a sub-directory is entered, call (DOWN PATH STAT RESULT), where PATH is
the path of the sub-directory and STAT the result of (stat PATH); when
it is left, call (UP PATH STAT RESULT).  For each file in a directory,
call (LEAF PATH STAT RESULT).  When ENTER? returns false, call (SKIP
PATH STAT RESULT).  When an `opendir' or STAT call raises an exception,
call (ERROR PATH STAT ERRNO RESULT), with ERRNO being the operating
system error number that was raised.

Return the result of these successive applications.
When FILE-NAME names a flat file, (LEAF PATH STAT INIT) is returned.
The optional STAT parameter defaults to `lstat'."

  (define visited (make-hash-table))

  (define (mark! name st)
    (hash-set! visited (file->key name st) #t))

  (define (visited? name st)
    (hash-ref visited (file->key name st) #f))

  (let loop ((name     file-name)
             (path     "")
             (dir-stat (errno-if-exception (stat file-name)))
             (result   init))

    (define full-name
      (if (string=? path "")
          name
          (string-append path "/" name)))

    (cond
     ((integer? dir-stat)
      ;; FILE-NAME is not readable.
      (error full-name #f dir-stat result))
     ((visited? full-name dir-stat)
      result)
     ((eq? 'directory (stat:type dir-stat)) ; true except perhaps the 1st time
      (if (enter? full-name dir-stat result)
          (let ((dir (errno-if-exception (opendir full-name))))
            (mark! full-name dir-stat)
            (cond
             ((not (number? dir))   ; check that we didn’t get an errno.
              (let liip ((entry   (readdir dir))
                         (result  (down full-name dir-stat result))
                         (subdirs '()))
                (cond ((not entry)
                       (begin
                         (closedir dir)
                         (let ((r (fold (lambda (subdir result)
                                          (loop (car subdir)
                                                full-name
                                                (cdr subdir)
                                                result))
                                        result
                                        subdirs)))
                           (up full-name dir-stat r))))
                      ((or (string=? entry ".")
                           (string=? entry ".."))
                       (liip (readdir dir)
                             result
                             subdirs))
                      (else
                       (let* ((child (string-append full-name "/" entry))
                              (st    (errno-if-exception (stat child))))
                         (if (integer? st) ; CHILD is a dangling symlink?
                             (liip (readdir dir)
                                   (error child #f st result)
                                   subdirs)
                             (if (eq? (stat:type st) 'directory)
                                 (liip (readdir dir)
                                       result
                                       (alist-cons entry st subdirs))
                                 (liip (readdir dir)
                                       (leaf child st result)
                                       subdirs))))))))
             (else
              ;; Directory FULL-NAME not readable, but it is stat'able.
              (error full-name dir-stat dir result))))
          (mark! full-name dir-stat)
          (skip full-name dir-stat result)))
     (else
      ;; Caller passed a FILE-NAME that names a flat file, not a directory.
      (leaf full-name dir-stat result)))))
