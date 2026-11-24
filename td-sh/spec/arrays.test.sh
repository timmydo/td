## Seed conformance corpus for td-sh -- bash's indexed arrays.
## Oils spec-test format (oils-for-unix/oils); consumed by td_sh::run_dir.
## ash and dash have no arrays, so every golden here is bash 5.2's answer,
## measured, except where an `OK bash` block records how td-sh differs from it.
## compare_shells: bash

#### compound assignment keeps each element whole
a=(x "y z" '' *.no-such-file)
printf '<%s>' "${a[@]}"; echo
echo "${#a[@]} ${#a[1]} [$a]"
## STDOUT:
<x><y z><><*.no-such-file>
4 3 [x]
## END

#### a literal may span lines and carry comments
a=(one   # first
   two
   three)
echo "${a[@]}"
## STDOUT:
one two three
## END

#### [@] and [*] split and join as $@ and $* do
a=("p q" r)
set -- ${a[@]}; echo $#
set -- "${a[@]}"; echo $#
IFS=:
echo "${a[*]}"
## STDOUT:
3
2
p q:r
## END

#### elements are read and written by arithmetic index
a=(zero one two)
i=1
a[i+1]=TWO
a[-1]=last
echo "${a[@]} ${a[i]} ${a[-3]}"
## STDOUT:
zero one last one zero
## END

#### sparse indices, and keyed elements in a literal
a=([3]=c [1]=a b)
echo "${!a[@]} | ${a[@]} | ${#a[@]}"
a[10]=j
echo "${!a[*]}"
## STDOUT:
1 2 3 | a b c | 3
1 2 3 10
## END

#### += appends elements to an array and text to an element
a=(x)
a+=(y "z w")
a+=s
a[2]+=!
printf '<%s>' "${a[@]}"; echo
## STDOUT:
<xs><y><z w!>
## END

#### a scalar is a one-element array, and a plain assignment sets element 0
s=only
echo "${s[@]} ${#s[@]} [${s[1]}]"
a=(1 2)
a=x
echo "${a[@]}"
## STDOUT:
only 1 []
x 2
## END

#### unset removes one element or the whole array
a=(x y z)
unset 'a[1]'
echo "${!a[@]} ${a[@]}"
i=2
unset 'a[$i]'
echo "${!a[@]}"
unset 'a[@]'
echo "[${a[@]}] ${#a[@]}"
## STDOUT:
0 2 x z
0
[] 0
## END

#### an empty array is unset to the operators, so the guarded expansion is safe
set -u
f() { echo "$#:$*"; }
e=()
f ${e[@]+"${e[@]}"}
a=("p q" r)
f ${a[@]+"${a[@]}"}
echo "${e[@]-dflt} ${a[@]:+alt}"
## STDOUT:
0:
2:p q r
dflt alt
## END

#### operators apply to each element
a=(x.c y.c z.h)
echo "${a[@]%.c}"
echo "${a[@]/./-}"
echo "${a[@]#?.}"
## STDOUT:
x y z.h
x-c y-c z-h
c c h
## END

#### a slice selects by index
a=(a b c d)
unset 'a[1]'
echo "${a[@]:1} | ${a[@]:0:2} | ${a[@]: -1}"
## STDOUT:
c d | a c | d
## END

#### local -a declares an array that the call's return takes away
a=outer
f() {
  local -a a
  a+=(1)
  a+=("2 3")
  printf '<%s>' "${a[@]}"; echo " ${#a[@]}"
}
f
echo "$a"
## STDOUT:
<1><2 3> 2
outer
## END

#### local, export and readonly take a compound value
f() { local l=(x y); echo "${l[1]}"; }
f
readonly r=(1 2)
export e=(3 4)
echo "${r[1]} ${e[1]}"
r[0]=9
echo unreachable
## status: 2
## STDOUT:
y
2 4
## END
## OK bash status: 0
## OK bash STDOUT:
y
2 4
unreachable
## END

#### an array never reaches the environment
a=(1 2)
export a
printenv.py a
## STDOUT:
None
## END

#### the export listing reads back as the same array
a=(x 'y z' "it's")
a[5]=far
export a
listing=$(export -p | grep ' a=')
echo "$listing"
unset a
eval "$listing"
printf '<%s>' "${a[@]}"; echo " ${!a[@]}"
## STDOUT:
export a=([0]='x' [1]='y z' [2]='it'"'"'s' [5]='far')
<x><y z><it's><far> 0 1 2 5
## END
## OK bash STDOUT:
declare -ax a=([0]="x" [1]="y z" [2]="it's" [5]="far")
<x><y z><it's><far> 0 1 2 5
## END

#### an array literal is refused outside an assignment
echo a=(1)
echo unreachable
## status: 2
## STDOUT:
## END

#### an array cannot prefix a command
a=(1 2) true
echo unreachable
## status: 2
## STDOUT:
## END
## OK bash status: 0
## OK bash STDOUT:
unreachable
## END

#### the shape of an agent harness's pkill guard
probe() {
  local _skip="" _a
  local -a _probe=()
  for _a in ${1+"$@"}; do
    if [ -n "$_skip" ]; then _skip=""; continue; fi
    case "$_a" in
      --signal) _skip=1 ;;
      -[0-9]*) ;;
      -[PUGOF]?*) _probe+=("$_a") ;;
      *) _probe+=("$_a") ;;
    esac
  done
  printf '<%s>' ${_probe[@]+"${_probe[@]}"}; echo " ${#_probe[@]}"
}
probe -9 --signal TERM -f 'my prog'
probe -9
## STDOUT:
<-f><my prog> 2
<> 0
## END

#### a string bound for one command or one call hides the array whole
a=(1 2 3)
f() { echo "${a[@]}|${#a[@]}"; }
a=5 f
g() { local a=6; echo "${a[@]}|${#a[@]}"; }
g
a=7 printenv.py a
echo "${a[@]}"
## STDOUT:
5|1
6|1
7
1 2 3
## END

#### a dynamic name's element 0 is its value
echo "${#LINENO[@]} ${!RANDOM[@]}"
[ -n "${RANDOM[0]}" ] && echo set
## STDOUT:
1 0
set
## END

#### a subscript may hold blanks and quotes
a=(1 2 3)
a[1 + 1]=x
i=0
a[ i ]=z
j=1
echo "${a[@]} ${a["$j"]} ${a[ j + 1 ]}"
unset 'a["$j"]'
echo "${!a[@]}"
## STDOUT:
z 2 x 2 x
0 2
## END

#### unset expands no subscript of a name that is not an array
unset "u[\$(echo RAN >&2)]"
echo "unset name: $?"
x=1
unset 'x[0]'
echo "x[0]: $? ${x-gone}"
x=1
unset 'x[@]'
echo "x[@]: $? ${x-gone}"
v='x[$(echo RAN >&2)]'
unset "$v"
echo "scalar: $? $x."
## STDOUT:
unset name: 0
x[0]: 0 gone
x[@]: 1 1
scalar: 1 1.
## END
## OK bash STDOUT:
unset name: 0
x[0]: 0 gone
x[@]: 1 1
scalar: 0 .
## END

#### "${a[@]}" in one word joins as "$@" does, on IFS's first character
IFS=:
a=(1 2)
x="${a[@]}"
echo "$x"
## STDOUT:
1:2
## END
## OK bash STDOUT:
1 2
## END

#### reading a subscript before element 0 is fatal
a=(1)
echo "x${a[-5]}y"
echo after
## status: 1
## STDOUT:
## END
## OK bash status: 0
## OK bash STDOUT:
xy
after
## END
