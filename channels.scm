;; Pinned Guix channel — the reproducibility anchor for td.
;;
;; Everything builds against this exact commit. Bump it deliberately, never
;; silently (see CLAUDE.md "Repo conventions" and DESIGN.md §1.2). Captured from
;; `guix describe` so it matches the local store at project start.
(list (channel
       (name 'guix)
       (url "https://git.guix.gnu.org/guix.git")
       (branch "master")
       (commit "520785e315eddbe47199ac557e88e60eca3ae97c")))
