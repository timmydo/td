# store-native-profile — prove `td-builder profile --store-native` assembles a profile of
# LOGICAL /td/store symlinks that RESOLVE + RUN inside a store-ns own-root with /gnu/store
# ABSENT: the .scm-free userspace ASSEMBLY mechanism (no guix operating-system). The tool is
# bash-static (the cheap store-ns runner pattern); the guix-FREE /td/store-native userland the
# toolchain builds (#192/#197) joins this same mechanism.
# Heavy: builds the guix-free stage0 td-builder + runs a rootless userns (like store-ns 386).
HEAVY_GATES += store-native-profile
store-native-profile:
	@echo ">> store-native-profile: td-builder profile --store-native builds a profile of logical /td/store links that resolve + run in the store-ns own-root, /gnu/store ABSENT (the .scm-free userspace assembly mechanism)"
	sh tests/store-native-profile.sh
