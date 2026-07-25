## Seed conformance corpus for td-sh — control flow and command execution.
## Oils spec-test format (oils-for-unix/oils); consumed by td_sh::run_dir.
## Every case uses only shell builtins so the harness can run with a cleared
## environment (no PATH). Goldens are correct POSIX-sh (dash/ash) behavior.
## compare_shells: dash ash bash mksh

#### echo prints its arguments
echo hello world
## STDOUT:
hello world
## END

#### exit sets the status
exit 7
## status: 7

#### true and false set $?
true
echo $?
false
echo $?
## STDOUT:
0
1
## END

#### semicolon sequences commands
echo one; echo two
## STDOUT:
one
two
## END

#### and-or lists short-circuit
false && echo nope
true || echo nope
echo done
## STDOUT:
done
## END

#### if/then/else takes the true branch
if true; then echo yes; else echo no; fi
## STDOUT:
yes
## END

#### if/then/else takes the false branch
if false; then echo yes; else echo no; fi
## STDOUT:
no
## END

#### for loop iterates a word list
for x in a b c; do echo $x; done
## STDOUT:
a
b
c
## END

#### while loop with an arithmetic counter
i=0
while [ $i -lt 3 ]; do echo $i; i=$((i + 1)); done
## STDOUT:
0
1
2
## END

#### until loop runs while its condition is false
i=0
until [ $i -ge 2 ]; do echo $i; i=$((i + 1)); done
## STDOUT:
0
1
## END

#### case matches a pattern
x=banana
case $x in
  apple) echo fruit-a ;;
  b*)    echo starts-b ;;
  *)     echo other ;;
esac
## STDOUT:
starts-b
## END

#### functions run and see positional params
greet() { echo "hi $1"; }
greet world
## STDOUT:
hi world
## END

#### a pipeline feeds a builtin reader
echo hello | { read line; echo "$line"; }
## STDOUT:
hello
## END

#### subshell exit status propagates
( exit 4 )
echo $?
## STDOUT:
4
## END
