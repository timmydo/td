# td-pinentry

td-pinentry asks for a passphrase in a td-ui window on a foreign Wayland
desktop, for the two callers that ask for one on a host: gpg-agent, over
the pinentry protocol, and ssh and git, over the askpass convention. A
td program that runs git without a terminal (td-review's window) has no
other way to answer an SSH key's passphrase, an HTTPS password or the
gpg passphrase a credential helper needs; a terminal pinentry fails there
with "Inappropriate ioctl for device".

It is a host program. `./install-apps` builds it with the host's cargo
and installs it in `~/.local/bin`; no recipe builds it and the image does
not carry it.

## Trust position

td-ui's masked field is a rendering option, not a trusted-input path
(td-ui/DESIGN.md, "Invariants"), and Principle 7 puts a secret typed on
td behind the compositor's secure attention. td-pinentry therefore runs
on foreign desktops alone. It admits a system from os-release by
td-pass's rule: an `ID` or `ID_LIKE` naming td is refused with the
reason, and an unreadable or repeated identity admits nothing. On a
foreign desktop the trust boundary is the host's own: what its
compositor lets another client see of the window or the keyboard is what
any pinentry it runs would give up, and td-pinentry claims nothing more.

It keeps no secret beyond one prompt. It has no cache, refuses gpg-agent's
external password cache, writes no secret to a log, and offers nothing to
the clipboard; a paste into its field is allowed. A typed secret lives in
td-ui's `EntryModel`, reserved at its 1 KiB ceiling and zeroed by edits,
on clearing and on drop; the answer copies it into a buffer of its exact
size, and the line written to the caller is built in a buffer reserved
before it is filled; each is zeroed when done. Outside that reach are the
copies on the way in, which td-ui's `EntryModel` names: each key's chord
and text, and a paste's transfer buffer and string, freed unzeroed; and
the copies the compiler, the allocator or the kernel's pipe make.

Nothing ties the window to the request that opened it beyond its title
and text. td-ui offers no activation token, so a compositor that holds
back a new window's focus may leave the keyboard where it was; type only
once the window shows its field focused.

The window's frames are kept in `$XDG_RUNTIME_DIR`, else `/dev/shm`. A
masked field paints one mask glyph per character, so a frame holds a
secret's length and never a character of it. The window asks td-ui to
zero its frames when the prompt is answered. A field that shows its text
asks for no secret, known by the whole of its prompt: git's
`Username for '<url>': `, and ssh's question starting `The authenticity
of host ` and ending `(yes/no/[fingerprint])? `. Any other prompt is
masked, one that merely contains those words (a key file's name) and a
translated one included.

## Invocation

- `--help` prints the usage and `--version` the version.
- No argument, or a first argument starting with `-`, is pinentry:
  gpg-agent passes `--display`, `--ttyname` and the like, which its
  `OPTION` lines repeat and the window does not use.
- One other argument is an askpass prompt.
- Anything else, or an argument that is not UTF-8, is a usage error.

A connection handed down in `WAYLAND_SOCKET` belongs to the process it
was handed to, so only `WAYLAND_DISPLAY` (with `XDG_RUNTIME_DIR` for a
relative name) is read, and each prompt connects anew. GnuPG forwards
`WAYLAND_DISPLAY` from the gpg that started the agent's request to the
pinentry it starts.

## Pinentry protocol

One Assuan conversation over standard input and output. The greeting is
`OK Pleased to meet you`. Each command line is answered with `OK`, with
`D` data lines before it where it returns data, or with `ERR <code>
<text>`, the code a libgpg-error code with pinentry's source (5) in its
top byte. A line read is at most libassuan's 1002 bytes with its CR and
LF, since an agent may fill one; a longer one is read through and refused
as `Line too long`. A line written is at most 1000 bytes with its LF. An
empty line or one starting with `#` is no command. A command ends at a
space or tab and its parameters start after the blanks that follow; its
name is matched without regard to case. Parameters have their `%XX`
escapes decoded and are read as UTF-8, an invalid sequence replaced. A
read a signal interrupts is read again.

- `SETTITLE`, `SETDESC`, `SETPROMPT`, `SETOK`, `SETCANCEL`, `SETNOTOK`
  and `SETREPEATERROR` set the next prompt's texts; `SETERROR` sets an
  error shown on the next prompt alone; `SETREPEAT [label]` asks the
  next `GETPIN` to take its text twice, under `Repeat:` when no label is
  given; `SETTIMEOUT n` gives up after `n` seconds, `0` never. GTK
  mnemonics are dropped from labels (`_OK` is `OK`, `__` one `_`).
- `OPTION default-ok`, `default-cancel` and `default-prompt` name the
  labels used where no `SET*` command named one. The options describing a
  terminal, a toolkit or another label (`display`, `ttyname`, `ttytype`,
  `lc-ctype`, `lc-messages`, `owner`, `parent-wid`, `grab`, `no-grab`,
  `touch-file`, `invisible-char`, `debug-wait`, any other `default-*`)
  are accepted and change nothing. Any other option is refused as
  `Unknown option` (83886254), so the agent never counts on a feature
  this program lacks: an external password cache, enforced constraints, a
  formatted passphrase. `RESET` forgets the texts and keeps the options
  and the timeout, which gpg-agent sets once.
- `GETINFO` answers `flavor` (`td`), `version`, `pid` and `ttyinfo`
  (`- - -`); anything else is `Invalid parameter`.
- `NOP`, `HELP`, `SETREPEATOK`, `SETQUALITYBAR`, `SETQUALITYBAR_TT`,
  `SETGENPIN`, `SETGENPIN_TT`, `SETKEYINFO` and `CLEARPASSPHRASE` are
  accepted and change nothing. `BYE` answers `OK closing connection` and
  ends the conversation. Any other command is `Unknown IPC command`.
- `GETPIN` shows a masked field. Its text comes back in `D` lines of at
  most 1000 bytes, `%`, CR and LF escaped and no escape split between
  lines, then `OK`; an empty text is `OK` alone. A text typed twice is
  preceded by `S PIN_REPEATED`.
- `CONFIRM` asks a question with OK and Cancel, and `SETNOTOK`'s button
  between them when it was set; `CONFIRM` with a `--one-button` argument
  and `MESSAGE` show OK alone. OK is `OK`. The not-OK button shows on
  that two-button question alone, never on `GETPIN`.
- Cancel, Escape or closing the window is `Operation cancelled`
  (83886179); the not-OK button `Not confirmed` (83886194); the timeout
  `Timeout` (83886142); a window that cannot open `No pinentry`
  (83886165), its reason on standard error.

The reader runs on a thread of its own and holds at most sixteen lines
ahead. The window looks for the end of standard input every turn, at
least every 100 milliseconds: an agent that gives up or dies while a
prompt is up closes the window, and the conversation ends unanswered. An
agent sends nothing while it waits for an answer, so a caller that sends
sixteen lines or more while a prompt is up is taken as gone, its end
being unseeable behind them.

## Askpass

The prompt is the argument; the window shows it as the explanation over
an unlabelled field. The answer is written to standard output with one
newline and the program exits 0; Cancel, Escape, closing the window or
the timeout write nothing and exit 1, as does a window that cannot
open, which also says why on standard error. The window closes
unanswered when the process that started it is no longer its parent, so
an ssh or git killed meanwhile leaves no prompt behind; the pinentry
window watches the same. `SSH_ASKPASS_PROMPT` is
ssh's hint: `confirm` asks a question whose OK exits 0 and writes
nothing; `none` shows the prompt with a Dismiss button, and ssh ends the
program itself when it no longer applies.

## Window

The explanation wraps at spaces from the top, each of its lines starting
a row, a word wider than the window split, and an ellipsis ends it when
the window is too short for all of it. Under it are a row for the
window's own remarks, the error row, the field's label and the field, a
second label and field when the text is typed twice, and the buttons
across the foot: OK, the not-OK button when set, then Cancel. The field
and buttons keep their room before the explanation takes any, and a
window too short for both rows gives up the remark before the error.

On a text, Return answers what the field holds; in the first of two
fields it moves to the second. Tab and S-Tab move between two fields. C-v
and S-Insert paste; a paste or a typed character the field refuses (a
line break, past the ceiling) changes nothing and says why in a row of
its own over the error row, as a refused key does, so the agent's error
stays shown. The field's own keys are td-ui's `Action::from_chord`. On a
question, Left, Right, Tab and S-Tab select a button, starting on OK, and
Return presses the selected one. Escape cancels either. A button acts
when the press and the release are both on it. Two fields that differ
clear the second, show the agent's repeat error, and wait. F1 lists the
keys and F12 moves the theme, as in every widget window.

td-ui's keymap makes text of printable ASCII alone and refuses AltGr's
modifier states, so a character typed with AltGr or outside ASCII is not
typed; a layout whose keys reach a non-ASCII character without AltGr (a
German one's `ü`) is refused whole, and then no key works and the window
answers to the pointer alone. td-ui reports either refusal in a notice
naming the key or the layout, which can be a character of the secret, so
the window shows and writes one fixed sentence for both, naming neither,
rather than answer a passphrase short of the key. The window's other
notices go to standard error alone. A test holds td-ui to the notice
prefix this filter reads; telling a refused layout from a refused key
would need td-ui to report them apart. The request's timeout runs until
the person first edits a field, and stops then, as pinentry's toolkits
stop theirs.

## Using it

For gpg, once: `pinentry-program /home/USER/.local/bin/td-pinentry` in
`~/.gnupg/gpg-agent.conf`, then `gpgconf --kill gpg-agent`. gpg-agent
chooses its pinentry from that file alone; no variable a caller sets can
choose it.

For ssh and git, the caller's environment: `SSH_ASKPASS` naming
td-pinentry with `SSH_ASKPASS_REQUIRE=force`, so ssh asks the window
even when it has a terminal or no `DISPLAY` (OpenSSH 8.4 or later), and
`GIT_ASKPASS` naming it, which git prefers to `core.askPass` and
`SSH_ASKPASS`; git asks `SSH_ASKPASS` when neither of the others is set.

On a host, td-review sets `SSH_ASKPASS` and `SSH_ASKPASS_REQUIRE=force`
for the git commands it runs through its one command path when
td-pinentry is an executable file in its own executable's directory,
links resolved, as `./install-apps` places them. It sets nothing when its
own environment already holds a non-empty `SSH_ASKPASS`, `GIT_ASKPASS` or
`SSH_ASKPASS_REQUIRE`, a choice the person made (an editor terminal's
`GIT_ASKPASS` included), or no non-empty `WAYLAND_DISPLAY`, where ssh
keeps asking on its terminal; it leaves `GIT_ASKPASS` alone, so a
configured `core.askPass` still answers git's own prompts. A display
inherited but not in front of the person, as in tmux reattached over
ssh, still gets the window, and ssh waits on it until its timeout.

## Tests

The crate's tests drive the window state headless: an answer typed and
cleared, cancel by Escape and by closing, a masked field painting no
character of its secret, a visible field painting its text, two fields
that must match, Tab between them, the request's error shown, a paste
and a refused paste, a question's buttons by pointer with release off
the pressed button doing nothing and by keyboard, a message's one button,
the not-OK button kept off a message and a text, the timeout, typing
stopping it and a caret move or an empty paste not, the caret's blink, a
refused key reported without naming it in a row that leaves the agent's
error, td-ui's notice prefix pinned, a caller hanging
up and a late hang-up keeping the answer, and every part kept on small
and scaled surfaces. The protocol's tests run whole conversations
through the reader: the greeting, escapes both ways, each way a prompt
ends, `S PIN_REPEATED` with a label and without, an error shown once,
options taken and refused, `RESET` keeping the timeout, `GETINFO`,
comments, case, a tab after the command, unknown commands, a line too
long and one exactly at libassuan's ceiling with its CR, a long
passphrase split across `D` lines that decode back, and the inbox seeing
the caller's end behind a waiting prompt, behind lines sent meanwhile and
behind more than its queue, with queued lines kept in order and an
interrupted read read again. Askpass, invocation, mode admission and the
key list's spelling are tested beside them.

Native compositor cases run the binary under a real headless
td-compositor and type through its seat: askpass writes the typed
passphrase, its `%` included, and exits 0; Escape writes nothing and
exits 1; and a pinentry conversation returns the typed passphrase to its
caller as `D pas%25`. On td each instead observes the refusal.

Not yet: falling back to a terminal pinentry when no display is reachable
(a pinentry for gpg over SSH would need it), and a recipe that would put
the program in the image.
