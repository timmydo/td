## Seed conformance corpus for td-sh — word expansion and substitution.
## Oils spec-test format (oils-for-unix/oils); consumed by td_sh::run_dir.
## Builtins only; goldens are correct POSIX-sh (dash/ash) behavior.
## compare_shells: dash ash bash mksh

#### arithmetic expansion honors precedence
echo $((2 + 3 * 4))
## STDOUT:
14
## END

#### arithmetic expansion reads variables
a=6
b=7
echo $((a * b))
## STDOUT:
42
## END

#### parameter default when unset
echo ${name:-anon}
## STDOUT:
anon
## END

#### parameter default is skipped when set
name=alice
echo ${name:-anon}
## STDOUT:
alice
## END

#### string length
s=hello
echo ${#s}
## STDOUT:
5
## END

#### remove shortest matching suffix
f=archive.tar.gz
echo ${f%.gz}
## STDOUT:
archive.tar
## END

#### remove longest matching suffix
f=archive.tar.gz
echo ${f%%.*}
## STDOUT:
archive
## END

#### remove shortest matching prefix
p=/usr/local/bin
echo ${p#/usr/}
## STDOUT:
local/bin
## END

#### command substitution
echo "today is $(echo Friday)"
## STDOUT:
today is Friday
## END

#### double quotes expand, single quotes are literal
x=world
echo "$x" '$x'
## STDOUT:
world $x
## END

#### unquoted expansion splits on IFS
list="a b c"
set -- $list
echo $#
## STDOUT:
3
## END

#### quoted expansion is one field
list="a b c"
set -- "$list"
echo $#
## STDOUT:
1
## END

#### positional parameters via set
set -- one two three
echo $# $1 $3
## STDOUT:
3 one three
## END

#### shift drops leading positional parameters
set -- a b c d
shift 2
echo $# $1
## STDOUT:
2 c
## END

#### a here-document feeds a reader loop
while read a; do echo "got $a"; done <<EOF
one
two
EOF
## STDOUT:
got one
got two
## END
