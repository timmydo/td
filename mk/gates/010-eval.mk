# 1. Config eval — load every module; catches syntax/binding errors in well
#    under a second, before any expensive build. Run as a repl SCRIPT, NOT piped
#    via STDIN: `guix repl` reading from STDIN always exits 0 (swallows the
#    script's status), which made a broken module pass `eval` green. `guix repl
#    FILE` honors the exit code, so a load error reddens this gate honestly.
CHEAP_GATES += eval
eval:
	@echo ">> eval: load the load-bearing system modules (td-builder, td-build)"
	$(GUIX) repl $(LOAD) tests/eval.scm
