# 3. Boot + behavioral — realise the marionette test derivation. Its builder
#    runs the SRFI-64 assertions in/against a booted VM and exits non-zero if any
#    fail, so a failed assertion makes this gate go red (see the two-step note in
#    the recipe for why we must NOT pipe the build into `guix repl`).
HEAVY_GATES += test
test:
	@echo ">> test: boot marionette + assert behaviors"
	$(call realise-system-test,(tests boot),%test-td-boot,boot)
