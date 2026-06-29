# M10.2 guix-free placer (M10-design.md step 3, "Place"). The deployment side:
# a POSIX shell tool (system/td-place.sh) that runs ON THE TARGET — which has NO
# guix. Driven by the OCI manifest (not a blind layer scan), it: verifies the
# image's embedded identity (boot/td-identity) matches the --generation/--root-label
# it is placed as; APPLIES the userspace layers into that generation's own root,
# staged as roots/td/gen-N/root.tar (so the bare-label root=td-root-gen-N refers to a root
# that exists — M10.3 turns it into a labeled fs); extracts /boot per-generation;
# prunes to --keep (>=1); and regenerates a per-generation GRUB menu. Each
# generation is staged + validated then atomically swapped in, so a corrupt image
# never destroys the generation already installed. This gate exercises it
# hermetically (system/td-place.scm): it builds the per-generation bootc images
# with Guix (the M10.1 oracle) and runs the placer over them inside a derivation
# whose builder PATH is ONLY base tools, NO guix — so a successful build PROVES the
# placer is guix-free by construction (the same "absent → cannot be used" guarantee
# as `no-guix`), and `--check` proves the placed target tree reproducible. The
# deployment behavior is tested against the artifact (M10-design.md decision 2),
# not diffed against a Guix component it lacks: tests/place-check.scm cracks the
# tree and asserts each present generation is placed with its own kernel/initrd,
# an identity recording the artifact's sha256 (image-digest=, M12 §2.7 —
# value-checked against the real artifacts via TD_IMAGES),
# its applied root content, and a menuentry that selects its OWN root and no
# other's (per-entry, not block-wide), the user grub.cfg preamble survives, and
# (the prune scenario) the oldest generation's boot dir, root content AND menu
# entry are gone. Two scenarios: PLACE (gens 1,2 keep 10 — no prune) and PRUNE
# (gens 1,2,3 keep 2 — gen 1 dropped). Creating the labeled fs from the staged
# root.tar + the full boot+rollback is M10.3.
SYSTEM_GATES += place
place:
	@echo ">> place: guix-free placer extracts /boot + writes a per-generation GRUB menu, prunes old generations (M10.2)"
	@set -euo pipefail; \
	place_drv=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh '((@@ (guix store) run-with-store) s ((@ (system td-place) td-placed-tree) #:gens (quote (1 2)) #:keep 10))' 2>/dev/null`; \
	prune_drv=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh '((@@ (guix store) run-with-store) s ((@ (system td-place) td-placed-tree) #:gens (quote (1 2 3)) #:keep 2))' 2>/dev/null`; \
	test -n "$$place_drv" -a -n "$$prune_drv" || { echo "ERROR: could not lower the placer tree derivations" >&2; exit 1; }; \
	img1=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh --out '((@@ (guix store) run-with-store) s ((@ (system td-generation) td-generation-image) ((@ (system td-typed) td-config) #:generation 1)))' 2>/dev/null`; \
	img2=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh --out '((@@ (guix store) run-with-store) s ((@ (system td-generation) td-generation-image) ((@ (system td-typed) td-config) #:generation 2)))' 2>/dev/null`; \
	img3=`TD_GUIX="$(GUIX)" sh tools/guix-lower.sh --out '((@@ (guix store) run-with-store) s ((@ (system td-generation) td-generation-image) ((@ (system td-typed) td-config) #:generation 3)))' 2>/dev/null`; \
	test -n "$$img1" -a -n "$$img2" -a -n "$$img3" || { echo "ERROR: could not lower the generation image artifact paths" >&2; exit 1; }; \
	echo ">> place  tree derivation (gens 1,2 keep 10): $$place_drv"; \
	echo ">> prune  tree derivation (gens 1,2,3 keep 2): $$prune_drv"; \
	place_tree=`$(GUIX) build "$$place_drv"`; \
	prune_tree=`$(GUIX) build "$$prune_drv"`; \
	echo ">> check: reproducibility of BOTH placed target trees (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$place_drv" "$$prune_drv"; \
	echo ">> validate PLACE tree (gens 1,2 present, none pruned)"; \
	TD_PLACED="$$place_tree" TD_PRESENT="1 2" TD_ABSENT="" \
	TD_IMAGES="1=$$img1 2=$$img2" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	echo ">> validate PRUNE tree (gens 2,3 present, gen 1 pruned)"; \
	TD_PLACED="$$prune_tree" TD_PRESENT="2 3" TD_ABSENT="1" \
	TD_IMAGES="2=$$img2 3=$$img3" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm
