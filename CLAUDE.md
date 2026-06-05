# CLAUDE.md — <PROJECT_NAME>

You are building a functional Linux distribution incrementally on top of an existing
Guix system. You grow the OS *inside* a verification loop: you do not get credit for
code, only for a passing, reproducible test. Read `DESIGN.md` for the target and scope.

This file is your contract. The rules below are absolute and override any local
convenience.

## Prime directives (never violate)

1. **Reproducibility is a test.** Every artifact must pass `guix build --check`. A
   non-reproducible build is a FAILING test. Never commit a non-reproducible artifact.
2. **Hermeticity.** No undeclared dependencies. Builds run offline except declared
   fixed-output fetches. Never make a build pass by reaching outside the container,
   adding an undeclared dependency, or disabling `--check`.
3. **Never weaken a test to pass it.** Do not skip, delete, comment out, loosen, or
   `xfail` a test to turn a task green. If a test cannot pass honestly, STOP and report.
4. **Differential testing before replacement.** Never replace a Guix component
   (`guix-daemon`, store, config language, etc.) without first proving behavioral
   equivalence to the original — build the same thing both ways and diff the store
   paths. The existing component is the oracle.
5. **Stay in scope.** Build only the current milestone. The "out of scope for v0" list
   in `DESIGN.md` is binding. Do not start later layers early.
6. **Respect the state boundary.** Do not stash mutable state to make something work.
   What is declared-and-immutable vs. writable is defined in `DESIGN.md`.

## The loop

Your inner loop for every change:

```
write/change declaration
  -> evaluate config            (fails fast, sub-second)
  -> guix build --check TARGET  (reproducibility oracle)
  -> boot marionette system test (boot + behavioral assertion)
  -> reset VM state
```

Run all of it with the single pass/fail command:

```
<FILL IN: e.g. `make check` — must exit non-zero on any failure>
```

Run every build/test inside a fresh container so your own environment cannot
contaminate results:

```
guix shell -C --pure -- <command>
```

Do not proceed to the next sub-task until the current one is green.

## Definition of done (every task)

A task is done only when ALL hold:

- a test asserts the new behavior and passes,
- the build is reproducible (`guix build --check` passes),
- the change is the smallest increment that turns one test green,
- it is committed with a message stating what test now passes.

If any are missing, the task is not done.

## Workflow

1. Read the current milestone and its acceptance test in `DESIGN.md`.
2. Before writing implementation, state the sub-task and write (or name) the test that
   will verify it.
3. Make the smallest change that turns that test green.
4. Run the loop. If red, fix forward — never by weakening the test.
5. Commit a small increment. Move to the next sub-task.

Prefer many small green commits over one large change. If a change spans layers, split
it.

## When stuck or blocked

- If a test cannot pass honestly, STOP and report what is blocking — do not fake,
  stub-to-green, or disable.
- If a build needs something not declared, declare it properly; do not reach outside the
  container.
- If a task seems to require crossing into an out-of-scope layer, STOP and ask — do not
  expand scope on your own.
- Spec correctness, security/adversarial verification, and real-hardware behavior are
  human-reviewed. Flag milestones that touch them for sign-off rather than merging on
  green.

## Repo conventions

- *(FILL IN: directory layout, where declarations live, where tests live, naming,
  formatting/commit conventions)*
