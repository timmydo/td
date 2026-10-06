# td-agent

td-agent is td's agent harness: one td-ui window whose left side lists
conversations and whose right side is the active conversation or a new
one, driving language models reached through a pay-per-token API key
(OpenRouter first). Every conversation is a peer: the human creates each
one with a **workspace** from a template, an empty scratch directory, a
directory of theirs or a set of sparse git worktrees, and conversations
message and read one another only as the human allows. A conversation's
tool calls execute in a jail whose policy belongs to its workspace. It
is a coding agent first and a general assistant second, on the same
loop. This document is the normative contract for the program and the
starting point for successive agents; the root `AGENTS.md` and
`DEVELOPMENT.md` still govern changes and submission.

## Status

Increments 3 to 8 of §18 are built: td-ui's message list; the
crate with its gate, the window and conversation processes, the store
and the host launch; the model client over `td-fetch 1`, with the key file,
the models list, cost limits, credit and titles; td-net's streamed
fetch; streamed replies over it, drawn as they arrive and interrupted
by `Escape`; and the conversation tools, the first a model is given:
the todo list, `history_search` and `history_read`, `conversations`
and `send_message`, with the wake budget and pausing. After them came
the window's File menu and the dialog that stores the OpenRouter key
from it (§4, §6), then the Conversation menu, which chooses each
conversation's model, from a picker over the models list, and its
reasoning effort (§4), the system context, shown folded at the head of
the transcript, and the diagnostics export (§4); then increments 9 and
10, the tool host and the `workspace` jail, with directory and scratch
workspaces whose file and shell tools run in `ask` mode, and then the
peers step of §18, which made every conversation a peer (§3). Where
building them settled a point the design left open, the section says so
under "As built". No recipe names td-agent yet. The decisions below that
were the user's to make were made on 2026-10-01, 2026-10-02 and
2026-10-04:

- **Use:** both coding and general assistance, coding first.
- **Run target:** an unjailed installed launch on a development host first
  (`./install-apps`, as td-news and td-mail, APPLICATIONS.md §X.7), so
  the harness can be exercised against OpenRouter at once; packaging as a
  jailed td application follows as its own increment.
- **Coordination:** no orchestrator. Every conversation is a peer with
  the same tools and prompt; the human creates each and can archive or
  delete any; and reading or messaging another conversation is a
  crossing the human decides on a card (§3).
- **Workspaces:** every new conversation chooses a workspace template,
  Empty, Directory… or one configured with several sparse-checkout git
  worktrees and asynchronous fetch (§7, §15). Creating a workspace is
  the human's act, never a model's. Agents can commit and push (§9).
- **Processes:** one subprocess per conversation, under which, on a
  host, every process that conversation causes runs (§2). The first
  increments set no resource limits, and nothing in them may prevent
  per-conversation memory and CPU limits from being added later and
  inherited (§8).
- **Tool execution:** a jail per workspace (§8), a network policy wider
  than none (§10), and the host directories listed as `shared` in
  configuration, `~/Downloads` read-only by default, bound in (§8, §15).
- **Conversation features:** background processes, a todo list, search
  and reads over a conversation's full log, compaction and
  auto-compaction, and messages between conversations (§3, §12, §14);
  scheduled messages are designed now and built later (§3).
- **Development cost:** an edit confined to `td-agent/` selects td-agent's
  own tests and lints and nothing else in the gate until packaging (§17).
- **Auto mode:** the jail bounds what runs unreviewed; actions that cross a
  boundary go to a classifier pairing TypeSafe's Jev decision model on
  OpenRouter with a reasoning model; an action runs only when both allow,
  and anything else goes to the human (§11).
- **Chat components:** the transcript's widgets join td-ui, and their text
  is selectable and a whole message copyable (§4).

The design draws on Claude Code, Codex CLI, opencode, Aider, SWE-agent and
mini-swe-agent as surveyed on those dates. Vendor behaviour moves quickly,
and the design depends on none of it beyond the wire formats of §5.

## 1. Boundary and philosophy

Product references: Claude Code and Codex CLI for the loop, tools and
approval model; opencode for workspaces, child sessions and per-step
snapshots (§16 records what td-agent takes from it); mini-swe-agent for
how little scaffolding a strong model needs; Aider and SWE-agent for the
measured effect of edit formats and tool ergonomics. td-agent
reimplements none of them and inherits none of their compatibility claims.

td-agent is its own crate, `td-agent/`, and its own static binary, built by
td's source-built stage2 toolchain. Its manifest declares six
dependencies, all td crates by path: `td-civil`, `td-fetch-client`,
`td-fs`, `td-json`, `td-toml` and `td-ui`, and its lock lists exactly
those and td-agent. It
carries no TLS, resolves no names and opens no network
connection itself: every request to a model provider goes through the td
fetch service (APPLICATIONS.md §W.8) by td-fetch-client, as td-news and
td-mail do, so it needs no dependency sign-off and no td-crypto
admission. JSON and TOML are the td-json and td-toml crates td-news and
td-mail also depend on. Its `grep`
and `sed` tools are td-txt, the same multicall the image
ships as `/bin/grep` and `/bin/sed`, run as a program rather than
reimplemented (§12). It is zone-one source: not a foreign payload, not a
plugin host, and not a client of any td-run server (principle 5).

Out of scope for the first increments, each a later reviewed addition: an
MCP client, web search, a provider dialect other than OpenAI Chat
Completions, images, plugins, and any socket listening outside a jail's
own loopback.

## 2. Processes and trust

```text
              human
                |  window, keyboard, clipboard (td-ui)
                v
  +----------------------------------+
  | td-agent: window process         |--> git worker -> git remotes
  |  store index, cards, schedules,  |
  |  routing between conversations   |
  +----------------------------------+
                | one framed socketpair per conversation
                v
  +----------------------------------+
  | td-agent conversation <id>       |--> fetchd -----> model provider
  |  loop, model client, log writer, |--> egress -----> permitted hosts
  |  approval engine                 |
  +----------------------------------+
                | one framed pipe per jail instance
                v
  +----------------------------------+
  | td-jail instance, workspace      |
  |  tool host -> sh, td-txt, git    |
  |  worktrees rw, git pointers ro,  |
  |  system ro, loopback proxy only  |
  +----------------------------------+
```

There are four parties, and the design is the separation between them:

1. **The agent processes** are the window process and one conversation
   process per running conversation, the same binary under two
   personalities. Together they own the window, the conversation store,
   the model client, the API key, the fetch grant, the repository store
   and every decision of §11. They never execute anything a model wrote
   and never read or write workspace file content. They run git outside a
   jail only on repositories no jail can write, with the fixed invocation
   of §9, and everything else of git in maintenance instances. How the
   work divides between them is below.
2. **The tool host** is the same binary under the `tool-host` personality,
   started inside a jail instance. It performs every tool effect, file
   reads and edits as well as shell commands, local commits and the
   instruction reads of §13, and serves the in-jail end of the network
   proxy (§10). Path confinement is therefore the jail's mount namespace,
   not path-string checks in the harness, and td-agent needs no `unsafe`
   and no new UNSAFE.md surface. It speaks a bounded, length-framed,
   multiplexed protocol over the pipe the agent holds. **Every reply the
   tool host sends is jail-controlled data**: a process in the jail running
   as the same user can replace or impersonate it, so file contents,
   snapshots, digests and instruction text that come back are untrusted
   input, never authority.
3. **The fetch service** carries model traffic only. No jail instance is
   ever given its socket.
4. **The egress relay**, a td-net applet beside fetchd, carries a jail's
   permitted network traffic (§10). td-agent decides each destination; the
   relay refuses loopback, link-local, private and the machine's own
   addresses whatever it is told, a stricter predicate than fetchd's.

The combination to avoid is the "lethal trifecta": private data, untrusted
content, and a channel to an attacker-chosen destination. The provider
receives workspace content by design; that is the product. What must not
exist is a channel whose destination the model or the content picks
without a decision of §11. The key never enters a prompt, a transcript, the
classifier's input or a jail. A jail holds the workspace and untrusted
content, and its only way out is the proxy, whose every destination is a
standing decision of the human's or decided under §11 when asked for.
Publication with the human's credentials happens only through the git
worker's push, bound to a commit the human or classifier approved.

**The review command** stands apart from those parties: `td-agent review
[--model MODEL] [--effort LEVEL] [--max-tokens N] [--] [FILE]` is the
same binary run by a person or an agent from a shell, with no window,
conversation or jail, for one model's review of one git commit in td's
review workflow (DEVELOPMENT.md, Code review), where it is meant to take
an external reviewer CLI's place. It reads the commit's text as `git
show` prints it from FILE or standard input (an interactive one is
refused), at most 4 MiB (a larger one is refused, not cut, since a
review of part of a commit says nothing of the rest), and sends it in
one streamed request through the fetch service with the configured API,
model and key, as a turn is sent, but asking no Anthropic model to cache
it, since no later request shares its prompt. `--effort` is sent only
when given and refused for a model the list says takes no reasoning;
`--max-tokens` defaults to 32768, room for reasoning at `high`, cut to
the model's own limit. A fixed system instruction asks for a reply that
begins `REVIEWING: <subject> (<commit id>)` and then gives prioritized
findings, and quotes the commit in the user message between `<commit
NONCE>` and `</commit NONCE>`, a random 128-bit nonce per request, so the
commit's text cannot close its quotation; a commit holding them is
refused. The review is written to standard output as it streams, and the
model and provider that served it, its tokens, its cost and its
reservation to standard error. It exits non-zero unless the reply
stopped with something said: an error, a stream that ends without its
finish, a reply cut at its token limit, filtered or empty is no review.
A rate-limited request is asked again as a turn's is. Its cost is
bounded as §5 says. It keeps nothing, executes nothing the model says,
and has no tools: a reviewer that reads the rest of the tree is a later
step. The commit goes to the provider because the person ran the
command on it.

**One process per conversation.** The window process, `td-agent`, is the
one the human starts. It owns the window, the configuration, a lock on the
state directory that refuses a second window process, the index of
conversations, every card, the git worker's work on shared repositories
(the store's clones and fetches, and the publish repositories' imports
and pushes, §9), the day's cost total, the schedule timer and the routing
of messages between conversations (§3). For each conversation that is
running, has a background process (§12) or is open in the window, it
starts `td-agent conversation <id>` as its child over a framed socketpair
and passes it the API key over that socketpair, never through argv, the
environment or a file. The conversation process runs that conversation's
loop. It builds and sends the model requests, reserving against the day's
limit through the window process (§5); it is the only writer of its
conversation's directory (§6), which it holds an exclusive lock on, so a
restarted window process cannot start a second writer while an old
conversation process is still exiting; it runs the rules and the
classifier of §11; and it starts, or inside td asks the listener of §8
to start, every jail instance the conversation uses: its own file-tool
instance, `shell`, `grep`, `sed`, snapshot, background and maintenance
instances, forks included. Workspace maintenance that no turn asks for
(the periodic remote-tracking update, the counts `conversations` shows,
the check before archiving or deleting, §7) runs in the workspace's own
conversation process; when that conversation has no process, the window
process starts one for the purpose, without the key, and it exits when
the work is done; if the human opens the conversation meanwhile, the
window process hands that process the key and it goes on as the
conversation's process. A card goes up the socketpair as a request and
its answer comes back down. A conversation with no workspace has no
jail instances.

**State shared across conversations** is the window process's: the
human's rules, `deny everywhere` included, and each repository's
`.td-agent/rules` as the git worker read them (§11); each workspace's
mode, circuit-breaker counts and whether it has dropped to `ask`, which
span a workspace's conversation and its forks; admitted remotes and
allowlists; and every protected entry of the git chain, which it creates
itself (§8). Conversation processes report verdicts to it and it pushes
every change down the socketpairs as a numbered policy version; a
conversation process applies a change before its next decision, decides
again any approval still pending under the version it arrived in, and
checks the version once more just before an approved action runs, so a
deny answered in one conversation binds every other from its next action
on.

The split buys two things:

- **Faults stay in one conversation.** A crash, a stuck parse or an
  exhausted allocation ends one conversation process. The window process
  marks the conversation failed and can restart it from its log, which
  replays exactly (§6). A conversation process whose socketpair closes
  exits, and every instance it started dies with it (§8).
- **Resources have one owner.** On a host every process a conversation
  causes descends from its conversation process, so a limit placed on
  that process's cgroup is inherited by everything the conversation runs,
  which is what lets §8's later limits be added without redesign.

It is not a security boundary. Both personalities run unconfined as the
human and both hold the key; the jail is the boundary, as before. A
conversation with nothing running that is not open has no process, and
opening it starts one from its log.

**As built (increment 4).** Nothing runs in the background yet, so only
the open conversation has a process; switching shuts the old socketpair
and that process exits, or is killed if it has not within 2 s, since it
would hold its conversation's lock against the next.

- **Locks** are std's `File::try_lock`, `flock(LOCK_EX | LOCK_NB)`, on
  `window.lock` at the top of the state directory and `lock` in each
  conversation's directory. The kernel drops one when its holder exits,
  however it exits, so there is no stale lock and no pid to judge, and
  std opens every file close-on-exec, so no program a child execs keeps
  one (a child forked while one is held holds it too until then). It needs
  no `unsafe`. A conversation process waits up to 3 s for its directory's
  lock, for an earlier process of the same conversation still exiting,
  and is then refused; a second window process is refused at once,
  before it connects to the display.
- **Frames** are a four-byte big-endian length and that many bytes of one
  JSON object, at most 1 MiB, refused from the header before anything is
  allocated for it. A stream may end only between frames. A human's
  message is at most 128 KiB of text, which JSON escaping keeps within
  the frame and log-line bounds; the window and the conversation process
  each refuse a longer one by name.
- **The child** is `td-agent conversation <id> --state-dir <dir>
  [--create …]`, `--create` making the conversation in the workspace it
  names (§7), with its end of the socketpair as standard input and
  output. It replays its log up the socketpair (a `hello`, then every
  event), and exits when the socketpair closes. A turn is whole in the
  log before the window hears of it, so a window that closes the
  socketpair mid-turn interrupts nothing, and the child exits as it
  would between turns.
- **Restarts.** A child that exits, closes its socketpair or sends a
  malformed frame is killed and started again from its log, up to three
  times in a row without an acknowledgement; then the conversation is
  marked failed until it is opened again, which `Return` or a click on
  its row does. The window keeps each human message until the child
  acknowledges its delivery id, and resends the unacknowledged ones to
  the next child of that conversation, after a restart or when a
  conversation switched away from is opened again; the child logs each
  id once. A resend that fails shuts the socketpair, so the restart runs
  again rather than a child waiting on half a frame. A message the
  window cannot hand on at all, to a failed conversation or none, stays
  in the composer, and one the child refuses comes back to it with why,
  after any newer draft and never over its selection; one the composer
  cannot take is written whole to standard error.

**As built (increment 5).** A turn now takes as long as its model, so a
conversation switched away from mid-turn keeps its process: the window
keeps every child it has started, tracking each one's turn from its
`started` and `finished` records, and retires a background child once
its turn ends. A child that holds a message it has not acknowledged, or
a retry just asked for, counts as mid-turn, so a message sent just
before a switch is answered rather than logged and abandoned. A turn
that the closing window cuts short before its request is sent ends
with `C-r` offered when the conversation is next opened. Opening a
conversation whose child is still running adopts that child: the
window reads the conversation's log from the store, dropping a final
line still being written, and shows the child's later events, skipping
any whose sequence number it has shown. A background child that fails
is not restarted; its row says `failed` until it is opened. A
conversation left mid-turn shows `running` in the list until its turn
ends, and then closes. A message a background child refuses is noted
in the window and written whole to standard error, since there is no
composer of its own to return it to. The window sends every child its
settings first, before any message: the API key, or why there is none,
and the model client's configuration (§5, §15). The key crosses the
socketpair in that one frame and in no argument, environment variable
or file.

**As built (increment 7).** Two frames join the socketpair's. Down,
`interrupt` asks the open conversation's process to interrupt its turn;
between turns it means nothing. Up, `delta` carries what a streamed
reply brought since the last, its reasoning and its text, with its
request's sequence number, for the window to draw (§4). A delta is not
an event and is not logged: the reply is logged whole when its stream
ends (§5), and a window that adopts a conversation mid-stream draws
only the deltas after it opened, until the logged reply replaces them.
A background conversation's deltas are dropped.

**As built (increment 8).** The window routes messages between
conversations. Up, `send` carries a `send_message` the human allowed
(an id of the sender's, the receiver's id and the text), and `query`
asks for the states the window knows; down, `sent` answers a send,
queued or refused with why, and `states` answers a query. `message`
hands a receiver a message from another conversation (its delivery id,
sender and text, and the sender's role and a report's status where an
older outbox file holds them, §3), which it acknowledges with
`delivered` once logged, or refuses; `pause` pauses or resumes the open
conversation, and `clear_todo` clears its todo list. `hello` says
whether the conversation is paused, and carries its prefix for the
transcript (§4, the system context). The window checks every send
again, whatever the sender checked (the receiver exists and is not the
sender, the bounds, at most 16 undelivered), and writes it whole to the
state directory's `outbox`, one file per message under its receiver's
id, named by a rising order and its delivery id, before the sender hears
it was queued. Each poll hands what is queued to its receiver's process,
starting one in the background for a conversation that has none and has
not failed, and a message's file is removed only when its receiver
acknowledges or refuses it, or is deleted (§4); a load drops what is
queued for a receiver the store no longer holds. So once queued, a
restart of the receiver or the window neither loses nor repeats a
message: a receiver restarted
from its log is handed its unacknowledged messages again, and logs each
delivery id once. A sender that hears no answer, the window closing or
not answering in 30 s, tells its model that whether the message was
queued is unknown, and not to send it again unless an answer that needs
it does not come; a sender restarted mid-send finds the call
interrupted with its effect unknown (§6). A message the receiver
refuses (its log full, say) is noted in the window and not offered
again. A receiver the store no longer lists, or whose process cannot
be started, is said once and not tried again until the human opens it;
one whose process failed waits the same way. The outbox directory is
synced before each message is written, so a receiver's directory made
for it is durable first. A process woken for a message is let go, as
any background one is, when its turn ends, or at once when the message
started none; one told to resume is kept until it logs the resumption,
which comes after the start of any turn the resumption begins.

**As built (the File menu).** A `setup` may come again, and replaces the
first: a conversation process takes it between turns, as it takes any
frame that comes while a turn runs, so the turn under way finishes with
the settings it began with. Of the frames queued meanwhile it is taken
first, with any `choose` (the Conversation menu, below) in the order
they came, so the next turn has it whatever came before it, and a pause
behind it still goes ahead of the messages before it (§3). The window
sends one carrying the key the
human stored from its key dialog (§6) to every conversation process
that has not failed, and keeps it as the `setup` every process started
afterwards, restarts included, is sent first. The key still crosses
nothing but the socketpairs.

## 3. Conversations

Every conversation is a peer. Each has the same log and model client,
the same system prompt and the same conversation tools; what tells one
from another is its workspace, chosen from a template when the human
creates it (§7), and the human's choices for it: its model and effort
(§4), whether it is paused and whether it is archived. td-agent creates
no conversation by itself, and the human can archive or delete any of
them (§4). A workspace adds its own tools, the file, shell and git
tools of §12, and its own paragraph and environment block (§13); the
tools below are every conversation's. Creating a workspace is the
human's act, through a template; no model creates, archives or deletes
a workspace or a conversation.

**Coordination.** Every conversation has `conversations`,
`send_message`, `history_search`, `history_read` and `todo_write`
(§12):

- `conversations`: every conversation's id, workspace, state (idle,
  running, waiting for approval, paused, failed, archived), background
  process count, cost and last activity, which td-agent itself writes,
  most recently active first; with repository workspaces (increment 11),
  each worktree's state as well. Model-written fields, the title, the
  todo item in progress (§12) and background command lines, and what
  comes from refs a jail wrote, a worktree's branch and its commits
  ahead of and behind its base, are shown only for the caller itself, so
  the listing is not a channel between conversations.
- `send_message {to, text}`: queues a message for another conversation,
  which the window process routes. A message is delivered between turns:
  when the receiver's running turn ends, or at once if it is idle, it
  starts a turn as a user-role message labelled with its sender. Messages
  are asynchronous; a reply is a `send_message` back, which reaches the
  sender the same way. A message is at most 32 KiB and a receiver holds
  at most 16 undelivered; a send beyond either, to the sender itself, or
  to a conversation that is archived or that the store does not hold,
  fails with a result that says which.
- `history_search` and `history_read` (§12) read the caller's own log,
  or, with `conversation` naming another, that one's.
- `todo_write`: the conversation's own plan (§12).

**Crossings.** A message from another conversation is untrusted content,
whoever sent it, and reaches the receiver's classifier only in its
untrusted field (§11); it is never the human's authority. Reading
another conversation's log brings that conversation's content, its tool
output included, into the reader's context and so to the reader's
provider and anything the reader may later publish. For both, every
conversation counts as a workspace of its own, a fork included. So:

- a conversation reads and searches its own log without a crossing;
- reading or searching another conversation's log, and messaging
  another conversation, is a crossing, which the human decides on a
  card (§11), or by a standing answer they gave on one, before it
  happens, in `ask` and `auto` mode alike. The
  card names the other conversation by its id and title and shows, for
  a message, its text, as long as §11 lets a card part run and saying
  what it leaves out; for a read or a search, the page or the query
  asked for, and that the other log, tool output included, comes into
  this conversation's context and so to its model's provider. A refusal
  is the call's answer, telling the model not to reach the same result
  another way;
- the conversation process enforces the decision, as it does for
  `write_file`, `edit_file`, `sed` and `shell`. The window's post
  office checks every message again, whatever the sender checked: the
  receiver exists, it is not the sender, the size bounds, and at most
  16 undelivered;
- increment 13 brings the rest of §11's design. An "always" answer
  admits one operation in one direction for one pair: allowing A to
  read B lets neither B read A nor A message B (§11, As built
  (increment 13, crossings answered for good)). In `auto` mode the
  classifier decides a crossing, its `discloses` question covering
  content carried to a conversation that can publish where the source
  cannot. Until then every crossing is the human's, in both modes.

**Trust.** No conversation holds authority another lacks. None can
answer a card, create, archive or delete a conversation or workspace,
change a workspace's rules, network policy or limits, admit a remote, or
push except as §11 decides for its own workspace. A model can carry an
injection it read into another conversation, by a message or by being
read, and the design does not pretend otherwise; what bounds it is that
the carrying is itself a crossing, that the injection gains no authority
by it, and that what it can do there is what that conversation's
workspace allows, decided there by §11. The human's "deny everywhere"
rules bind every workspace, present and future (§11). The human can open
any conversation and talk to it directly.

**Notifications.** td-agent's own news of a workspace goes to that
workspace's conversation and to the window, which shows it, and to no
other conversation. A worktree that became ready or failed and a base
branch that advanced after a store fetch (§7) are notifications logged
in the workspace's conversation between turns, which its model reads
(§7, As built (increment 11, notifications)); a turn that finished, failed or
is waiting for approval is the window's to show, on the conversation's
row and as a notice when the conversation is not the one open. A
worktree that became ready or failed wakes its idle conversation; a base
branch advancing waits for its next turn. Nothing wakes a model on
another conversation's behalf.

**Wakes.** Two models can wake each other indefinitely, and so can a
model and its own background processes. Every turn started by a message
from another conversation or by a background exit notice counts against
the receiver's wake budget, derived from the log: after twenty since the
human last wrote to that conversation, those deliveries queue without
starting turns and the human is notified. Two kinds of turn do not
count, because something else bounds them: a firing of a schedule the
human approved, bounded by its own times, and a worktree's notice,
caused by a checkout, which no model can repeat at will. The human can
pause any conversation; a paused conversation starts no turn until
resumed, messages and notices to it queue, and schedule firings to it
are skipped (below). Every turn is reserved against the cost limits as
any other.

Conversations in another td-agent state directory, and other agent
harnesses' sessions on the machine, are out of reach in this design.
Sending to them would need a socket listening outside a jail, which §1
leaves out; seeing them could be read-only file access to their logs,
which is not designed here (§19).

**Schedules (later).** A schedule delivers a message to a conversation at
set times, so recurring work (a nightly dependency check, a weekly triage)
starts while the human is away. It is designed here and built after the
increments of §18:

- a schedule is a five-field cron expression (minute, hour, day of
  month, month, day of week; numbers, `*`, lists, ranges and steps, no
  names or macros; day of week 0 to 7, both 0 and 7 Sunday; when both
  day fields are restricted, either matching suffices, as in cron) or a
  single local time written `YYYY-MM-DDTHH:MM`, in the zone `TZ` names or
  else `/etc/localtime`; a target conversation, which is required and
  has no default; the message text; and `catch_up`, default false;
- the human creates one through a card or the composer. A model asks for
  one with `schedule {cron | at, text, to, catch_up?}`, which is a
  human-only crossing in both modes, because a schedule spends money
  unattended; the card shows the target, the next three times it fires
  and the catch-up choice. The approval is stored with the schedule,
  binding its target, times and text, and each firing gives the
  classifier that record as the human's standing decision to run this
  task at this time; it does not make a model-written text the human's
  instruction. `schedules` lists, and `cancel_schedule` cancels, only
  the schedules targeting the caller's own conversation; cancelling any
  other is the human's;
- a schedule whose target is archived or deleted stops firing and is
  shown as such until the human removes it;
- schedules live in the state directory, written by the window process,
  and fire only while it runs; there is no system timer or service.
  Before delivering, the window process journals each occurrence by
  schedule and its UTC instant, so a restart or a clock set back never
  fires one twice. A firing missed while td-agent was not running is
  dropped unless `catch_up` asks for one at startup, and never more than
  one. A local time that does not exist is skipped and one that repeats
  fires at its first instance;
- a firing is delivered like a message, labelled with the schedule and
  its author. Text a model wrote stays untrusted when it fires even
  though the human approved the schedule: the human approved when it
  runs and what it says, not that it is their instruction. A firing whose
  previous turn is still running is skipped and logged rather than
  queued, and every firing's turns are reserved against the cost limits.
  A firing to a conversation the human has paused is skipped and logged
  the same way.

**As built (increment 8).** Every conversation has `todo_write`,
`history_search`, `history_read`, `conversations` and `send_message`, a
workspace's tools after them (§12). The crossing rules are those above:
a conversation reads its own log, and every read of another's and every
message to another is a crossing, decided as "As built (peers)" says.
No conversation messages itself. A send to an id the store does not
hold is refused by name, and one to an archived conversation is refused
saying so (§7, As built (archiving)). `conversations`
lists at most 200, the most recently active first, with how many more
there are.

- **A message** reaches its receiver's model as a user-role message whose
  first line is its label, `[a message from conversation ID, not from
  the person]`. The receiver logs it as a `message` event (§6) and
  decides there and then, between turns, whether it starts one. A
  message logged, or queued in the outbox, by an older td-agent keeps
  the label it was given, byte for byte, whenever a request carries it
  again: `[a message from the orchestrator, not from the person]` for
  one whose sender's role was `orchestrator`, and `[a report from
  conversation ID, status S, not from the person]` for a `report`, its
  status `in_progress`, `done` or `blocked`.
- **The wake budget** is counted from the receiver's log: the first turn
  of each message from another conversation, or of each background
  process's end (§12), after the human's last message in that log. A turn the human asks again (`C-r`) does not
  count, nor does a report an older log holds: it was the notification
  of its sender's own turn, which the sender's budget bounded. Past
  twenty, a message is logged `held` without starting a turn, and the
  first held since the budget was renewed adds a notice saying so, which
  the window also shows when the conversation is in the background. A
  held message is in the log, so the next turn's request carries it; the
  human writing to the conversation renews the budget, and nothing else
  does.
- **Pausing** is `C-S-p` on the open conversation (§4). It is logged as
  a `pause` event and kept in `meta`, which an open puts right from the
  log's last `pause` should a process have died between the two, so it
  survives restarts. A paused conversation logs each message `held`
  and starts no turn for it; resumed, it starts one turn for the
  messages held since its last turn began, unless one of them would be
  a wake past the budget, and that turn's request carries them all. A
  pause sent while a turn runs takes effect when the turn ends, before
  any message that came meanwhile, though not before the human's own
  message or retry sent ahead of it. The human's own message or retry
  resumes a paused conversation, logging the resumption, and is
  answered as always.

**As built (peers).** No conversation is created at startup: the window
opens the most recently active conversation, or none when the store
holds none, and the human starts one with `C-n` (§4). Every conversation
has the same tools and the same system prompt (§13), a workspace's tools
and paragraph aside. Crossings are cards in both modes: the conversation
process asks the window, as it does for a change (§11, "As built
(increment 10)"), waits, and runs the read or sends the message only
when the human allowed it; a refusal, or a card withdrawn, is the call's
answer. `report` is gone, and a model that calls it is answered as for
any tool it was not given. An old orchestrator conversation, whose
`meta` says role `orchestrator`, is an ordinary conversation with no
workspace, kept, listed, opened and deleted like any other; its prefix
differs from the one written now, so it takes the new one as a `prefix`
event before its next request (§6). The `role` stays in `meta` and in
logged `message` events only as data, so old logs and outbox files keep
reading and old messages keep their labels; nothing else decides by it
but that one made as the orchestrator keeps its title, the title model
never asked.
`conversations` lists by last activity alone, to the millisecond and
then by id as the window's list does, at most 200.
`orchestrator_model` is retired: a configuration that sets it loads,
with a note that the key is no longer read (§15). The wake budget is
renewed only by the human's message to that conversation.

## 4. Window and layout

td-agent is a td-ui widget window (td-ui/DESIGN.md, "Widget window"). A
horizontal `split::Controller` divides it. The preferred share persists in
the state directory, which the toolkit's widget leaves to the consumer.

**Left: conversations.** Every conversation, most recently active
first and none pinned, as a tree table (td-ui/DESIGN.md, "Shared tree
table"): each row shows its title, its workspace (the template's name or
the directory), conversation state (idle, running, waiting for approval,
failed) and branch, and opens to its worktrees with their state
(fetching, checking out, ready, failed), any forked conversations, and
its background processes (§12). A row waiting for approval is marked
distinctly, so a run left in the background is visible from the list,
and the notifications of §3 are shown on their conversation's row. A
row's context menu (the context menus step of §18) archives the
conversation or deletes it (below); archiving hides it from the list,
which shows archived conversations when the human asks, each with a
context menu that unarchives it. A repository workspace's worktrees go
with either as §7 says.

**Right: the active conversation.** From top to bottom:

- the transcript, a message list (below). It holds user and assistant
  messages; reasoning, collapsed to one line until opened; tool
  calls as blocks with their arguments, status and a bounded excerpt of
  their result; verdicts; messages from other conversations and
  schedules, labelled with their source; and a divider where compaction
  ran (§14).
- approval and question cards (§11), composed from the toolkit's existing
  action buttons and wrapped text block, never transcript text.
- the todo list (§12), collapsed to its item in progress until opened.
- the composer, an editable pane. `Return` sends, as `C-Return` does from
  outside a dialog; `S-Return` is a newline.
- a status row: the model, the mode (`ask` or `auto`), the network policy,
  whether the workspace is unconfined, context used against the model's
  length, the resource limits (`no limits` until §8's land), the count
  of background processes, the conversation's cost, and the key's
  remaining credit.

**The message list joins td-ui.** The transcript's widgets are general
chat components, so they are built in td-ui, with its DESIGN.md amended in
the same landing, rather than privately here. Their text is selectable and
copyable:

- a press and drag selects text across message boundaries, Shift extends a
  selection, a double click selects a word, and `C-c` copies the selection
  through the window's `Clipboard`;
- every message has a copy action, a button on its header and `C-S-c` on
  the focused message, that copies the whole message as its source text,
  and a tool block's copy action copies its full result as retained in the
  log, not the drawn excerpt;
- copying never includes the chrome: headers, buttons and verdict marks are
  not part of any selection.

Everything else the list draws (wrapping, collapsing, scrolling and paging)
is the list's own, under the same raster and typeface contracts as the
toolkit's other widgets.

**Keys.** `C-n` new conversation, through the template chooser of §7,
`C-PageUp`/`C-PageDown` previous/next conversation, `Escape` interrupt
the running turn. A card never takes focus by itself, so a `y` typed
into the composer as a card appears answers nothing; the human focuses
the card (`C-Space` or the pointer) and then answers `y` or `n`.

The window is operable through td-ui's driven control socket (td-ui/DESIGN.md,
"The semantic seam"), which is how the native compositor tests and an
agent drive it. That socket can answer cards, so it is created under the
caller's runtime directory with the toolkit's ownership contract, and no
jail instance is ever given that directory.

The window is the window process's alone; conversation processes draw
nothing (§2).

**As built (increment 4).** Each conversation counts as a workspace of
its own: the list is every conversation, most recently active first,
with the state of the open one (`starting`, `idle`, `restarting` or
`failed`; a closed one shows none), and `C-n` started a conversation
with no workspace; it now opens the chooser of §7 (As built
(templates)). `F6` and `S-F6` move the focus between the list, the transcript
and the composer, and `Return` on a list row opens it. The transcript
holds what td-ui's message list bounds it to (16 MiB of text); past that
it drops its oldest messages an eighth at a time and says so, and the
log keeps every one. The status row is the state, a notice when there is
one, `no model`, the mode, `no limits` and `0 background`. The split's
share is the file `layout` in the state directory. The driven control
socket is opt-in, `--control-socket PATH`, and its actions are `new`,
`previous`, `next`, `send`, `focus-next` and `focus-previous`.

**As built (increment 5).** A row's state may also be `running`, while
a turn is under way. A reply shows as an `assistant` message: its
reasoning in a collapsed section, its text, and its usage as the
message's status (prompt tokens with cached and written, completion
tokens, and cost). A turn that ends other than `replied` adds a
`td-agent` message saying why. One that may pass (a 502 or 503, a
transport failure, an error inside a 200) adds that `C-r` asks again,
and the status row says so until it does; `retry` is the driven
action. The status row is the state, the retry hint and a notice, then
the open conversation's model, the context its last turn request used
against the model's length from the models list (`ctx -` before
either is known), the conversation's cost and the day's spending across
conversations, each against its limit where one is set, the key's
credit, the mode, `no limits` (the resource limits of §8) and `0
background` (the background processes of §12).

**As built (increment 7).** A streamed reply is drawn as it arrives:
its first delta adds an `assistant` message with the status
`streaming`, its reasoning a collapsed section before its text, and
each later delta is appended to its section through the list's
`append`, so the window does no more per delta than the bytes it
brought; reasoning that comes after text draws the message again with
its section first. When the reply is logged the message is settled: kept
as drawn, its reasoning left open or closed as the human left it, when
its sections are the logged reply's, and replaced by the logged reply
otherwise. A transcript at its limit mid-stream makes room as for a new
message, dropping its oldest; a reply it still cannot draw is marked
`cut short`, its later deltas dropped, and the logged reply replaces
it; one the transcript cannot hold even so stays as drawn with the
verdict `cut short`. A reply logged incomplete (§5) carries the
verdict `incomplete`; one whose process fails mid-stream keeps what was
drawn, marked `interrupted`, until a restart draws the transcript again
from the log. While the open conversation's turn runs, `Escape` asks
it to interrupt and the status row says so; otherwise `Escape` is the
focused widget's. `interrupt` is the driven action. The turn's
`td-agent` message then says that the stream was closed but that not
every provider stops generating, or billing, when a stream closes, and
that `C-r` asks again. A turn whose reply is logged and whose title is
being asked for (§13) does not hear an interrupt: the title request is
short and counted.

**As built (increment 8).** The open conversation's todo list is drawn
above the composer, over the chrome, collapsed to one line: how many
items are done of how many, then the item in progress, or the first
pending, with its mark (`[ ]` pending, `[>]` in progress, `[x]` done,
`[-]` cancelled). `C-t` shows the whole list, a line an item and at most
twelve lines, the last saying how many more there are, and collapses it
again; `C-S-t` clears it, which the conversation logs. `C-S-p` pauses or
resumes the open conversation (§3); the list and the status row say
`paused` while it is. `C-p` stays the composer's. The driven actions
are `pause`, `todo` and `clear-todo`. The transcript shows a message
from another conversation under its sender, a report an older log
holds with its status, and one held with the verdict `held: paused` or
`held: wake budget`; it gets the status of the turn it starts as a human
message does. A reply that calls tools carries a `tool calls` excerpt
naming each call with its arguments, and each result is a `tool NAME`
block, an excerpt of the result whose copy action copies the whole,
marked `error` when it is one. A pause, a resumption and a cleared list
are `td-agent` notices.

**As built (the File menu).** A menu bar, td-ui's `chrome::Bar`, takes
the window's top row, and the split lies under it. Its headers, `File`,
`Conversation` (below) and `Help`, in that order, open td-ui's shared
menu controller (td-ui/DESIGN.md, "Shared menu controller") in adaptive
fit, as td-mail's Folder menu does. `F10` opens File, as it opens
td-editor's menus, a press on a header opens its menu, and `Left` and
`Right` move between them. While one is open the
window routes its keys and the pointer to it, td-editor's set: `Up` and
`Down` move, `Return` or `Space` chooses, `Escape` or `F10` closes it,
every other chord is consumed, as is a hover, a press outside closes it
and goes no further, and a focus loss or a resize closes it. It is
painted after the window's frame. File's items are:

- `New conversation…`, shown with `C-n`, which does what `C-n` does:
  the template chooser of §7;
- `Set OpenRouter key…`, which opens the key dialog below; it has no
  chord;
- `Export diagnostics`, the diagnostics export (below, "As built (the
  diagnostics export)"); it has no chord;
- `Messages…`, shown with `C-S-m`, which does what `C-S-m` does: the
  Messages window (below, "As built (the Messages window)");
- `Quit`, which closes the window as the compositor's close does; it has
  no chord.

The header `Help` is td-ui's `keys::BUTTON`, and its one item, `Keys`,
is `keys::ITEM`: it shows the key list below, as `F1` does, and shows
`F1`. A shortcut File shows is a chord the window binds, which a test
holds; Help's `F1` is td-ui's window's own chord, which the program
never sees. Pausing and the todo list are the open conversation's, not a
file's, and stay chords only. The driven actions gain `menu` (`F10`)
and `set-key`, which has no chord and opens the dialog through the
item's own path; the driven state gains `menu` (`open` or `closed`),
`dialog` (`none`, or the part the dialog's keyboard is on: `entry`,
`cancel`, `save`, or `replace` while the confirmation below asks) and
`entry`, the masked entry's length, never its text.

The key dialog is modal over the window: while it is open it has every
key and the pointer, and no widget under it shows focus. It is composed
from td-ui's widgets as td-pass composes its PIN prompt: a bordered
chrome panel, centred and at most 72 cells wide, holding a title, what
it does and where the key is stored (§6), wrapped to at most six rows,
an `entry_model` in its masked mode painted by `chrome::TextEntry`,
three rows for a refusal or a warning, wrapped, and `chrome::Buttons`
Cancel and Save.
`Tab` and `S-Tab` move between the entry, Cancel and Save; `Return`
acts on what has the keyboard, the entry's being Save; `Space` presses
a focused button; `Escape` cancels. `C-v` and `S-Insert` ask the
clipboard for its text, which goes to the entry trimmed of the
whitespace around it, a trailing newline included. That paste is the
dialog's even when it comes after the dialog closed, and is then
dropped with a notice, never given to the composer, until the
window's clipboard says it gave it up (the control seam's inputs never
release it); any other paste
that comes while the dialog is open is dropped too. `C-c`,
`C-x`, `C-Insert` and `S-Delete` are refused by the masked entry, which
says so, and the clipboard is never asked to take anything. A press
and a release on one button choose it; a press on the entry puts the
caret there and a drag selects; a press elsewhere is consumed. The
row under the entry says why the text is not a key as it is typed or
pasted, and why Save refused it (§6). When a key is stored already,
td-ui's confirmation dialog asks `A key is already stored; replace
it?`, Cancel first; it is placed centred, or else at the window's top
or foot, wherever its Replace is not under the press that saved, as
td-ui's confirmation contract asks. Cancel keeps the stored key and the
dialog's text. The dialog closes when it is cancelled or the key is
stored, and its entry is cleared first, its bytes zeroed; a write the
window refuses keeps it open with why. A window too small for it
refuses to open it, and one resized too small closes it, cleared, each
with a notice.

The masking is td-ui's display option and no trust boundary
(td-ui/DESIGN.md, "Invariants"), and the dialog is not on the
compositor's secure-attention path. It collects no secret that
authenticates the human (principle 7's PINs and passphrases): the key
is a bearer credential the human copies from their provider, the bytes
the key file holds, which any process of theirs can read on the
host launch the dialog serves. Masking keeps it off the screen and
out of what the window shows the driven seam; it does not stop another
client drawing a look-alike, which is why the dialog is the host
launch's alone and the key on td is the portal credential (§6).

Without a key the status row says `no key: F10` after the state, until
one is stored, when a note says where it was stored and that every
conversation uses it from now on.

**As built (the Messages window).** The status row keeps to items of a
fixed width: the state, `C-r asks again` while a turn may be asked
again, `no key: F10`, a count of the notes not yet read (`2 new
messages: C-S-m`), `workspace: C-S-w` for a repository workspace (§7,
As built (increment 11, the workspace card)), the model and effort,
the context, the cost, today and the credit, the mode, `no limits` and `0 background`. A note, which
the row used to show cut to fit (a refusal said by name, a step done, a
background conversation's notice under its title), goes to a log of the
last 500, each with the time it came and kept whole up to 16 KiB, a
longer one cut and saying so. `C-S-m` or File → `Messages…` opens the
Messages window, modal over the body as the picker is: a title row and
td-ui's message list (td-ui/DESIGN.md, "Shared message list"), a note
to a message headed by its time in UTC, oldest first and following the
newest, its keys the transcript's (`C-c` copies the selection, `C-S-c`
the focused note); `Escape` or `C-S-m` closes it. Opening it reads
every note, and a note that comes while it is open joins it, read; a
paste while it is open is dropped and said. A held `Escape` or
`C-S-m` closes nothing; only a press does. It does not open over
another modal, and the key dialog replaces it. It always opens, so what
it holds is never out of reach: where its list has no room it says so
under its title and lays the list out once a resize gives it room, and
a window too small for the split still draws it. A note the list
refuses past its bounds takes the oldest shown with it, as the
transcript's do, and the list keeps no more notes than the log. The
workspace a conversation works in, which the row named, is the list's
third column, `Workspace`: `none`, `scratch`, `template NAME`, or a
directory by its folder's name (the deletion question names it whole,
so two folders of one name tell apart there).
The list's default share is a third of the body, so that its three
columns fit a window 1024 pixels wide. The driven action is
`messages`, and the state gains `notes` (`open` or `closed`), `unread`
and `note`, the newest note whole on one line.

**As built (row menus).** A right press on a conversation's row (td-ui's
`Context`, td-ui/DESIGN.md, "Widget window") opens its row menu at the
pointer: `Archive`, or `Unarchive` for an archived conversation, then
`Delete…`, which puts the deletion question of that row's conversation,
open or not. `S-F10` opens a row's menu under its row, at the list's
corner when the row is out of view: with the list focused, the selected
row's, so the keyboard reaches an archived conversation, and otherwise
the open conversation's; it closes an open row menu as `F10` closes the
bar's. The list keeps the row selected through updates, so the
keyboard's place holds until the human moves it or opens another
conversation. A right press elsewhere opens nothing, and opening a row's
menu does not open its conversation. The row menu is td-ui's shared menu
controller in its context kind, held in the bar menu's place while it is
open, so it takes the keys and the pointer as the bar's does and a right
press with either open closes it; the bar's comes back when the bar next
opens. An archived conversation (§7) leaves the list until Conversation
→ `Show archived`, checked while they show, shows archived ones too,
their state `archived`; opening one is refused with a note naming
Unarchive, `C-PageUp` and `C-PageDown` pass over it, and the window
starts with the most recently active conversation not archived. The
driven actions are `row-menu` and `show-archived`, and the state gains
`archived` (how many there are), `shown` (`all` or `unarchived`) and
`row-menu` (the conversation the open row menu is of, else `none`).

**As built (the key list).** `F1` shows td-ui's key list over the
window (td-ui/DESIGN.md, "Key list"). Its first section is every chord
of the driven action table, `control::BINDINGS`, with its help line, so
the list and the table an agent reads are one source; `set-key`,
`model` and `export-diagnostics`, which have no chord, are the File and
Conversation menus'. The section is titled `Global`. Then come the
focused widget's keys (the conversation list's, the transcript's or the
composer's) and the other two's, listed beside `ui::App::key`. Every
row is spelled as td-ui's keymap spells chords, which `keys::check`
holds under each focus, and Help → Keys adds no row: td-ui's own Window
section lists `F1`. The control socket delivers its keys to the
window's state, not through td-ui's window, so an `F1` sent there opens
nothing.

Help → Keys opens the list only when td-ui's window's own input chose
it, the live pointer or the physical keyboard (td-ui/DESIGN.md, "Key
list"). The choice cannot tell its origin where the menu carries it out:
the window and the control seam both deliver to `App::input`. So the
window delivers through `App::input_live`, which clears the choice
before the input and reports it after, and `Session` answers
`take_show_keys` from that alone; the seam's `F10`, `Right` and
`Return`, or its presses, choose the item and show nothing, and the
choice they leave is cleared before the window's next input.

**As built (the Conversation menu).** Each conversation has its own
model and reasoning effort. The configuration's `model` and
`reasoning_effort` (§15) are what every conversation starts with and
keeps until the human chooses otherwise, and a choice is the open
conversation's alone: it moves to no other, and the configuration file
is never written. The default model, which
a conversation with no model of its own uses, can be chosen from the
window too (below).

- **The menu.** The bar's second header, `Conversation`, lies between
  `File` and `Help`. Its items are `Model…`, which opens the picker below, and
  `Effort`, a submenu of every effort §15 admits (`none`, `minimal`,
  `low`, `medium`, `high`, `xhigh`), the open conversation's checked.
  Choosing one asks for it at once. Both items are off with no
  conversation open, and `Effort` is off for a model whose cached
  `supported_parameters` lacks `reasoning`, since it would not be sent
  (§5). The menu shows state, so the window builds it again, at a new
  revision, from the state of the moment each time it opens, by `F10`
  or a press on a header; while it is open nothing rebuilds it.
- **The picker** is td-ui's finder (td-ui/DESIGN.md) over the cached
  models list, modal over the window's body as td-mail's attach chooser
  is: a title row saying what it is for, a filter entry, the list and a
  status row. Its entries are every model of the list, sorted by id,
  each with its price in dollars per million prompt and completion
  tokens as its meta (`$3/$15`, `free`, or nothing where that would not
  fit a row's 16 bytes). A model that does not list `tools` or
  `max_tokens`, which every request sends (§5), is shown greyed and
  cannot be chosen, and the status row says why. The conversation's
  model is marked and selected when it opens. Typed characters filter by
  the words of an id, as the finder filters; `Up`, `Down`, `PageUp`,
  `PageDown`, `Home` and `End` move; `Return` or `C-Return` chooses the
  selected model, as does a second press on the same row within 400 ms,
  with no other input between; `Escape` closes it with nothing chosen,
  and every other chord is consumed. While it is open it has every key
  and the pointer, no widget under it shows focus, and a paste is
  dropped with a notice. It keeps the chosen effort. With no models list
  yet it does not open, and a notice says the list has not been fetched;
  a window too small for it closes it with a notice; opening another
  conversation, or the key dialog, closes it.
- **The choice** goes to the open conversation's process as a `choose`
  frame carrying the whole choice, a model and an effort each or null
  for the configuration's, both checked as §15 checks those keys. The
  window builds each choice on the last it asked for, so choosing a
  model and then an effort before the first is logged asks for both.
  The process takes it between turns as a setting, as it takes a key:
  before queued messages and pauses, in the order settings came. It
  logs it as a `choice` event, writes it to `meta` (§6) and syncs; its
  next request uses it. A choice made while a turn runs applies from the
  turn after.
  The log is the record: `meta` follows it, and a process that died
  between the two is put right on the next open, as pausing is. The
  window shows the choice when it hears the event, with a notice in the
  transcript naming the model and effort (or `no reasoning`) `from the
  next request`; the
  history tools show the event to a model as the person's choice.
- **The default model.** `Default model…`, an item of the same menu,
  on with or without a conversation open, opens the same picker titled
  for the default, the default marked and selected. A model chosen
  there is saved as `default-model` in the state directory (§6), with
  the configuration's `model` it was set over, and replaces that
  `model`: the window sends every conversation's process a fresh
  `Setup`, which it takes between turns as it takes a key, so new
  conversations and those with no model of their own use it from their
  next turn. What it is set over is the configuration's `model` key as
  the file says it at that moment, read again, or that the key is left
  out, so a change to td-agent's built-in default is no edit. At start
  the saved default holds while that key is unchanged; a `model` edited
  since is the newer and wins, and the window forgets the saved default
  and says so in a notice. The configuration file is not written.
- **Refusals** that name where a model was set (§5) say `the
  conversation's model (Conversation → Model…)` for a chosen one, and
  `` `model` or the default model (Conversation → Default model…) ``
  otherwise.
- **The status row** names the open conversation's model and then its
  effort (`anthropic/claude-sonnet-5.5 medium`), or `no reasoning` for
  a model that does not take one, and its context length is looked up
  for whichever model that is.
- **Driven.** The actions gain `model`, which has no chord and opens
  the picker through the item's own path; the state gains `picker` (the
  selected model, `nothing`, or `none` when closed), `query` (the
  filter), `model` and `effort`; and `default-model`, which opens the
  default's picker, `picking` (`default`, `conversation`, `template`
  for the template chooser of §7, or `none`) and `default`, the default
  model.

**As built (the system context).** The transcript shows what every
request begins with. Its first message, headed `system`, is the
conversation's prefix (§6, §13) read back: a section `system prompt`
holding the system message's text, and a section `tools (N)` listing
each tool's name and description, folded. The message itself is folded
to its header, so it costs one row until the human opens it, which a
press on its header or the transcript's fold key does, as any message's.
Its copy gives the prefix's exact bytes, the tools' parameter schemas
included. A `prefix` event (§6) adds another such message where it was
logged, marked `replaced from here`, since every request after it
begins with that one. A prefix of the older array form shows its
messages, and one the window cannot read shows a line saying so, its
copy still the bytes; an empty one, of a conversation that has not yet
made a request, shows nothing, and its first request's `prefix` event
shows it. The `hello` frame (§2) carries the prefix file's text, or
null when it is past the 128 KiB a human's message may be, which the
frame holds escaped; the window then says the system context is not
shown, as it does when the file cannot be read. A conversation the
window adopts mid-turn, whose log the window reads itself, has its
prefix file read by the window too, within the store's 1 MiB bound on a
prefix. The transcript drops its oldest messages first when it is
full (the log keeps them all), so in a long conversation the system
message goes before any other, and opening the conversation again
shows it again. A prefix of more messages than a message holds
sections shows the first fourteen and says how many more there are.

**As built (deleting a conversation).** Conversation → `Delete
conversation…` deletes the open conversation for good, whichever it is;
it is off only with none open. A row's menu asks it of that row's
conversation, open or not (As built (row menus), below).

- **The question** is td-ui's confirmation dialog (td-ui/DESIGN.md),
  modal and centred over the window's body: it names the conversation
  by title and id and says that its log, todo list, cost record and the
  messages waiting for it are removed from the machine, that what it
  sent to other conversations stays in theirs, and that this cannot be
  undone. `Cancel` is focused first, so `Return` alone deletes nothing;
  `Tab` reaches `Delete`. `Escape`, opening another conversation, the key
  dialog, or a window too small for it closes it with nothing deleted.
  While it is open it has every key and the pointer, and a paste is
  dropped with a notice.
- **The deletion** is the window's, and never a model's, in this order:
  the conversation's processes, open, in the background or retiring,
  are killed and waited for; its ledger reservations go; then the store
  deletes its directory (§6). The store takes the conversation's
  lock first, so it never deletes under a writer, holds it to the end,
  and renames the directory out of the list before removing it; one
  whose directory was never made is deleted already. Only then does
  what the outbox holds for it go. The window drops its row and, when it
  was the one open, opens the most recently active conversation left,
  or none. A deletion refused before
  the rename says why, and the conversation stays whole: the human's
  messages it had not acknowledged are parked for its next process and
  the outbox keeps the others'. Once renamed it is deleted; files a
  removal left are removed at the window's next start, and messages the
  outbox could not remove at its next load.
- **Driven.** The actions gain `delete-conversation`, which has no chord
  and asks through the item's own path; the state gains `confirm`
  (`none`, or where the question's keyboard is: `details`, `cancel` or
  `delete`).

**As built (the diagnostics export).** File → `Export diagnostics`
writes one archive a human can attach to a report: everything td-agent
keeps that bears on what it did, and never the key file.

- **What it holds.** Every regular file beneath the state directory
  (§6): each conversation's `meta`, `prefix` and `log`, the outbox, the
  models cache, `spend`, `layout` and the remotes admitted on cards, a
  crash's leftover temporaries,
  under `td-agent-diagnostics/state/`; the configuration file as
  `td-agent-diagnostics/config`, copied as written, so the notice and
  the manifest say to read it before sharing; and a generated
  `MANIFEST` naming the version, the time, the kernel, the two
  directories, how the key was kept out, every file taken with its
  size, and every file left out with why, in order of name. td-agent
  writes no log of its own beyond the conversations' (its standard
  error is the launcher's), so nothing else is collected.
- **The key.** Three things keep it out, each enough alone. The key
  file, its temporary (a crash during a replacement leaves the new key
  there) and the directory holding them are never taken: a file of
  either name is left out wherever it is, a file that is either under
  another name (a hard link, or the configuration linked to it) is
  known by its device and inode, and so is the directory, should the
  state directory be it or hold it. Every key the window has held, the
  one it started with and each the dialog stored, and the stored one
  read again through §6's checks, are looked for: a file holding one,
  as written or as a JSON string escapes it, is left out. A stored key
  that cannot be read again stops the export when the window holds no
  key either; otherwise the manifest says it was not looked for. A key
  replaced in an earlier run is not looked for.
- **What else is left out.** A link (the configuration is followed, as
  §15 reads it), a file that is not regular (each is opened without
  waiting and checked by its descriptor, so a FIFO swapped in never
  blocks the export), one past the log's 256 MiB bound, one deeper than
  four directories, and anything past 1 GiB in all; the walk considers
  at most 100,000 names, and the manifest counts the rest.
- **The archive** is ustar, written by td-agent itself, members mode
  0600 with uid and gid 0, a name past ustar's 100 bytes split into its
  prefix field. A file that cannot be written into it ends the export,
  since what follows would not be an archive. It is written as
  `td-agent-diagnostics-<UTC time>.tar.part`, made new with mode 0600,
  synced and then linked to its name without the `.part`, which
  replaces nothing (renamed, once the name is free, on a file system
  with no hard links), in `~/Downloads` when that is a directory, else in
  the home directory. A name already taken, finished or not, gets a
  `-1`, `-2` and so on, so a quit mid-export leaves only a `.part`.
- **Compression** is the host's, not td-agent's: `zstd -q -c`, else
  `gzip -q -c`, found on `PATH`, run directly with no shell, the archive
  its standard input and its standard output a second `.part` file
  td-agent makes new with mode 0600, synced and linked into place as the
  archive is, after which the archive is removed. A program the host
  lacks is passed over, and one that fails gives way to the next, its
  first line of standard error kept; with none that works the plain
  archive stays and the notice says why.
- **The window** runs the export on a thread of its own, says
  `exporting diagnostics…` and, when it ends, where the archive is,
  how many files it took and left out, and to read it before sharing.
  One runs at a time. The driven actions gain `export-diagnostics`,
  which has no chord.

## 5. Model client

**Dialect.** OpenAI Chat Completions as OpenRouter serves it at
`POST https://openrouter.ai/api/v1/chat/completions`. The base URL is
configuration, so another OpenAI-compatible endpoint is the same code;
because fetchd refuses loopback and link-local destinations, a model
server on the local machine is not reachable this way. OpenRouter's
Anthropic Messages and beta Responses endpoints are not used: the first
covers only Anthropic models, the second is stateless anyway. The
classifier's first stage uses OpenRouter's decisions endpoint,
`/api/alpha/decisions`, instead (§11).

**Headers.** `authorization: Bearer KEY`, `content-type: application/json`,
and the attribution pair `http-referer` and `x-openrouter-title: td-agent`.

**Request.** `model`, `messages`, `tools` (on every request, follow-ups
included, since OpenRouter requires them), `max_tokens`, `reasoning`
with an effort from configuration or the conversation's own choice
(§4), and `provider: {require_parameters:
true}` so a request is never routed to a provider that would silently
drop `tools` or `reasoning`. That routing is why every member a request
carries, but `model`, `messages`, `stream`, `provider` and
`cache_control`, which OpenRouter does not route on, must be one the
model's `supported_parameters` lists: a member no endpoint lists routes
the request nowhere, and OpenRouter answers 404, "No endpoints found
that can handle the requested parameters". So `reasoning` is sent only
to a model that lists it; a conversation's model that does not list
`tools` or `max_tokens`, which bounds what a request may cost, is
refused by name before any request, and a title model that does not
list `max_tokens` is asked for no title, with a notice saying why; and
nothing optional is sent. There is no
`parallel_tool_calls`, which few models list (none of Anthropic's,
OpenAI's or Google's), and no `tool_choice`, which some do not
(Amazon's Nova, for one) and whose `auto` is the default when `tools`
are sent. A model that runs calls in parallel does so unasked.
`provider.data_collection` is configuration (§15); the shipped default
is `deny`.

**Responses.** `finish_reason` is one of `tool_calls`, `stop`, `length`,
`content_filter` or `error`. Each `tool_calls[i].function.arguments` is a
JSON string, parsed under a bound; a call whose arguments do not parse
gets a tool-result error naming the parse failure, never a guess. The
calls in one response are approved and run one at a time, in the order
given, and each gets exactly one `tool` message with its `tool_call_id`,
in that order.

**Reasoning echo.** An assistant message's `reasoning_details` array is
stored once and replayed byte-identical on that message in every later
request. OpenRouter requires the sequence unmodified, and some providers
reject a tool-call continuation without it (Gemini's thought signature).
From a whole response it is the raw byte span of the array, spliced, not
re-encoded from a parse; from a stream it is the array as assembled from
the deltas, serialized once. Either way td-agent never re-encodes a stored
array afterward.

**Prompt caching.** Every request is the conversation's stored prefix plus
its messages (§6, §13). For `anthropic/*` models the request carries
top-level `cache_control: {type: "ephemeral"}`. Other providers cache
automatically. Cached and cache-write token counts are shown in the turn's
usage.

**Usage and cost.** The final response, or the final chunk when
streaming, carries `usage`, including `cost` in credits. It is recorded per
request and summed per turn, per conversation and across conversations.
`GET /api/v1/key` supplies the remaining credit for the
status row. `GET /api/v1/models` is fetched at startup and cached in the
state directory. It supplies `context_length`,
`top_provider.max_completion_tokens`, `pricing`, and
`supported_parameters`; a model lacking `tools` or `max_tokens` is
refused for a conversation with that reason.

**Spending limits.** Cost is known only after a response, so limits are
enforced by reservation. Before every request (acting, classifier,
compaction or title), td-agent reserves its worst case from the cached
pricing: the estimated prompt tokens at the highest prompt rate that could
apply (the cache-write rate where it exceeds the prompt rate), plus
`max_tokens` at the completion rate, plus any per-request fee. A request
whose reservation would carry the turn's accumulated cost past
`max_cost_per_turn`, the conversation's past `max_cost_per_conversation`,
or the day's total past `max_cost_per_day`, is not sent; the turn stops
and says which limit. A model without pricing cannot be reserved against
and is refused while a limit is set.

The review command (§2) has no conversation and no day's ledger, which
the window process alone keeps: each review fetches the models list and
reserves from it as a turn does, its prompt estimated at one token per
three ASCII bytes and one per byte of anything else, at the highest
prompt rate, its `max_tokens` at the highest completion rate, and the
per-request fee; it is not sent when that alone passes
`max_cost_per_turn`, or when the model is unlisted or unpriced while
that limit is set. Without that limit, a list that cannot be had leaves
the model unlisted rather than refusing the review. What it spends is not added to the day's total; it
is said on standard error, and the provider's own key limit is the
bound across reviews.

**Errors.** 401 and 402 are shown and stop the turn. 429 retries with
bounded exponential backoff, honouring `Retry-After`, at most three times:
a rate-limited request was not run. 502 and 503 are shown with a retry
action rather than retried automatically, because a provider may have
generated, and billed, before failing. Anything else is shown with the
provider's message. With streaming, an `error` object can arrive inside a
200 stream; every chunk is checked for one.

**Transport and streaming.** `td-fetch 1` answers a request with one
buffered body by default, under a body cap and a five-minute deadline over
the whole exchange. The first model-client increment uses that with
`stream: false`: correct, but silent until each response completes, and a
response that runs past five minutes fails. Streaming is its own work,
in two increments (§18):

1. **td-net (landed):** the fetch service's streamed response mode. The
   request head carries `stream`, and the reply after its head is a
   sequence of `chunk N` frames ending in `end` or `error kind: reason`,
   read through the client module's `post_stream`. The five minutes are
   replaced by a two-minute idle deadline on every origin read and write
   and a thirty-minute total, both the service's (APPLICATIONS.md §W.8).
2. **td-agent:** an SSE reader over those frames. It handles `data:` lines,
   skips `:` comment lines (OpenRouter's `: OPENROUTER PROCESSING`), stops at
   `data: [DONE]`, and bounds each event. Tool-call fragments are assembled
   by `index`. Text and reasoning deltas are drawn as they arrive.

Interrupting closes the fetch connection. Not every provider stops
generating, or billing, when the stream closes; the interrupt says so.

**As built (increment 5).** The conversation process makes each request
with `td_fetch::post`, `stream` absent, the reply capped at 512 KiB so
it fits a log line. `http-referer` is `https://github.com/timmydo/td`.

- **The body** is `{HEAD,"messages":[PREFIX…,MESSAGES…]}`. `HEAD` is the
  exact text of the other members, logged with the request: `model`,
  `max_tokens`, `reasoning: {effort}`, `provider: {require_parameters:
  true, data_collection}`, and `cache_control` for `anthropic/*`. The
  prefix's messages come from §6's prefix. `MESSAGES` are the user
  messages and turn replies logged before the request, a reply as
  `{"role":"assistant","content":…,"reasoning_details":…}` with the
  stored bytes spliced in. With no tools there is no `tools` yet, and
  the refusal of a model
  lacking `tools` waits for the first tools (§18 increment 8).
  `reasoning` is left out for a model whose cached `supported_parameters`
  lacks it, since `require_parameters` would otherwise route it nowhere.
- **`max_tokens`** is 16,384, or the model's `max_completion_tokens` if
  that is less, cut to what the context has left after §14's estimate. A
  conversation whose estimate already fills the context stops the turn
  and says so, until compaction.
- **`reasoning_details`** is found in the 200's bytes by a byte-span
  walk over the JSON (`span.rs`), stored as that text, and spliced back.
  The log holds it as a string, so the bytes sent again are the bytes
  received.
- **Errors.** 401, 402 and every other 4xx stop the turn with the
  provider's message, as does a fetch failure that left nothing sent: no
  fetch socket, a refused connection, or the service refusing or finding
  the request malformed before sending it. A 429 is retried up to three
  times, after its `Retry-After` seconds, or else 1, 2 and 4 s; a wait
  past 60 s stops the turn instead. A 408, any 5xx, a transport failure,
  a reply the service refused past the cap or its other response
  bounds, and an error inside a 200 (a top-level `error`, a choice's
  `error`, or `finish_reason: error`) end the turn with `C-r` offered
  (§4), and are never retried by td-agent. A provider's raw error
  message, where OpenRouter passes one on, is appended to its own, and
  either is cut to 500 characters.
- **Money** is counted in whole pico-credits (10⁻¹² of a credit) in a
  `u64`, parsed exactly from the decimal text a response or price
  carries. Prices round up and costs to nearest. The pricing used is
  `prompt`, `completion`, `request`, `internal_reasoning`,
  `input_cache_read` and `input_cache_write`. A negative price, as
  `openrouter/auto` gives, means no pricing, and so does a missing
  prompt or completion price. A reservation is the prompt estimate at the
  larger of the prompt and cache-write rates, plus `max_tokens` at the
  larger of the completion and reasoning rates, plus the request fee.
- **What a request cost** is its `usage.cost` where the response gives
  it; or else its tokens at the cached pricing, reasoning tokens at the
  larger of the completion and reasoning rates, where the usage gives
  both its prompt and completion counts; or else its whole reservation.
  A failure that may have been billed (the `C-r` class above) counts
  its reported cost where an error inside a 200 gave one, and else its
  whole reservation. A failure that ran nothing counts zero. Each
  `usage` record says which basis it used: `reported`, `computed`,
  `reserved` or `none`. An interrupted request counts its reservation
  only when no `usage` of it was logged before its process died.
- **The limits.** The conversation process checks the turn's and the
  conversation's limits against its own log before each request; the
  day's is the window process's. The child asks the window to reserve
  each request's amount; the window grants it only while the day's
  total stays within `max_cost_per_day`, and the child sends its cost
  when known, which replaces the reservation. The day is the UTC day,
  since td-agent carries no zone data, and its total is the state
  directory's `spend`, written before each grant is answered. A
  reservation still held when its child dies stays spent. Reservation
  ids start at random in each process, so no grant meant for one
  process of a conversation is taken by the next; a reservation the
  failed child asked for in the poll that saw it fail is not answered;
  and a grant that comes after its request gave up waiting (30 s) is
  released at once with a cost of zero. A model
  missing from the cached list is refused by name. A model with no
  pricing, or any model before the list is first cached, is refused while
  a limit is set; with all three `none` it is sent and reserves nothing.
- **The models list** is fetched at window start, without the key, by a
  thread of the window's, under a 16 MiB bound. It is cached as the state
  directory's `models`, and only `id`, `context_length` (or the top
  provider's), `top_provider.max_completion_tokens`, the pricing above
  and `supported_parameters` are kept. Conversation processes read that
  cache.
- **Credit** is `GET /key` with the key and no redirect followed, at
  start and after a turn ends, at most every 20 s. The row shows
  `limit_remaining` (a negative remainder as zero), or what the key has
  used when it has no limit.

**As built (increment 7).** A turn's request streams. Its head gains
`"stream":true` after `max_tokens`, so the logged head still rebuilds
the exact body; it is sent with `td_fetch::post_stream`, the frames'
sum bounded at 32 MiB, since every delta is a JSON object of its own,
many times the text it carries. A title request stays counted, with
`td_fetch::post` and its 512 KiB cap: it is short, drawn nowhere and
never interrupted. `td_fetch` was then td-news's copy, unedited; it is
now the td-fetch-client crate.

- **The stream** is read on a thread of the conversation process's own,
  which hands its head and each frame to the turn through the channel
  the window's frames come by, so the turn hears an interrupt between
  any two frames. The thread reads on only while the turn still wants
  its request; when the turn ends, however it ends, the thread stops
  at its next frame and drops the stream, which closes the connection.
- **The SSE reader** (`sse.rs`) reads `text/event-stream` as the HTML
  standard defines it: lines ending in LF, CRLF or a lone CR, a CRLF
  split across frames included; `:` comment lines skipped; `data` lines
  joined by line feeds into one event, dispatched by a blank line; the
  `event`, `id` and `retry` fields and a leading byte order mark
  ignored; an event the stream's end cuts off not dispatched. `data:
  [DONE]` ends the reply and nothing after it is read. Each line and
  each event's text is held to 256 KiB, and the whole stream to the
  32 MiB of the request's limit; its buffers are reused from event to
  event.
- **Assembly** (`assemble.rs`) reads each event as a chunk:
  `delta.content` and `delta.reasoning` are appended;
  `reasoning_details` fragments are joined as OpenRouter's own client
  (ai-sdk-provider) joins them, by the type's transitions and not by
  `index`, which providers reuse for distinct blocks: a
  `reasoning.text` or `reasoning.summary` fragment joins the entry just
  before it when that entry has its type, appending its `text` or
  `summary`; where upstream fills only a missing `signature` and
  `format`, any member the entry lacks or holds as null or empty is
  filled, the entry's first value of every other member standing; every
  other fragment, an encrypted block always, is an entry of its own.
  Tool-call fragments are assembled by `index`, `id`,
  `type` and the function's `name` from the first fragment that carries
  each and `arguments` appended, kept for the first tools (increment 8);
  `finish_reason` and `usage` are the last given. Text, reasoning and
  reasoning details are held to 512 KiB as the log line carries them,
  JSON escaping counted (twice for the details, stored as a string),
  so the reply fits a log line; the tool calls count the bytes they
  keep; at most 256 details entries and 256 calls. The details array
  is serialized once, when the reply ends, stored as that text, and
  spliced back as before.
- **Errors.** Every chunk is checked for an `error` object, at the top
  or in its choice, and for `finish_reason: "error"`; each ends the turn
  as an error inside a counted 200 does, charged its reported cost or
  else its reservation, with `C-r` offered. A chunk that is not JSON,
  and a stream past a bound, end it the same way. A head whose status is
  not 200, or a 200 whose `content-type` is `application/json`, is read
  whole under the 512 KiB cap and classified as a counted reply, so 429,
  401, 402 and 502 behave as before. Whether a reply is whole is its
  `finish_reason`'s to say: a stream that ends after one without
  `[DONE]` is whole, and one that ends before any, with `[DONE]` or
  without, or breaks off with a transport error, ends the turn with
  `C-r` offered and its reservation charged unless a `usage` with a
  cost had come, as a counted reply with no choice is. A stream broken
  or interrupted while its head's error body is read is charged so too:
  the reservation is the bound when nothing reported a cost.
- **What a stream brought** before it broke, failed or was interrupted
  is logged as an `assistant` event marked `incomplete` (§6), when it
  brought any text, reasoning or details, before its `usage` and
  finish. An incomplete reply is never sent back to the model: its
  reasoning details may lack the signature that closes them, and a
  retry asks the same request again, byte for byte.
- **Interrupting.** `Escape` (§4) sends `interrupt` (§2). During a
  stream the turn ends at once: its request is charged as a broken
  stream's, its turn finishes with `C-r` offered and an outcome saying
  the stream was closed but not every provider stops generating, or
  billing, when a stream closes. The connection itself closes when the
  stream's thread wakes for its next frame: `td_fetch`'s `Stream` can be
  closed only by the thread reading it, and the shared client (now the
  td-fetch-client crate) is not edited here. OpenRouter's
  `: OPENROUTER PROCESSING` comments make that prompt, and the service's
  two-minute idle deadline bounds it
  against a silent origin; a request interrupted before its head came
  is still sent, and closed once its head comes. An interrupt while the
  window reserves the request ends the turn before it is sent, the
  reservation released; one during a rate limit's wait ends the wait
  and the turn, as the window closing does.
- **Recovery** is unchanged: a request is logged and synced as started
  before it is sent, and one a restart finds unfinished is interrupted,
  never resent. Nothing of a stream is logged before it ends, so a
  process killed mid-stream leaves only that interruption.

**As built (increment 8).** Every request carries the conversation's
tools. They live in the prefix (§6, §13), which is now a JSON object
`{"tools":[…],"messages":[{system}]}`,
`messages` its last member, so a body is `{HEAD,` then the object's
members with its `messages` array left open, the log's messages, and
`]}`. A prefix written as increment 7 wrote it, an array of messages,
still rebuilds its requests byte for byte, and such a conversation gets
the object as a `prefix` event before its next request, as does one
whose prefix still carries the `tool_choice` and `parallel_tool_calls`
this increment first sent. A model whose cached
`supported_parameters` lacks `tools` or `max_tokens` is refused before
any request, naming the setting that chose it.

- **A turn** is a loop of steps, each one request reserved against the
  limits as any other. A whole reply's calls, whatever its
  `finish_reason`, run one at a time in their order; each is logged as
  `tool_call` and synced before it runs, and answered by exactly one
  `tool_result`, which goes back as `{"role":"tool","tool_call_id":…,
  "content":…}` after the reply, itself sent as
  `{"role":"assistant","content":…,"tool_calls":[…]}` with `content`
  null when it has no text. A reply with no calls ends the turn. A turn
  stops after 40 steps, each a reply (a rate-limited request asked
  again is the same step), saying so, every call answered; a message
  goes on from there. A reply that finishes `tool_calls` with no calls
  ends the turn as replied.
- **Arguments** are parsed under a 256 KiB bound as a JSON object whose
  members are all named by the tool; empty arguments are `{}`. Anything
  else, an unknown tool or a bound passed is an error result saying
  what was wrong, and the turn goes on. A result too long to log is
  replaced by an error saying so. A call that came without an id or a
  tool name, with an id past 256 bytes or one an earlier call of the
  reply has, or with a name past 64 bytes, fails the reply as an error
  inside a 200, charged and offered again: answered, it would make
  every later request one the provider refuses. The tool calls' bytes
  count against the reply's bound escaped, as the log carries them.
- **Log room.** A request is sent only while the log has room for its
  reply and for each of the 256 calls a reply may make to be answered
  without running, and a call runs only while there is room for its
  result at its longest (a log line) and for the later calls' answers;
  past that it is answered as not run, the log full. So a reply's calls
  can always be answered, by the process or by the repair at load.
- **Interrupting** between calls answers each call not yet run as not
  run, and the turn ends with `C-r` offered, which goes on from the log.
  An incomplete reply's calls never run.
- **The title** follows the first turn the human began (their message,
  or a retry of it) that had a whole reply, however that turn ended,
  quoting the human's first message and the last reply. A turn a
  message from another conversation began does not count, so a
  conversation first woken by one is titled after the human's first
  turn.

## 6. Credentials and the conversation store

**The API key** is held only by the agent processes (§2).

- **On a host:** `$XDG_CONFIG_HOME/td-agent/openrouter.key`, holding one
  line. It is opened without following a final symlink, and the opened
  descriptor, not the path, is checked: a regular file with one link,
  owned by the caller, mode 0600. Every ancestor directory up to `/` must
  be owned by the caller or by root and writable by neither group nor
  others. That is stricter than td-ui's control-socket rule, which admits
  sticky ancestors, because nothing about a key file needs a shared
  directory. Anything else is refused by name. There is no
  environment-variable form: one mechanism, and nothing that a child
  could inherit.
- **On td:** the `td.Secret1` portal credential `agent/openrouter`,
  retrieved the way td-mail retrieves `mail/main` (APPLICATIONS.md §W.4),
  and written with `td-secret set agent/openrouter`. The stock VM is
  unenrolled, so there the portal's refusal is the expected outcome and is
  shown as such.

The key is never logged, rendered, written into a conversation, sent to
the classifier, or present in any jail instance's environment or
filesystem; §8's admission rules keep its file out of every source a jail
binds. Git credentials are the human's own and are used only by the git
worker outside any jail (§9).

**The conversation store** is `$XDG_STATE_HOME/td-agent/` on a host, and
the application's persistent state directory on td. No jail instance ever
sees it. Each conversation is a directory named by a random id (one the
human deleted (§4) is renamed `.deleting-<id>` under its lock and then
removed; a listing skips it, and the window at its start finishes a
removal whose lock nobody holds), holding:

- `meta`: title, workspace, model, mode, parent conversation when forked,
  creation time, whether it is archived, and a `role` kept only as data
  (§3).
- `prefix`: the exact bytes of the request prefix (§13), written once at
  creation and never rewritten. A later td-agent whose prompt or tool
  definitions differ appends a new prefix as a log event instead, which
  costs that conversation one cache miss and keeps every earlier request
  reproducible.
- `log`: an append-only newline-delimited JSON event log, each event
  carrying a sequence number. Each event is one of:
  - a user message, or a message from another conversation or a
    schedule, with its source (§3);
  - a request as sent: its prefix version, its parameters (model,
    reasoning, provider, `max_tokens`), and the messages after the prefix;
  - an assistant message as received, with its raw `reasoning_details`;
  - a tool call;
  - an approval decision, with who decided it (rule, classifier stage, or
    human), its probabilities where Jev gave them, and the reason;
  - a tool result as returned to the model, and the full result when the
    returned one was cut;
  - a step snapshot (§12);
  - a todo list as written (§12);
  - the human's choice of the conversation's model and effort (§4);
  - a background process started, exited, killed or lost (§12);
  - a notification or notice delivered to the conversation (§3, §7, §12);
  - usage and cost;
  - a compaction (§14);
  - an interruption, an undo or a redo.

Every request ever sent is a pure function of `prefix` and `log`, so
replaying the log reproduces the exact bytes previously sent, which is
what keeps provider caches warm across restarts. Appends are written whole
and fsynced at turn boundaries, and before any effect runs its started
record is synced as well (Recovery, below). A torn final line is dropped
on load, and dropping it is reported. The log doubles as the audit
trail: no jail instance can write it, and the classifier's and the
human's verdicts are in it. A conversation can be forked at any
message, which copies its prefix and log up to there into a new
conversation, and exported to a local file; nothing is shared through
any service.

**Recovery.** Every effect is logged as started, with an id, and synced
before it runs, and logged as finished after: a model request, a tool
call, a background process, a delivery. A conversation process that
starts and finds an effect started but not finished never repeats it. A
tool call becomes a result telling the model that a restart interrupted
it and its effect is unknown, so the model checks the state; a model
request is recorded as interrupted, its reservation counted as spent at
its full amount, and the turn waits for the human or the next message
rather than resending; a background process is recorded as lost.
Messages between conversations carry delivery ids, the window process
keeps undelivered ones in the state directory, and a receiver logs each
id once, so a restart neither loses nor repeats a delivery.

**As built (increment 4).** `meta` is a JSON object at version 1 whose
workspace, model, mode and parent are null until the increments that set
them. `prefix` is empty, since there is no request yet. The log holds a
user message with its delivery id, a `turn` effect started and finished
(`no model`), an interruption, and a notice. A user message and its
started record are synced together, and the finished record when the
turn ends. On load, a torn final line is truncated away and a notice
saying so is appended; a whole line that does not parse, or a sequence
gap, refuses the log; a user message logged without its turn's start,
the process having died between the two lines, is given one; and an
effect started and not finished gets one interruption record. A log
longer than 256 MiB is refused rather than read, so a message is
accepted only while the log has room for it at its longest and 64 KiB
more for the records that follow it, and refused by name past that.
Files are opened without following a final symbolic link, and
directories are created mode 0700. The human's unacknowledged messages
are kept in the window process's memory (§2), not the state directory:
there is not yet a message between conversations to keep there. One
still held when the window closes is written whole to standard error.

**As built (increment 5).**

- **The key file** is read once, by the window process at start (and
  again, through the same checks, only by the diagnostics export of §4,
  which reads it to leave out any file holding it), with std alone and
  no `unsafe`. The path is walked a component at a time,
  each opened `O_PATH | O_NOFOLLOW` beneath the descriptor of the one
  before it (through `/proc/self/fd`), so no component can be swapped
  between its check and its use. A symbolic link met on the way is read
  and followed by the same walk, at most 40 of them, and every directory
  reached is checked by its descriptor's metadata: owned by the caller
  or root, and neither group- nor other-writable, with no exception for
  a sticky bit. The file itself is opened `O_NOFOLLOW | O_NONBLOCK |
  O_NOCTTY` beneath its directory, then checked as above, and must hold
  one line of printable ASCII of at most 4,096 bytes, its newline
  optional. Each refusal names the path and what is wrong with it. A
  missing file is not an error at start: every turn ends saying where to
  write the key. The key reaches no log, transcript, argument list,
  environment or standard error; its `Debug` form is redacted.
- **The prefix** is a JSON array of the system message, written at
  creation from `td-agent/prompt/`. A conversation whose prefix differs
  from the one this program writes (an empty prefix from increment 4,
  say) gets a `prefix` event holding the new one before its next
  request, and requests name the prefix they used.
- **New events.** A `request` (its turn, its purpose, `turn` or `title`,
  its prefix, its exact head, its body's length and its reservation) is
  a started effect, synced before it is sent; an `assistant` message
  (its text, its reasoning text, its raw `reasoning_details` and its
  finish reason); `usage` (tokens, cost and its basis, §5); a `title`;
  and `finished` records that carry `retry` when the human may ask a
  failed turn again. A title request's head holds its whole body, which
  quotes the first exchange; a turn request's body is rebuilt from the
  log, and a test holds that the rebuilt bytes are the bytes sent.
- **Recovery** applies to requests as to turns: one started and not
  finished is interrupted, never resent, and its reservation counts as
  spent in the conversation's total.

**As built (increment 7).** An `assistant` event may carry
`incomplete: true`, absent when false, for what a stream that broke,
failed or was interrupted had brought (§5). It is shown and kept in the
log, and left out of every later request, so requests remain a pure
function of `prefix` and `log`. Only whole replies count toward the
conversation's first reply, after which a title is asked for, and only
a whole reply is quoted in the title request.

**As built (increment 8).** `meta` gains `paused`, absent in older
ones and read as false. New events:

- `message`: a message from another conversation, with its delivery id,
  sender, sender's role (data only, §3), text, the status of a report an
  older td-agent sent, and `held` (`paused` or `budget`) when it started
  no turn; its delivery id is logged once, as a user message's is.
- `tool_call`: the reply it belongs to, the call's id and tool, logged
  and synced before the call runs.
- `tool_result`: the reply, the call's id and tool, the `tool_call` it
  finishes (0 for a call never started), the content as returned to the
  model, and whether it is an error.
- `todo`: the whole list as written, marked `cleared` when the human
  cleared it.
- `pause`: the human paused or resumed the conversation.
- `approval`: the decision on a call, by whom (`human`; `td-agent` when
  a card was withdrawn; from increment 13, `rule` when a rule refused or
  allowed it, `mode` when `auto` mode ran it, and `classifier` when the
  classifier allowed it or, outcome `ask`, gave it to the human), and
  Jev's
  probabilities and the reason; the history tools show only its outcome
  and who decided.

An `assistant` event carries its `tool_calls` (id, tool and the
arguments as the model wrote them) when it made any. On load, every
call of a whole reply that has no result gets one, before any later
request could need it: a call found started is answered that a restart
interrupted it and its effect is unknown, and is reported as
interrupted; one never started is answered as not run. Neither runs
again. A message logged without its turn's start, and not held, is
given one, as a user message is.

**As built (the Conversation menu).** `meta`'s `model` holds the model
the human chose for the conversation, and a new member `effort` the
effort, each null for the configuration's and in older ones. A new
event, `choice`, holds both, whole, each null for the configuration's;
the last one is the record, and `meta` is put right from it on load
(§4).

**As built (the File menu).** The key file can also be written, by the
window process, from the key dialog of §4. Nothing else writes it.

- **What is taken.** The entry holds at most 256 bytes. The text is a
  key when it is one line of printable ASCII, with no spaces, at most
  256 bytes and not empty; anything else is refused with why, naming
  the fault and never the text. One that does not start `sk-or-` is
  stored with a warning, since `base_url` may name another provider.
- **The write** walks the path as the read does: every directory from
  `/` down to `$XDG_CONFIG_HOME` is checked, by descriptor, owned by the
  caller or root and writable by neither group nor others, before
  anything is made, and a missing `$XDG_CONFIG_HOME` is refused, not
  made. `td-agent` is made mode 0700 (exactly, whatever the umask,
  set by its name beneath the checked parent, which needs no read
  access a umask may have taken) when it is missing, and
  `$XDG_CONFIG_HOME` synced; then the walk is taken
  again to `td-agent`. A `.gitignore` naming `/openrouter.key` and
  `/openrouter.key.tmp` is made there next, unless one is, before any
  key is: a configuration home is often a git repository of dotfiles,
  and a key committed and pushed there is published. It names the two
  files rather than `*`, so the `config` beside them can still be
  committed. One already there, a link included, is the human's and left
  as it is, whatever it says; so a new one is written whole under
  `.gitignore.td-agent.tmp`, made new without following a link, set to
  exactly 0644 by its descriptor so git can read it whatever the umask,
  synced,
  and hard-linked in, which fails rather than replace one that came
  meanwhile, and a crash never leaves part of one to be taken for the
  human's. That temporary name holds no secret, so one a crash left is
  removed. A key placed by hand gets none until a save from the dialog.
  It keeps an untracked key out of `git add`; it does not untrack one
  already committed (`git rm --cached` does, and a key once pushed is
  to be revoked), nor stop `git add -f`, nor bind a dotfiles tool that
  copies files by its own rules. The directory is synced once it is
  made, since a save may stop to ask before its own sync.
  A key path that is there and not a regular file,
  a symbolic link included, is refused, saying to remove it; a regular
  one is replaced only when the confirmation of §4 said so, and one
  that comes after that check is kept: without the confirmation the
  temporary file below is hard-linked as `openrouter.key`, which fails
  if one is there, rather than renamed over it. The key and
  one newline are written to `openrouter.key.tmp` beside it, made new
  without following a link (`create_new`, `O_NOFOLLOW`), set to exactly
  0600 by its descriptor, checked by that descriptor to be a regular
  file of the caller's with one link, and synced; it is renamed over
  `openrouter.key`, or linked as it and removed, and the directory
  synced. The temporary file's name is fixed: one that is there, a link
  or a file a save that did not finish left, is refused by name and
  left alone, and one a failed write made is removed. A save cut short
  between its write and its rename, by a crash or a kill, leaves the
  key in that temporary file, mode 0600 in the 0700 directory, until
  the human removes it, which the next save asks for. Last, the file is
  read back through the read's own checks, which it must pass, holding
  the key written. A refusal after the key file was replaced says that
  it was; the window does not then hand the key on.
- **After the write** the window hands the key to every conversation
  process in a fresh `setup` and keeps it for every one started later
  (§2), and asks the key's credit with it. Every refusal says what is
  wrong, naming the path at fault where there is one, and goes to
  standard error and to the dialog; the key goes to neither, nor to a
  notice, the driven state, the clipboard, an argument, the environment
  or any file but `openrouter.key` (and the temporary file a save cut
  short leaves).
- **On td** the key is the portal credential (above), and the file is
  not read. td-agent runs only as `./install-apps` installs it on a
  host until the packaging increment, so there is no jailed run for the
  window to tell apart today and the item is always shown; the packaging
  increment hides it in the jailed run, or has it say that the key is
  set with `td-secret set agent/openrouter`.

## 7. Workspaces

A workspace is what a conversation's tools work in, a policy, and one
conversation: a private empty directory, a directory of the human's, or
a directory of sparse git worktrees. It is made when the human creates
its conversation, from a template, and lives until that conversation is
archived or deleted (Archiving and deleting, below). Creating one is the
human's act; no model creates a workspace, and nothing a model says
chooses a template. A conversation made before templates may have no
workspace, and then has no file, shell or git tools.

**Templates.** Every new conversation starts from a workspace template
the human chooses. `C-n` and File → `New conversation…` open a chooser,
td-ui's finder as the model picker uses it (§4), listing the built-ins
first and then the configuration's `[[template]]` entries (§15) in the
order written; `Return` creates the conversation from the one selected
and `Escape` creates nothing.

- **Empty** is always listed: a private, empty scratch directory in the
  conversation's jail directory, with the file and shell tools and no
  git checkouts, for general assistance or scratch work (Empty and
  directory workspaces, below).
- **Directory…** is always listed: the folder chooser, over a directory
  of the human's, admitted per §8.
- **A configured template** is listed by its `name`. It names the git
  repositories to check out, each a remote, a base, a branch and sparse
  paths; the shared directories its workspaces bind, when it names
  them, in place of the configured `shared` list; and, later, its
  network policy (§10). A
  template that names no repository makes an Empty workspace with its
  own shared directories.

Choosing is the decision: no card follows it but a remote's admission
(below). A template's repositories are prepared by increment 11's git
worker (below; As built (increment 11, preparation)). The conversation
starts on the human's first message,
which can be sent at once. Two workspaces made from one template work
on branches of the same name, each in a repository of its own (below),
and a push names the remote branch it writes (§9).

The templates step builds the chooser, `[[template]]` and the two
built-ins, which replace File's two workspace items (As built
(increment 10, workspaces), below); increment 11 builds what a
template's `repos` asks for: the store, the git worker, workspace
repositories and their worktrees, everything from Layout to Keeping
current below, and the workspace card.

**As built (templates).** `C-n` and File → `New conversation…`, now
File's one New item, open the model picker's finder over Empty (its
meta `scratch`), Directory… (`folder`) and each configured template in
the order written (`scratch`, or `repositories` for one naming any),
Empty selected: typing filters, `Return` or a second press chooses, and
`Escape` makes nothing. Empty starts a conversation in a scratch
workspace, and Directory… opens the folder chooser. A template naming
no repository makes a `{"kind": "template", "name": …}` workspace, a
scratch directory as Empty's is, which binds the template's own
`shared` list when it names one. Each such list is admitted at the
window's start as the top-level one is, a refusal named with its
template, and handed to every conversation process beside the
top-level list; a conversation finds its template's list by name, so
an edited list applies from the next start. A template that names no
list binds the top-level one, as Empty does; one no longer configured,
removed or renamed, binds none, so that no edit widens what its old
conversations reach without the human choosing it. When the lists
would take more than half of a setup frame they are not handed on,
said in a note, and template workspaces bind none. The list's
Workspace column and the `conversations` tool name the workspace
`template NAME`, and the
question before a deletion says, by the workspace's kind and not its
name, that a template's scratch directory goes with the conversation.
A template naming repositories was listed then, the finder's note
saying it was refused until increment 11, and choosing it made nothing;
increment 11 prepares it (As built (increment 11, preparation)). Choosing Directory… goes straight to the folder chooser: a card
that waited behind the template chooser waits behind it too.
Without the launch's jail no workspace can be made and there is nothing
to choose: `C-n` starts a conversation with none at once, the reason on
standard error, which is how the native compositor tests run.

**The workspace card.** A repository workspace has a card of its own,
shown in its conversation once its bases are fetched and kept behind its
status row: it lists each repository's `.td-agent/rules`, shows the
project instructions beside the mark that trusts them (§11, §13), and
recommends against opening the tree in tools that execute on open
(§8). It decides nothing about the creation, which the human's choice
of template already made.

**As built (increment 11, the workspace card).** While the open
conversation has a repository workspace, the status row says `workspace:
C-S-w` after its notes, and `C-S-w` or Conversation → `Workspace card…`
(shown with `C-S-w`, off otherwise) opens its card, modal over the body
in the Messages window's panel and with its keys, titled with the
workspace's name, at its first entry rather than following the newest;
`Escape` or `C-S-w` closes it. The window reads, as it opens, the
conversation's recorded project instructions and which of its workspace
repositories `meta` says are prepared, and shows: first the
recommendation against opening the worktrees in a tool that runs code on
opening a folder, then one entry for the worktrees of a remote read at
one commit alike, as the prompt groups them (§13), and one for a
remote's worktrees not read yet. An entry names the remote, each
worktree with its base and branch and whether it is checked out, and
what the model is given at the commit: the file's text as the prompt
carries it, every control, whitespace other than a space, and invisible
or bidirectional character (a right-to-left override, a zero-width
space) named as `<U+XXXX>`, and a `<` that begins such a name in the
text named too, so a name on the card is always one; or that there is
none, or was not read and why, or is read once the commit is fetched. It
does not catch look-alike letters from other scripts. A record that
cannot be read is said, and what it would have told is then unknown, not
guessed. An entry too large for the list says so in its place; no entry
gives way to another, the recommendation least of all. It is what was
read when it opened, and opening it again reads again; it opens only
over no other modal, for the open conversation, and a note while it is
open is counted, not added to it. The control seam's state ends with
`card`, `open` or `closed`, and its `workspace` action is `C-S-w`. Each
entry also says what its repository's `.td-agent/rules` add at the
commit, listed as the matcher reads them, or that there are none, or why
they were not read (§11, As built (increment 13, a repository's rules)).
It does not show the trust mark, which changes nothing until increment
13's classifier and is added with it, so the card does not offer what
would do nothing yet.

**Layout.**

```text
~/td-agent/<name>/                    the workspace tree, the human's to read
  <repo>/                             a sparse linked worktree per entry
  <repo>-<branch>/                    a second worktree of the same repo
$XDG_DATA_HOME/td-agent/
  store/<host>-<path>-<digest>.git    one bare repository per remote
  ws/<name>/<repo>.git                the workspace's own repository
  publish/<name>/<repo>.git           what the git worker pushes from
```

- **The store** holds one bare repository per admitted remote, cloned and
  fetched by the git worker (§9). It is the only place objects are
  downloaded, and no jail can write it.
- **The workspace repository** is per workspace and per repo. Its objects
  borrow the store's through `objects/info/alternates`, so creating one
  copies nothing; it has its own refs, its own configuration written by
  td-agent, and the workspace's linked worktrees. Workspaces never share a
  repository, so one workspace's jail can move no other workspace's
  branches. It is jail-writable, so no git outside a jail ever reads or
  writes it (§9).
- **The publish repository** is per workspace and per repo, also over
  alternates to the store, and is never mounted into any jail. It holds
  only commits imported for a push (§9).
- **A worktree** is a linked worktree of the workspace repository at
  `~/td-agent/<name>/<repo>/`, on the branch the entry names, created from
  its base, with a cone-mode sparse checkout of the entry's paths. Sparse
  checkout is enabled in the repository's shared, read-only configuration,
  and each worktree's patterns live in its own `info/sparse-checkout`,
  which the agent may change from inside the jail with `git sparse-checkout
  add` or a `set` naming no mode, which rewrite only those patterns; a
  `set` naming a mode (`--cone` included) and `disable` write
  configuration and fail there, `disable` after first checking the
  whole tree out.
- The workspace tree's root is `workspace_root` (default `~/td-agent`),
  admitted like any source (§8).
- A workspace's `<name>` is made by td-agent from its template's name
  and its conversation's id, so that no two workspaces share one.

**Admitted remotes.** A remote is the human's decision: a URL, or a host
with a path prefix matched on whole path segments after normalization, in
`remotes` in configuration or added through a card.
Only `https` and `ssh` transports are admitted, never `file`, a local path,
`ext` or plain `http`. Naming a remote in a template does not admit it:
choosing a template whose remote is not admitted asks the human, on a
card in both modes, to admit it, saying that cloning it runs outside any
jail with the human's credentials and bypasses the egress relay;
refused, nothing is made. A remote of another transport is refused
outright. Branch and base names are checked as git ref names before use
and passed after `--end-of-options`.

**As built (increment 11, admission).** Choosing a template asks, on
one modal card titled `Admit remotes`, about every remote it names that
neither `remotes` in configuration nor an earlier card admits, each
once as td-agent records it, and only once every other check of the
template has passed (As built (increment 11, preparation)), so no
admission outlasts a template refused for something else; a remote of
another transport is refused that way, never asked about. The card
names the template, how many remotes it asks about when more than one
(all admitted together) and the remotes, and says that, admitted,
td-agent's git clones and fetches each for the human, outside any jail
and with the human's git credentials, bypassing the egress relay, and
that each stays admitted, with or without a final `.git` (an exact
admission compares paths so), for every later workspace. `Cancel` is
focused first and a key that comes as the card is shown decides
nothing, as on a tool's card (§11); `Cancel` or `Escape` makes nothing
and says so in a note. Losing the keyboard sets the card aside, and it
is asked again when the keyboard comes back; a window grown too small
for it closes it, saying nothing was made. `Admit` records each remote,
exactly, in the window's `remotes` file in the state directory (one URL
a line, oldest first, replaced whole, at most 256) and then makes the
workspace. A file the window cannot read at start (a line td-agent
would not have written, more than 256 lines) is moved aside, to
`remotes.set-aside-<time>`, and said, so none of it is admitted and a
card can admit again. An admission is the human's alone: nothing a
conversation sends admits a remote, and a conversation's ask for a
store is answered only for a remote and bases its own workspace record
names, and only while an admission covers the remote (As built
(increment 11, preparation)). Removing a line from the file, or from
the configuration, ends that admission at the window's next start.

**Asynchronous from the first increment.** Creating a conversation from
a repository template returns at once. Each worktree then moves through
`fetching` (clone or fetch of the store), `checking-out` (workspace
repository, worktree, sparse checkout, done in a maintenance instance,
§9) and `ready`, or `failed` with the reason. Several worktrees, and
several workspaces, prepare at once, bounded by `fetch_concurrency`. The
conversation's first model request waits only for `fetching`: its
project instructions and the repository's `.td-agent/rules` are read
from each base commit in the store by the git worker outside any jail,
with `cat-file` on the trusted store (§13); they are upstream's content
at the base and need no checkout. A tool call that touches a worktree
still checking out gets a result that names its state, and the
conversation is notified when it becomes ready or fails, which the
window shows too (§3, Notifications). A jail instance binds a worktree
only once it is ready; the long-lived file-tool instance is replaced
whenever the set of ready worktrees changes.

**As built (increment 11, a call told a worktree's state).** Before
any card is asked, a workspace tool's call is refused when an
absolute path it names, or the working directory it runs in when it
names none (`shell` without `workdir`, `glob` and `grep` without
`path`), is in a worktree not prepared: `<checkout> is still being
checked out; td-agent tells you when it is ready` while the process
is waiting for its store or checking it out, else `<checkout> could
not be prepared; td-agent tries again when this conversation is next
opened`, which is when its process next starts (a conversation left
while its process works on keeps it). A path is judged as written,
`.` and `..` taken lexically, against each worktree's checkout by
whole components; a relative path is left to the tool host, which
refuses every one (§8). The jail refuses such a call anyway; this
says why, and asks the person nothing about a call that cannot run.
What a command reaches by itself, outside its working directory, is
still the jail's to refuse. A checkout that ends during a step's calls
is taken up before the next request, so a later call in that step is
still told it is checking out.

**Keeping current.** The git worker fetches each store remote in the
background every `fetch_interval` (default ten minutes) and on demand
(`git_fetch` from a workspace's conversation, §9). It then updates each
workspace repository's remote-tracking refs from the store in a
maintenance instance, so a workspace sees new upstream commits without
any network in its jail, and notifies each workspace conversation whose
base advanced, which the window shows too. Rebasing is the
conversation's own work.

**As built (increment 11, background fetch).** At its start and every
`fetch_interval` after, the window asks its store thread to fetch, in
the background, each remote a repository workspace names whose
conversation is neither archived nor gone with its archive, while the
configuration or a card admits it, with every base those workspaces
name on it; a remote whose last fetch has not answered is not asked
again. The thread runs one job at a time, a conversation's ask before
any background fetch queued ahead of it, so a preparation waits only
for a fetch already running, which `FETCH_TIME` and the stall limits
bound; a background fetch only fetches and resolves each base, a base
upstream deleted failing alone. A fetch or base that fails is said
where td-agent's diagnostics go, not in the window, once until what it
says changes or it mends. The window keeps, in memory, the commit each
base it has seen was at as each fetch, a preparation's or its own,
last found it, and when a base it knew moves, forward or back, says so
in a note, `upstream moved: <base> of <remote> is at <commit>`. That
is a cache of what the store said, blind to what moved while td-agent
was not running, so it decides nothing for a workspace: a workspace's
remote-tracking refs follow by comparing the store's commit with its
conversation's own record (As built (increment 11, remote-tracking
refs)).

**As built (increment 11, remote-tracking refs).** Once its worktrees
are checked out, a conversation's process sets each base's
remote-tracking ref, `refs/remotes/origin/<base>` in the workspace
repository, to the commit the worktree started at, in a maintenance
instance (`track`, below), clearing the ref locks a killed run of it
left, as a checkout does, and records it in `meta`'s `tracked`, a
remote, a base and a commit each, which only that process writes and
never from what a jail could have written; a repository whose refs
cannot be set is not prepared, and is tried again as any preparation
is. A retried preparation keeps a worktree an earlier run made at the
base resolved then, but sets the ref to the base now, so a move between
the two is not said, as the project instructions it records are read
at the base now. A process with a repository prepared asks the window, at its
start, where the window last found its bases (`Heads`), and the window
answers from what it keeps of each fetch and tells every running
conversation whose workspace names a remote after each fetch of it, a
preparation's or its own. A conversation not running is told when its
process next starts, so a base that moved while td-agent was closed is
caught by the first fetch after. Told a commit other than the one it
recorded, between turns (one told during a turn waits for its end),
the process sets the ref there, whatever the jail left in it, records
it, and logs a notice, `upstream's <base> moved from <commit> to
<commit> in <remote>: each worktree's refs/remotes/origin/<base> names
it now`, a notification (As built (increment 11, notifications)),
which the window shows; a base it had no record of, from a
meta written before, is set and recorded silently. Every ref is set in
one transaction, or none is, so the record is never half right. A
failure is logged once for each remote until it says something else or
the refs are set, and the same commits are not tried again until they
change or the process starts again, so a lasting failure does not
start an instance every fetch. A write that would take `meta` past
what is read back is refused. A failure is a notice, the human's to
mend, not the model's news. Nothing wakes the conversation: the model
reads the move at its next turn, as §3 has it. Rebasing stays the
conversation's own work.

**As built (increment 11, notifications).** td-agent's news of a
repository workspace is a `notification` in its conversation's log,
beside the `notice` the store and td-agent's own troubles use: a
repository checked out and ready or not prepared, and a base that
moved upstream. The window shows a notification as it shows a notice,
on the row and in the transcript; `history_search` finds both under
the kind `notice`, so the tool's schema, and the prefix, is unchanged.
Unlike a notice, a notification is given to the model, as a user
message beginning with the line every message has, then the label
`[td-agent's news of this workspace, not from the person]`, then its
text; the environment of a repository workspace's prefix names the
label, says td-agent tells the model when a worktree is ready or fails
and when a base moves, and that the line and the label are td-agent's
but what the news quotes from git, the remote or the jail is not, and
asks nothing by itself, a message from the person still the one to
answer. A failure's reason is quoted on one line, every control named,
and cut to 1,000 characters. The sentence every conversation's prefix
has about the received line is left as it was, so no other
conversation's cache is lost; `history_search` takes `notification`
as a name for `notice`, its schema unchanged. When a notification is
logged, and whether it wakes the conversation, is As built (increment
11, asynchronous preparation).

**As built (increment 11, asynchronous preparation).** The first turn
waits for each store's answer, whose project instructions are
recorded on the conversation process's main thread; the checkout then
runs on a thread of its own (`Checkout`), holding nothing of the
conversation's, so that turn and any later one go on meanwhile. The
thread runs each worktree's checkout and then sets the remote-tracking
refs, each in a maintenance instance as before, and hands back the
commits set or why not. td-jail ties an instance to the thread that
starts it; a maintenance instance runs within its call, the call
waiting for it to end and killing it at its deadline, so the thread
outlives its instance and the instance still ends with the process.
Tool instances, the file tools' long-lived one among them, are still
started from the main thread. What the thread hands back is taken up
where its news can be read in order: during a turn, before each
request (`between`), and before a request rate-limited is asked
again, its worktrees then bound for that step's calls, so the
notification falls after every result of the step before, never
between a tool call and its result, and the next request reads it in
the same turn; between turns, at once. The
repository is then recorded prepared, which binds it from the next
call, its commits recorded, the notification logged, and the window
told the process is done with the store (`Prepared`), which it keeps
the process for until then; a second answer for a store whose
preparation's end is not yet taken up, a checkout running or a
failure before one, waits for the same `Prepared`. Ready, the process asks where its bases
are now (`Heads`), so a move while it checked out is not missed. A
notification taken up between turns wakes the conversation, a turn
started of it (`Started`'s `of` names the notification) that reads
it: only when the person has written to the conversation, so a
conversation opened and never asked anything spends nothing; when it
is not paused, its news then read by the next turn, which resuming
alone does not start; when there is a key to ask with; while the
window is there; when nothing the window sent is about to start a
turn that would read it anyway; and when it does not say what the
last news of its remote said, so a failure each process start meets
again, which the model has read, buys no turn. Such a turn is not
counted against the wake budget, which counts turns started of a
message (§3), and its `Started` is sent before `Prepared`, so the
window, seeing the turn, keeps the process for it. A preparation that
fails before its checkout starts, a store refused, instructions that
do not fit or a thread that cannot start, is kept as a checkout's end
is and said, and wakes, alike. A thread that panics hands back that it
ended. A process that ends while a checkout runs takes its instance
with it, and the next process asks again, the checkout taking up what
the last one left. Two remotes' checkouts, and a checkout and a
prepared repository's `track`, may now run at once, each its own
repository; their maintenance instances share the conversation's
maintenance home, which their git, its `HOME` `/nonexistent`, does
not use, and each spec has a name of its own.

**As built (increment 11, preparation).** Choosing a template that
names repositories makes its workspace's record at once, in the
window: every remote admitted (one that is not is asked about first,
As built (increment 11, admission)),
every base and branch a branch name td-agent passes to git, no branch
named twice for one remote, at most 32 entries, no more directories
bound than td-jail binds (32: its worktrees, repositories, stores and
shared directories together), and a record of at most 16 KiB. The
workspace is named from its template and the conversation's id,
`<template>-<8 hex>`, and reserved by making `ws/<name>` in the data
directory, which only one making can do: a name another workspace
holds, there or as a tree under the workspace root, is passed over for
a new id. Entries naming one remote share its repository,
`ws/<name>/<repo>.git` in the data directory, each with a worktree of
its own, `<repo>` for the first and `<repo>-<branch>` for the rest,
under `<workspace_root>/<name>/`; and every path is fixed in `meta`'s
`{"kind": "repositories", …}`, so an edited template moves nothing.
The data directory is `$XDG_DATA_HOME/td-agent`, else
`~/.local/share/td-agent`: a directory and no link, made the caller's
alone (0700, an older one narrowed) and named as it resolves;
td-agent's part of every data home is refused to directory workspaces
and shared directories (§8). The conversation's process, at its start
and after any restart, asks the window for each remote whose
repository it has not recorded prepared (`Fetch`, with its bases). The
window checks the remote and bases against the conversation's own
record and the remote against what is admitted again, and hands it to
its store thread, which runs the git worker on the stores outside any
jail, one fetch at a time (`fetch_concurrency` is not read yet, and
the background fetches of Keeping current wait their turn there too),
and answers with the human's identity and each
base's commit, or why not. The process then lays the repository out
and checks each worktree out in a maintenance instance (§8, As built
(increment 11, the layout)), with the host's git by the path it
resolves to, records the repository in `meta`'s `prepared`, and says
in a notification in its log that it is ready, or why it is not, which
the window shows and the model reads (As built (increment 11,
notifications)); one that fails is asked for again when a process for the
conversation next starts, as on opening it again. From its ask until
it says it is done (`Prepared`) the window keeps its process when the
conversation is left, as it keeps one with a turn under way. Its
instances bind a repository, with its checkouts and its store's
objects, only once it is recorded prepared: until then no instance but
maintenance binds it, so what a run cut short left is td-agent's own,
and the next run uses a whole repository or worktree id again, removes
a checkout without its id, takes an index as a finished checkout,
clears the git locks a killed run left (the worktree's `index.lock`
and `HEAD.lock`, the branch's), and keeps a branch an earlier run made
where it made it, at the base resolved then, so a retry never moves a
branch. A checkout's git has ten minutes in all, and its instance
longer, so a failure cleans up before the instance ends. A call into a
worktree not yet ready is refused as outside the tool host's roots,
and one that names no directory while the first worktree is not ready
is refused (the tool host is told its working directory,
`--directory`, rather than taking its first root), never run in
another root; the conversation says so first, naming the worktree's
state (As built (increment 11, a call told a worktree's state)). The prefix names every worktree with its remote, branch,
base and paths whether or not it is ready, so it holds while they
prepare. The first turn waits for each store's answer, which carries
the project instructions (§13, As built (increment 11, project
instructions)); the checkout then runs on a thread of its own, and its
news wakes an idle conversation (As built (increment 11, asynchronous
preparation)), and a call into a worktree not ready is told its state
(As built (increment 11, a call told a worktree's state)). Rules from
the base are a later step (the background fetch,
the remote-tracking refs and the notifications are As built
(increment 11, background fetch), (increment 11, remote-tracking
refs) and (increment 11, notifications)). Deleting the conversation removes its
repository workspace (As built (increment 11, removal on deletion)),
and archiving it does too (As built (increment 11, removal on
archiving)).

**Empty and directory workspaces.** The Empty template makes a scratch
workspace under the jail directory, for a general-assistant conversation
or scratch work; Directory… admits an existing directory of the human's
that is not a git repository. Neither has git management. A directory
whose top holds a `.git` is refused with a pointer to a repository
template.

**As built (increment 10, workspaces).** File had two workspace items,
which the templates step folded into the chooser as its built-ins: New
scratch conversation became Empty, and New conversation in a directory…
became Directory…, which opens td-ui's finder over the human's folders
(Return enters one, Backspace goes up, Control+Return chooses the one
listed; a repository's top is marked `git`, a link `link`, hidden names
left out). The window admits the directory when it is chosen, before the
conversation exists, and says a refusal by name (§8, Admission). A
repository's top, a work tree's or a bare one, and anything inside a git
directory are refused too; a work tree's subdirectory is admitted, since
its `.git` is out of the jail's reach. The shared directories,
`[[shared]]` in configuration and `~/Downloads` read-only by default,
are admitted once at the window's start, each refused one named and left
out. Without the launch's td-jail and td-txt no workspace is made (As
built (templates)). A conversation's `meta` records its workspace,
`{"kind": "scratch"}` or `{"kind": "directory", "path": …}`, fixed at
creation; its jail directory is `$XDG_STATE_HOME/td-agent/jail/<id>/`,
holding the instances' `home/`, a scratch workspace's `scratch/`, and
`specs/`, which its process clears whenever it starts. A directory
workspace is the human's and is never removed. Deleting the conversation
renames its jail directory out of the way and removes it on a thread of
its own, with the walk Archiving and deleting describes; the window's
start does the same for one left without its conversation or cut short.
The walk goes through directory descriptors opened without following a
link, so a process still running in the tree cannot turn it out of the
tree, and holds at most 256 open, moving a deeper directory up within
the tree. The list's Workspace column names the workspace; the deletion
question says whether a scratch workspace goes with the conversation;
and the diagnostics export leaves `jail/` out, since it holds the
human's work, and writes its archive, which holds every conversation's
log, to the home directory rather than a `~/Downloads` that a workspace
reaches. The model's tools in a workspace are §12's (As built (increment
10, the tools)), each change and command decided by the human (§11).

**Archiving and deleting.** The human archives or deletes a conversation
from its row in the list (§4); no model can. Archiving keeps the
conversation's log and hides it from the list until unarchived; it
stops the conversation's processes, its background ones included
(§12), and its schedules (§3). Deleting removes the conversation for
good (§4, As built (deleting a conversation)). A scratch workspace goes
with a deleted conversation and stays with an archived one; a directory
workspace is the human's and is never removed.

A repository workspace goes with either, since its repositories keep
the store from pruning (§9). Archiving or deleting its conversation
stops the workspace's instances, then asks a maintenance instance
whether each worktree is clean and each branch's tip has been pushed.
That answer is jail-controlled, so a workspace that reports anything
uncommitted, untracked or unpushed, or whose answer cannot be read, is
removed only on the human's confirmation listing what would be lost;
declined, the conversation is neither archived nor deleted. td-agent
then removes the workspace tree `~/td-agent/<name>/` with its
worktrees, the workspace repository, the publish repository and the
workspace's jail HOME with a walk of its own that never follows a
symbolic link and restores the owner's permissions on a directory the
jail left unreadable before descending; it never runs `git worktree
remove`, which would run git over jail-written content. The store is
left alone. An unarchived conversation whose repository workspace was
removed keeps its log; its file, shell and git tools then refuse,
saying the workspace went with the archive.

**As built (increment 11, removal on deletion).** Deleting a repository
workspace's conversation removes the workspace with it; archiving one
does too, as the next paragraph says. The deletion question says
what goes, that each worktree is asked first, and that files git
ignores, such as build output, are not asked about. Once it is
confirmed, the window stops the conversation's processes, as a
deletion does, and holds the conversation while its worktrees are
asked: the list says `deleting`, it does not open, previous and next
pass over it, and the post delivers nothing to it. On a thread, each
worktree of a repository `meta` records prepared is surveyed in a
maintenance instance the window starts, its conversation having none
left (`survey`, below), within ten minutes for the whole survey, a
worktree not reached by then said not asked: how many of its files are
changed or untracked, and how many commits reachable from any ref of
its repository (every worktree's `HEAD`, branches, tags, the stash) are
in none of the commits its repository's worktrees started at, which the
conversation recorded with its project instructions. Until a push
exists (increment 14) those commits are all a worktree's work can be
compared against. A worktree never prepared was bound by no instance
but maintenance, so is not asked; one whose checkout finished but was
never recorded prepared, and anything of the human's in the workspace
tree beside the checkouts, go unasked. When every worktree reports
nothing, the conversation is deleted and the workspace removed at once;
when any reports something, or cannot be asked (no jail, no recorded
commit, a failed instance, the time spent), a loss card lists each such
worktree by checkout and branch, a line each cut to 1 KiB, with what it
reported (its repository's commits said once) or why it could not be
asked, says that the answers come from git inside the workspace, which
the model could have changed, and asks; it is asked until answered, set
aside and asked again as a tool's card is, and said once when it cannot
be shown. `Delete anyway` deletes both; `Cancel` keeps both, the
human's messages it had not taken held for it again (in memory, as an
archive holds them, so lost if the window quits meanwhile). Before the
conversation is deleted, the workspace tree and the data directory's
`ws/<name>/`, which holds its repositories, are each renamed beside
itself to `.deleting-<name>` (a random suffix when an earlier removal
left that), so a crash past that point leaves nothing the next start's
sweep does not take; renamed back if the deletion fails, they are
otherwise removed on a thread by the walk that follows no link. A
crash after the renames and before the deletion leaves the
conversation with its workspace swept, its repositories still recorded
prepared, so its tools fail on the missing paths until it is deleted
again, which the human had chosen; one whose renaming back fails is
swept too, which is said. A
directory not named for the workspace is left and said. At its start
the window sweeps the workspace root and `ws/` of directories with
names a removal gives (`.deleting-`, a workspace's name, ending in
eight hex digits, and perhaps a suffix), since the root is the human's
to name; a root since
renamed is not swept. A conversation whose record cannot be read is
deleted as one with no workspace, any it had left and said. The
conversation's jail directory, with the instances' home, goes with the
conversation as before; the store stays. Archiving a conversation being
deleted is refused.

**As built (increment 11, removal on archiving).** Archiving a
repository workspace's conversation removes its workspace as deleting
it does (above): the same survey, the list saying `archiving` while it
runs and the conversation neither opening nor taking a message, and,
when anything would be lost or could not be asked, the same loss card
headed `Archive and lose work`, its action `Archive anyway`. Archive
acts at once as it did, with no question first, so a clean workspace
goes without one; the loss card is the question for one that is not.
Kept, the conversation and its workspace stay as they were. Archived,
its `meta` gains `removed`, which only the window writes, under the
conversation's lock while no process of it runs, and which unarchiving
never clears; a conversation's own process keeps it as it found it.
The directories are renamed away before `meta` is written, put back if
that fails, and removed after, as a deletion's are. Unarchived, a
conversation whose workspace went asks the window for no store, lets
go an answer to one it asked for before the archive (the window told
it is done with, nothing laid out), and refuses every workspace tool
before any card with
`the workspace went with this conversation's archive`; its environment
says the worktrees were removed when the person archived it, so its
prefix changes once, logged as any change is; its workspace card says
each worktree was removed with the archive. Deleting it later asks
nothing, there being nothing left to ask, and archiving it again
removes nothing. A conversation whose record cannot be read is
archived as one with no workspace, any it had left and said. A crash
after the renames and before `meta` is written leaves the conversation
unarchived and not marked, its workspace swept, so its tools fail on
the missing paths until it is archived or deleted again, as with a
deletion. The instances' home in the conversation's jail directory
stays with an archived conversation and goes with its deletion, as
before.

**As built (archiving).** `meta` holds `archived`, absent and false in
one written before. Only the window writes it, under the conversation's
lock, once the conversation's processes, open, in the background or
retiring, are killed and waited for and its cards withdrawn, as a
deletion's are, unless its `meta` cannot be read, as while it is still
being made, which is refused before anything stops; a turn under way
ends without a question, since archiving loses nothing and is undone by
Unarchive. A lock not taken within two seconds leaves the conversation
unarchived, which is said, and opens it again if it was open; one that
was in the background starts again only when a message or the human
opens it. A failure after the new `meta` is in place, syncing its
directory, is judged by reading the mark back: stored, the conversation
is archived and the trouble said. The human's messages it had not taken
wait for its next process. What other conversations queued for it stays
in the outbox, which hands nothing to an archived conversation and so
starts no process for it, and the window refuses a new `send_message` to
it saying it is archived; `conversations` names its state `archived`.
Unarchiving clears the mark; what waited in the outbox is then handed to
it, which starts its process in the background as any message does, and
that process takes the human's messages parked with it; with nothing
waiting, nothing starts until the human opens it. A conversation's own
process keeps the mark as it found it when it rewrites `meta`. A scratch
workspace stays with it; a repository workspace goes with it (As built
(increment 11, removal on archiving)), and there are no schedules yet
to stop.

## 8. The workspace jail

A workspace's jail is a policy, started as td-jail instances. Each
conversation has one long-lived instance serving its file tools; it runs
the tool host alone, which starts no process, and is replaced when the
ready worktrees change. Each `shell`, `grep`, `sed`, snapshot and
maintenance call is an instance of its own. td-jail tears an instance
down with every process in it when its entry exits or its stage 1 dies,
so a timeout or an interruption, which kills the instance, also ends
every descendant, and nothing a foreground command started can go on
changing the workspace after its call has returned; only a background
process (§12) outlives its call, and only until it is killed. td-agent
needs no process-group signalling of its own to get this.

The instance must also die with the conversation process that launched
it, abruptly included. Today stage 1 is bound by its death signal to
td-jail's outer process, and nothing binds that process to its launcher,
so a conversation process killed with `SIGKILL` could leave its instances
running. The `workspace` kind therefore binds its outer process to its
launcher, with a parent death signal armed and then checked against the
expected parent, as stage 1's is, and a lifetime pipe from the
conversation process whose closing ends the instance. The death signal
follows the parent thread, not the process, so the pipe is the bound
that holds whichever thread starts td-jail; the jail tests kill a
conversation process and the window process with `SIGKILL` and find no
instance left.

Every instance has:

- **Worktrees:** each ready worktree read-write at its own path, each its
  own mount.
- **Git metadata:** the mount chain below.
- **Shared directories:** the configured host directories (`shared`,
  default `~/Downloads`), read-only by default at their real paths, so the
  human can hand the agent files; the agent hands files back in the
  workspace tree, which the human reads. A directory configured
  read-write joins every workspace that shares it, so it is a channel
  between workspaces and is the human's explicit choice. A template may
  name its own shared directories in place of the configured list
  (§7, §15), which is the human's choice as the list is.
- **System:** the system's executable and library trees and a selective
  `/etc`, read-only, and td-agent's own tool host and td-txt, read-only.
  No cgroupfs, no `/sys`.
- **Home:** the workspace's private `HOME` and a private `/tmp`, under the
  jail directory. The caller's home is absent except through the
  worktrees and shared directories, so `~/.ssh`, keyrings and the agent's
  own state are invisible, not merely unwritable.
- **Network:** a network namespace with loopback alone. The tool host
  listens there for the proxy of §10 when the workspace's policy is not
  `off`; that listener is inside the instance's own namespace, and
  nothing else leaves.
- **Unix sockets:** a seccomp policy that refuses `socket(AF_UNIX, ...)`
  and admits `socketpair(AF_UNIX, ...)` for `SOCK_STREAM` and
  `SOCK_SEQPACKET` alone, the type compared with `SOCK_NONBLOCK` and
  `SOCK_CLOEXEC` masked off; Rust's `Command::spawn` makes a
  sequenced-packet pair for every child, and a connected one cannot
  address another socket. A network
  namespace does not separate pathname Unix sockets, so without that
  refusal a socket a host service publishes in a worktree or shared
  directory would be a way out; and a datagram pair could still address
  one with `sendto`, which a connected stream pair cannot. td-jail's
  current filter admits `AF_UNIX`, so this is a variant of it for the
  `workspace` kind. The cost is that a program needing its own Unix
  listener fails in the jail.
- **Limits:** none in the first increments; the instance is placed
  under its conversation, on a host as a descendant of its conversation
  process, where later limits attach (below).
- **Isolation:** private PID, UTS, and, where the kernel provides it, IPC
  namespaces (td today does not, APPLICATIONS.md §0); no new privileges.
- **Inheritance:** no descriptor but the instance's pipe, no runtime
  directory, and an environment scrubbed to `PATH`, `HOME`, `TMPDIR`,
  `LANG`, `TERM=dumb` and, when the policy is not `off`, the proxy
  variables of §10. `PATH` begins with the host's own `PATH`
  directories as they resolve, those inside a system tree the instance
  binds, each once and at most 16 (`jail::system_path`, td-jail's
  `path` spec key); the rest is td-jail's fixed list. A store-based
  host's tools, in a profile under the home that no instance binds but
  resolving into its store, are so found by name: the model's `git
  commit` and `cargo build`. They are resolved at each launch, so a
  profile changed or collected since is followed.

**The git mount chain.** Commits happen inside the jail, so a workspace
repository's objects, refs, index and logs are writable there, and
everything there is jail-controlled. Two rules keep that from reaching
outside:

1. No git outside a jail ever opens the workspace repository (§9). Git
   follows symbolic links the jail plants in `logs/`, `refs/`,
   `FETCH_HEAD` and the message files, and would append to or truncate
   whatever host file they name, so every git operation on it runs in a
   jail instance, where such a link reaches only what the instance can.
2. The files that tell git what to execute or where to look are
   read-only, so the git worker's maintenance git and the human's
   read-only git in a worktree act only on td-agent's configuration. A
   model's own git in a `shell` call also reads the workspace HOME's
   `~/.gitconfig`, which the model can write; that reaches only the jail.

Bind mounts protect a path only while no directory above it can be
renamed: the kernel refuses to rename a mount point but moves a mount with
a renamed ancestor. So every directory on the path from a mount top to a
protected entry is itself a mount, and td-jail's filter refuses the whole
mount surface, old and new API. The chain:

- the workspace repository `<repo>.git`, read-write, as a mount, with
  `config`, `config.worktree`, `commondir`, `shallow`, `hooks/`, `info/`,
  `remotes/`, `branches/` and `worktrees/` bound read-only on top of it;
- its `objects/`, read-write, as a mount, with `objects/info/` (which holds
  `alternates`) read-only on top of it, and the store's `objects/`
  read-only at its own path, for the alternates to reach;
- each `worktrees/<id>/`, read-write, as a mount on the read-only
  `worktrees/`, with its `commondir`, `gitdir` and `config.worktree` bound
  read-only on top of it;
- each worktree, read-write, as a mount, with its `.git` file bound
  read-only on top of it.

td-agent creates every protected entry itself, outside any jail, before
any instance binds it: the repository's files, `commondir` naming the
repository itself (`.`), since git refuses an empty one, and the others
empty where git expects none, and for each new worktree its directory,
its `.git` file and its `worktrees/<id>/` with `gitdir`, `commondir`
and an empty `config.worktree` (the protected entries) and `HEAD`
(written once, writable afterwards), each with a fresh `mkdir` or an
exclusive, no-follow create, so nothing the jail planted is reused. A
protected file is its owner's to write (td-jail refuses one that is
not, so that its read-only probe fails for the mount alone). These are
plain files td-agent writes, not git run on the repository. `worktrees/` is
read-only in every instance, maintenance included, so no jailed process
creates, moves or prunes a linked worktree; a maintenance instance then
only checks the new worktree out.

The repository's `config` is td-agent's, read-only, and sets what a jailed
git needs and nothing it could misuse: the human's `user.name` and
`user.email`, `core.sparseCheckout` and `core.sparseCheckoutCone`,
`branch.autoSetupMerge=false` (so `git switch -c` does not try to write
it), `submodule.recurse=false`, `core.fsmonitor=false`, `core.hooksPath`
naming an empty read-only directory, `gc.auto=0`, `maintenance.auto=false`,
`diff.ignoreSubmodules=all` and `status.submoduleSummary=false` (so the
human's read-only `git status` in a worktree never starts a git in a
jail-made submodule, which would read that one's configuration),
`rerere.enabled=false` (rerere otherwise switches itself on when a
jail-made `rr-cache/` exists), and, for the git worker's own gc,
`gc.writeCommitGraph=false`, `repack.updateServerInfo=false` and
`gc.worktreePruneExpire=never`, since `objects/info/` and `info/` are
read-only.
There is no `extensions.worktreeConfig`, so no `config.worktree` is ever
read. What therefore fails in the jail, by design: `git config`, `git
remote`, a `git sparse-checkout` that names or changes the sparse mode, adding
worktrees and submodules, and gc; the git worker does gc in a
maintenance instance.

**What the jail does not protect.** Anything outside the jail that later
acts on a worktree or shared directory is a persistence channel, and much
of it acts with no decision by the human:

- language servers that build or expand on open or save (rust-analyzer
  runs `build.rs` and proc macros);
- direnv and `.envrc`;
- file watchers and sync clients;
- the human's own git in a worktree: read-only commands act on
  td-agent's configuration, but a writing command (commit, checkout,
  fetch) appends to reflogs and writes message files through whatever
  links the jail planted there. The human reviews through td-agent's
  diff view or the publish repository, not by committing in a worktree;
- git repositories other than td-agent's own: a nested `.git` the jail
  creates inside a worktree, which the human's git or prompt may read;
- the human's global git configuration acting on jail-written content: a
  `.gitattributes` naming a filter the human configured, such as LFS;
- and the ordinary case: a build script, Makefile, `.cargo/config.toml`
  or test the human runs.

A coding agent writes code for the human's machine to run, so this is
inherent to the product. td-agent shows the diff of each step's file
changes, and its workspace card recommends against opening an untrusted
workspace in tools that execute on open. The design claims no more than
this.

**Admission.** Every worktree, shared directory, the workspace root, any
extra directory granted later, and every mount of the git chain passes the
source checks of td-jail's filesystem grants (APPLICATIONS.md §C):
canonicalization with links refused, type, device and inode checked before
and after the bind, comparison by mount identity, and refusal of overlap
with td's reserved trees and state. The `workspace` kind departs from §C in
three ways its amendment names:

- its targets are the sources' real paths, where a §C home grant is
  mounted at `/home/td/<rel>`;
- it nests the read-only and read-write mounts of the git chain, where §C
  refuses overlapping grants; no other overlap is admitted;
- worktrees are not mounted `noexec`, because `shell` runs code built
  there (`cargo test`, `./run.sh`); they stay `nosuid,nodev`.

td-agent then refuses, for every one of those sources, on top of §C:

- one that is, contains, or lies inside td-agent's configuration
  directory, its state directory, its jail directory, the publish
  repositories, or the caller's runtime directory (`/run/user/UID` when
  `XDG_RUNTIME_DIR` is unset), other than the parts of the data
  directory the git chain binds and a conversation's own instance home
  and scratch workspace, which td-agent makes in its jail directory;
- one that is, contains, or lies inside a credential location: `~/.ssh`,
  `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.config/gh`,
  `~/.netrc`, `~/.git-credentials`, `~/.config/git/credentials`,
  `~/.cargo/credentials` and `~/.cargo/credentials.toml`, `~/.npmrc`,
  `~/.pypirc`, `~/.docker`, `~/.kube`, `~/.password-store`,
  `~/.local/share/keyrings`, td's own credential and secret stores, or a
  browser profile; a configuration home's (`~/.config` and
  `$XDG_CONFIG_HOME`) `autostart`, `systemd`, `environment.d` and `git`,
  which the session or git runs from; a data home's `applications`;
  and sandboxed applications' `~/.var/app` and `~/snap`;
- a shared or extra directory that contains a worktree or any part of the
  git chain, which would make a protected entry's ancestors renamable;
- a shared or extra directory, or a directory workspace, that is,
  contains, or lies inside `workspace_root`, so that no workspace reaches
  another's tree; and td-agent creates no worktree under a path a live
  grant covers;
- any of the above reached through a bind mount: td-agent compares the
  grant, and every mount at or below it, with each refused place by
  device and the path within it, as `/proc/self/mountinfo` gives them,
  and a place not made yet by where it will be, its deepest existing
  ancestor resolved;
- a directory workspace, or a shared or extra directory the model may
  write, that is or contains a program td-agent runs or binds (td-jail,
  td-txt, td-agent itself, each by the path it resolves to, which is
  the path it is run by), or that is or contains a directory on
  `PATH`, `~/bin`, `~/.local/bin` or `~/.cargo/bin` (one inside them
  puts nothing there): the model could replace its own jail, or the
  `cargo` that builds it. td-jail refuses the same by mount identity
  for its own executable and the spec's programs. The programs run are
  the ones `./install-apps` placed in `~/.local/lib/td`, which no
  workspace may write; the checkout they were built from is never
  needed by them and is granted, or not, like any other directory.

**Limits.** The first increments set none, and say so in the status row.
td-jail's application launch sets the package's `RLIMIT_DATA` on every
instance even with `cgroup-root=none`; the `workspace` kind sets no data
limit of its own, and keeps, never raises, any limit it inherits, as
td-builder's `run-capped` keeps a tighter ambient one. The limits are
designed so that adding them later changes no structure. On a host every
process a conversation causes, its jail instances and everything in
them, descends from its conversation process (§2), and td-jail's host
launch, configured with `cgroup-root=none` as APPLICATIONS.md §X.1
requires today, leaves each instance in its launcher's cgroup. The later
design is then additive:

- **A node per conversation.** On a host with a delegated cgroup v2
  subtree, the window process moves itself into a leaf of its own,
  never the delegated root, and creates one node per conversation; each
  conversation process joins its node by writing to the node's
  `cgroup.procs` before starting anything. Every instance inherits the
  node, whose `memory.max`, `memory.high`, `cpu.max` and `pids.max` then
  bound the whole conversation. Joining a cgroup is a file write, so
  td-agent still needs no `unsafe`. A node per workspace above its
  conversations, and a total above those, hold no processes and are the
  same mechanism one level up.
- **A leaf per instance.** cgroup v2 lets a node either hold processes
  or enable controllers for children, not both. So splitting a
  conversation's node into a leaf per instance, with `memory.oom.group`
  set so an out-of-memory kill takes one instance rather than the
  conversation, puts the conversation process in `<conversation>/self`
  and each instance in a sibling leaf. That needs td-jail to place an
  instance under a caller-named node; today it creates exactly
  `<owner>/<instance>` and refuses deeper membership, so that is a
  td-jail amendment of its own.
- **Without a delegated cgroup**, a per-process `RLIMIT_DATA`, which
  td-builder's `run-capped` uses for test binaries, is inherited across
  fork and exec. It bounds each process, not their sum, so it caps a
  runaway allocation but not a conversation's total; it has no CPU
  counterpart that throttles (`RLIMIT_CPU` kills). Setting it needs
  `unsafe` that td-agent does not carry, so it would be td-jail's to
  apply to an instance's entry.
- **Inside td**, instances are started by a root request listener, not
  as descendants of the conversation process ("Inside td" below), so
  that listener must place them under the conversation's node; the
  packaging increment designs that.

To keep this open, the rule is placement: everything a conversation
causes is placed under that conversation's node, and on a host the means
is ancestry. So on a host no increment may run a conversation's work in
the window process or in any process not descended from that
conversation's own; and nowhere may one conversation process serve two
conversations, or an increment depend on td-jail moving an instance out
of its launcher's cgroup on a host. The
git worker's imports and pushes are children of the window process,
which would take a limit of their own (§9).

**Mechanism.** td has one confinement implementation, td-jail
(APPLICATIONS.md §C), and td-agent does not grow a second one. The jail is
a new td-jail launch kind, `workspace`, whose policy is the list above.
It grants no Wayland, bus, audio, fetch or tty, binds the admitted sources
and the git chain, and runs the tool host as its entry. That kind is
specified and landed in td-jail, with its APPLICATIONS.md and UNSAFE.md
amendments, in the increment that first exposes a file or shell tool (§18).

**On a development host.** td-jail's `--host` launch (APPLICATIONS.md §X.1)
already runs for an unprivileged caller inside a user namespace it
creates; the capability its stage 1 raises is the new namespace's. The
`workspace` kind differs from §X.1's host application launch in ways its
§X amendment must name:

- §X.1 launches a materialized package, and its applications need the
  caller's Wayland socket and a local td-busd socket. A `workspace`
  instance runs the installed tool host and td-txt, which the same
  `./install-apps` that installs td-agent builds from the checkout, bound
  read-only into the instance, with no package, no Wayland and no bus.
  A td-agent window running when `./install-apps` replaces it starts
  later conversations from the new file, which work. The conversations
  it already runs, and its own archive and delete survey, find their
  file gone: their workspace preparation and tracking are refused, and
  so are their tools unless they had already found their programs,
  which then run the newly installed td-jail and tool host;
- §X refuses to borrow the host's own `/etc` or system trees, but a coding
  agent's tools on a host are the host's: its compiler, git and shell. The
  `workspace` kind binds them read-only, for that kind alone, as an
  availability divergence;
- `./install-apps` builds td-jail and td-txt from the checkout and
  installs them beside td-agent, td-net's launch names them to it, and
  td-agent writes each instance's spec for td-jail under
  `$XDG_STATE_HOME/td-agent/jail/` (As built, below).

If that amendment is not made, tool execution is refused by name on a
host. There is no silent unconfined fallback.

**As built (increment 10, the td-jail kind).** The `workspace` kind is
APPLICATIONS.md §C's `td-jail --workspace LAUNCHER-PID SPEC [ARG...]`,
with §X.8
naming its host divergences, and it settles four things this section
left open. The spec is td-agent's to write, per instance, not
the launch's: an ordered keyfile of the entry and td-txt, the home, the
worktrees and the shared directories, read-only or read-write, kept
outside every directory it grants, since the instance runs as the
caller and could otherwise rewrite the next instance's spec. The
lifetime pipe is the channel itself: td-jail requires the instance's
standard input and output to be one stream socket, the tool host's
framed protocol runs over it, and the tool host ends when it closes;
for an entry that is not reading, the outer process's parent-death
signal, checked against its parent as stage 1's is, ends the instance
with its launcher, so td-agent starts td-jail from the conversation
process's main thread. `/tmp` and `/var/tmp` are fresh, private and
executable, since worktrees are executable anyway and test suites run
scripts they write to `TMPDIR`; the home is `noexec`. The kind refuses
every overlap between its directories; the git chain's nesting is not
an overlap of directories but mounts td-jail derives from a repository's
fixed names (As built (increment 11, the chain), below). td-agent's own
refusals above (its state, credential locations, `workspace_root`) are
td-agent's, applied before it writes a spec; td-jail's are the reserved
trees, the caller's real home, overlap and links. The killed-launcher
tests named above run with td-agent's launch in the next commit; the
kind's own live test launches it on a socket, reads its plan back from
inside, and is ignored where unprivileged user namespaces are absent.

**As built (increment 11, the chain).** td-jail binds the chain above
from two spec keys (APPLICATIONS.md §C, The `workspace` kind): a
`checkout`, a worktree whose `.git` file is bound read-only over it,
and a `repository`, a workspace repository read-write with every
protected entry of the list above bound over it from fixed names, the
read-only `worktrees/` enumerated for each linked worktree's directory
and files. Nothing of the chain is named by path in the spec, so a
launcher cannot misplace a link, and an entry that is missing, a link,
or a file with a second name refuses the launch, since an absent entry
cannot be protected and td-agent creates every one first. Beside a
repository, a plain `worktree` holding a `.git` is refused, since its
`.git` could be pointed at a repository the jail made; a git worktree
is a `checkout`. The plan's paths take at most 384 KiB of stage 2's
argv, counting each word's overhead, which assumes a stack limit of at
least 2 MiB. Stage 2 derives each repository's required links itself,
from the fixed names and its read-only `worktrees/`, with their modes
(only `objects/` and each linked worktree's directory writable), and
requires the plan's to be exactly those; a link inside a worktree may
only be its `.git`. A protected file must be its owner's to write. It
reads each link back as the one mount at its path, mounted on the link
or top it lies in, so that it is the mount seen there; of its kind and
mode, a writable one written and a read-only one refusing a write with
`EROFS`; and refuses any other mount below a repository. The store's
`objects/` reaches an instance as a `read` directory at its own path.
td-jail checks an entry's kind and names, not its content, so td-agent
makes every `.git`, `gitdir` and `commondir` afresh, never one that
was ever jail-writable (As built (increment 11, the layout), below). The
kind's live test lays out a linked worktree as td-agent will and, from
inside, finds every protected file unwritable, unremovable and
immovable, every protected directory unwritable and immovable, the
writable links writable, the store read-only, and a jailed git
committing through the linked worktree and reading the repository
through its root. Mounting no chain, a writable one, or one lacking
`config` fails its readback; one out of order fails at its own mount,
and the stacking check is proved apart, on mountinfo with a hidden and
a doubled link.

**As built (increment 11, the layout).** `src/repo.rs` lays a
workspace repository out as the chain above protects it, as plain
files td-agent writes outside any jail: `create` makes the repository
over a store, `add_worktree` a linked worktree, and nothing calls them
yet; the window's preparation arrives with repository templates. Each
is built complete in a staging directory beside the repository, each
file an exclusive create written through to the disk, and renamed into
place whole: the repository at its path, a worktree's id directory
into `worktrees/`, which no jail can write, so no instance finds one
half made and a crash leaves its debris beside the repository rather
than as an id td-jail would refuse to launch over. A repository, a
checkout or an id that exists, or a link planted at one, is refused and
never written through; a refused call removes only what it made; the
paths written into git's files are the ones they resolve to, as
td-jail binds them; and a repository holds at most 32 worktrees, as
many as td-jail binds. td-jail reads `worktrees/` at both of its
stages, so a worktree added while an instance over its repository is
starting fails that launch: the caller adds them while none is. The
repository holds `HEAD`, the configuration above (the identity from
the git worker's copied global file, quoted; `core.hooksPath` its own
empty, read-only `hooks/`; no `extensions.worktreeConfig`),
`commondir` naming itself, an empty `config.worktree` and `shallow`,
`objects/info/alternates` naming the store's `objects/`, and its
directories. A worktree's id is letters, digits, `.`, `_` and `-`; its
`worktrees/<id>/` holds `gitdir` naming the checkout's `.git`,
`commondir` naming the repository, an empty `config.worktree`, `HEAD`
naming its branch, and `info/sparse-checkout` with the entry's paths
as cone patterns, as `git sparse-checkout set --cone` writes them (no
`sparse` is the whole tree, an empty list the top's files alone; a
path segment git would read as a glob or a comment is refused); the
checkout holds only its `.git`. A maintenance instance is td-jail's
`workspace` kind with td-agent as its entry, run as `td-agent maintain
TASK...` by `jail::maintain`, which blocks its thread, kills the
instance past its time, and reads the entry's one line of answer, `ok`
or `failed` and a reason, from the channel; its policy binds the
checkout, the repository with its chain and the store's `objects/`,
which `jail::Policy` now names (`checkouts`, `repositories` and
`objects`, the last no root of the tool host's). Its git is the
host's, by the path it resolves to, which the kind's system trees
bind, run with a cleared environment, `GIT_DIR` the linked worktree's
directory and `GIT_WORK_TREE` its checkout, no system or global
configuration, `HOME` naming no directory, and hooks, fsmonitor,
attributes and excludes files, submodules, every protocol and auto gc
off on its command line. Its first task, `checkout`, trusts
nothing in the id directory, which is the jail's to write once it is
in place: it refuses one holding an `index`, makes the branch at the
base with `update-ref` and an empty old value, sets `HEAD` to it, and
reads the branch's tree by its id with `read-tree --reset -u`, which
the sparse patterns select and which overwrites what a broken run left;
a checkout that fails after making the branch removes it while it is
still where it was made, so the task can be asked again. It is run only
before its repository is recorded prepared (§7, As built (increment
11, preparation)), when nothing but these tasks writes the repository,
which it cannot itself check: so it clears the locks a killed git left
(the worktree's `index.lock` and `HEAD.lock`, the branch's and
`packed-refs.lock`) and keeps a branch already there, which only an
earlier run of it, cut short, made, checking that out instead of the
base; that one worktree's branch is no other's is the workspace
record's to hold, which refuses a branch named twice for one remote.
Its second, `survey`, writes nothing: `git --no-optional-locks status
--porcelain=v1 -z --untracked-files=all` counts the changed and
untracked files, ignored ones aside (past its 64 KiB answer, "more
than could be counted"), writing no index, and an index lock a killed
tool left stops nothing; `rev-list --count --exclude=refs/remotes/*
--exclude=refs/td-agent/* --all --not BASE...` the commits any ref
but a remote-tracking one (upstream's, which `track` sets) or a step
snapshot's (td-agent's undo, §12) reaches that none of the bases do, which td-agent
passes from its own record, every base of the repository's worktrees;
it answers `changes N ahead M`, read back as the jail's word, nothing
more. A workspace
repository is never fetched into: its third, `track`, sets each base's
remote-tracking ref with `update-ref --no-deref` to the commit the git
worker resolved in the trusted store, whose objects the alternates
already reach, so its empty
`shallow` (which makes git call it shallow) matters to no command
td-agent runs; commit, log, counts and export do not depend on it. The
live test in `tests/jail.rs` checks a sparse worktree out in a
maintenance instance, refuses a second run, and in a shell instance
commits as the identity the configuration names, cannot write
`config`, widens the cone with `add` and narrows it with a plain `set`,
and cannot name the sparse mode, which leaves the patterns as they
were.

**As built (increment 10, the launch).** `./install-apps` installs
td-jail and td-txt from the checkout beside td-agent, and td-net's
launch names them in `TD_AGENT_JAIL` and `TD_AGENT_TXT`; without them
every tool is refused with that reason. `jail::launch` writes an
instance's spec, mode 0600, under a 0700 directory of the caller's,
creates the instance's 0700 home, and starts `td-jail --workspace PID
SPEC tool-host --txt /opt/workspace/bin/td-txt --root DIR...` with one
end of a stream
socketpair as its standard input and output and an empty environment;
the tool host's roots are the worktrees and the shared directories,
never the home. td-jail must be named `td-jail`, since its argv[0]
selects its kind. td-jail's own standard error is read into a bounded
tail of bytes, decoded whole, that any failure of the host's channel
reports with how td-jail ended. Dropping the client kills td-jail's
outer process, which takes stage 1 and the whole instance with it
through their parent-death signals, removes the spec, and waits, at
most five seconds, for the instance to let go of the channel. The
instance's stage 2 holds it, so the wait ends as the namespace's init
exits; the kernel kills that namespace's last processes as it does,
and a system call already under way in one of them may still complete.
The drop blocks its thread for that wait, and the first channel failure
for up to two seconds while td-jail finishes its account.
The caller names the spec directory; a conversation's are under
`$XDG_STATE_HOME/td-agent/jail/`, outside `/tmp`, which td-jail
reserves, and a specs directory and a home are named as they resolve.
Ignored live tests, which name
td-jail and td-txt as the launch does, run file, grep and shell calls in
an instance and read its confinement back from inside, and kill a
launching process with `SIGKILL` and find no process of its instance
left, the command it was running included.

**Unconfined workspaces.** The one way to run tools without a jail is a
choice the human makes when creating an Empty or Directory… workspace;
repository workspaces are always jailed. It is shown in the status row and
the list, every action in it goes to the human (§11), and it is still
given only the pipe and the scrubbed environment. Its commands can reach
the key file, the store and the publish repositories, the fetch and
egress sockets, the human's SSH agent, and the control socket, through
which they could answer their own cards; it is a decision to trust the
model with everything the human has, and its card says so.

**Inside td.** As a jailed application, td-agent cannot run td-jail
itself, and td-authd's `application-start` is configured by root, which
accepts no requests. Starting a workspace instance on td-agent's behalf
needs a new root request listener with an amendment to
`td-authd/DESIGN.md`, and the workspace root becomes a portal grant. The
packaging increment designs that (§18, §19).

## 9. Git

**The git worker** is the part of td-agent that runs git. It does so in
two places, and the line between them is the design. Outside a jail it
is the window process's, which holds the shared repositories; a
maintenance instance is started by the conversation process whose work
needs it, or for workspace maintenance no turn asks for by the
workspace's own conversation process (§2); the survey before a
deletion, whose conversation has no process left, by the window.

- **Outside any jail**, only on repositories no jail can write: cloning
  and fetching the store, importing into and pushing from the publish
  repository, and computing a push's evidence there. On a development host
  it runs the host's `git`; on td, the image's.
- **Inside a maintenance instance**, a `workspace` jail instance with no
  network that runs only the git worker's fixed commands, every git
  command that touches a workspace repository: checking out a worktree
  td-agent created (§8) and its sparse patterns, updating remote-tracking
  refs from the store (bound read-only), gc, the ahead-and-behind counts
  `conversations` shows, the cleanliness check before archiving or
  deleting, and exporting a commit for a push. Its git reads no
  configuration the model can write: `HOME` names no directory,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`,
  hooks and fsmonitor are forced off on the command line as in the
  repository's configuration, and `GIT_DIR` and `GIT_WORK_TREE` are set,
  so a nested `.git` the model made cannot steer discovery. Everything
  it reports is still jail-controlled data, since the refs and objects
  it reads are the model's, and is shown as such.

Every outside invocation is fixed in shape:

- a cleared environment, to which only `PATH`, `LANG`, `HOME`,
  `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` and the human's SSH agent
  socket (which the copied credential helpers need) and the variables
  below are added, so no inherited `GIT_SSH_COMMAND`, `GIT_EXEC_PATH`,
  `GIT_CONFIG_PARAMETERS` or `GIT_ALTERNATE_OBJECT_DIRECTORIES` applies;
  `GIT_TERMINAL_PROMPT=0` and SSH's `BatchMode=yes`, so a launch from a
  terminal never waits on it;
- `GIT_DIR` set explicitly and no worktree;
- `GIT_CONFIG_NOSYSTEM=1`, and `GIT_CONFIG_GLOBAL` naming a file td-agent
  writes, holding only the identity and credential-helper lines copied
  from the human's own global configuration, and nothing else of their
  configuration applies;
- `-c core.hooksPath=` an empty directory td-agent owns,
  `-c core.fsmonitor=false`, `-c protocol.allow=never` with
  `protocol.https.allow` and `protocol.ssh.allow` set to `always`,
  `-c submodule.recurse=false`, `-c fetch.recurseSubmodules=false`,
  `-c push.recurseSubmodules=no`, `-c http.followRedirects=false`,
  `-c core.attributesFile=/dev/null`, and `GIT_NO_REPLACE_OBJECTS=1`;
- the remote URL taken from td-agent's record of the admitted remote,
  never from a file a jail can write; ref names checked and placed after
  `--end-of-options`.

**Commits** are made in the jail, by the model's own `git commit` through
`shell`, under the read-only configuration of §8.

**Pushing** is the `git_push {worktree, remote_branch?, force?}` tool, and
an approval of it is bound to immutable values:

1. A maintenance instance resolves the worktree's branch to a commit id
   and exports the commits from the base to it as a pack over its pipe.
2. The git worker imports the pack into the publish repository with
   `index-pack --strict --max-input-size`, the pipe's frames bounded too,
   so the objects are checked and nothing but objects crosses;
   jail-written refs, reflogs and replace refs never do. The input bound
   limits the compressed pack only: until §8's limits land, a hostile
   pack can make the import use memory in proportion to what it inflates
   to, and the import is the first of the git worker's children to take a
   memory limit of its own when they do.
3. It computes the evidence there, outside the jail, against the
   merge-base with the remote's current tip as well as the base: the
   commit subjects, the paths changed with their line counts, every added
   binary file, and a deterministic scan for credential shapes (private
   keys, tokens of the common forges and clouds) over every commit's own
   diff and full message, not only the overall difference, so a secret
   added in one commit and removed in a later one is still found. The
   diff runs with `--text --no-textconv --no-ext-diff` and attributes
   taken from an empty tree, so a commit's own `.gitattributes` cannot
   hide content from the scan; an added binary file counts as a scan
   match.
4. The approval (§11) names the commit id, the admitted remote, and the
   destination `refs/heads/<branch>`, the branch validated and never
   starting with `+`, `:` or `-`; a force push names the remote's current
   id as the expected old value.
5. The git worker pushes exactly `<id>:refs/heads/<branch>` from the
   publish repository with the human's credentials, with
   `--force-with-lease` carrying that expected id when forced, and returns
   the result, including the remote's message, to the model.

A push to a protected branch (`main`, `master`, and any configured in
`protected_branches`), a force push, and any push whose scan matched
always go to the human. Another push is the classifier's in `auto` mode
and the human's in `ask` mode. The tool pushes only `refs/heads/`, so
branch deletion and tags are not expressible through it.

**Fetching** is `git_fetch {worktree}`, a repository workspace's tool:
an immediate store fetch and remote-tracking update for that workspace,
which moves data in from an admitted remote and needs no approval. It is
the only fetch a model asks for; the background fetch of §7 does the
rest.

**Object lifetime.** Workspace and publish repositories borrow the
store's objects, and store gc cannot see their refs, so the store never
prunes while any workspace exists: its gc repacks with unreachable
objects kept. Snapshots (§12) are kept reachable by
`refs/td-agent/snapshots/<worktree>` in the workspace repository, which
is the conversation's own, and
workspace gc, in a maintenance instance, keeps what those refs reach.

A repository workspace's history is therefore the agent's to make and the
human's, or the classifier's, to publish, as in td's own workflow, where a
pushed branch is the submission.

**As built (increment 11, the store).** The first step of increment 11
is the git worker's outside half, `src/git.rs`, which nothing calls
yet: the window's store fetch, base reads and admission arrive with
repository templates. A remote parses as `https://host[:port]/path`,
`ssh://[user@]host[:port]/path` or the scp-like `[user@]host:path`;
every other transport, a local path, credentials in an https URL, a
query, an empty, `.`, `..` or option-like path segment, and a user or
host that could be read as an option are refused by name, the host
lower-cased and a default port dropped; an IPv6 literal is refused. An
scp-like path relative to the login's home keeps that form, since an
`ssh://` URL cannot spell it, and is a different remote from the
absolute one. A `remotes` entry is a remote, admitting that one (its
transport, user, host, port and path, a final `.git` aside), or
`host[/prefix]`, admitting every https or ssh remote on that host, on
any port and as any ssh user, whose path starts with the prefix's
whole segments, a final `.git` aside, the path written relative to the
ssh login's home or absolute alike. A branch or base is checked as
`git check-ref-format --branch` would and may not start with `-`, `+`,
`:` or `/`. The store for a remote is
`store/<host>-<path>-<digest>.git`, its host and path made one file
name and a 64-bit FNV digest of everything naming the repository
fetched, a final `.git` included, appended. The store records that text
in `td-agent-remote` before anything else, written whole, and is refused
to any other remote, so a collided digest fails rather than shares; it
is then made with `init --bare`, again whenever it lacks its `HEAD`, and
holds no remote configuration. A fetch names
td-agent's record of the URL after `--end-of-options` with
`+refs/heads/*:refs/heads/*`, `--prune`, `--no-tags`,
`--no-write-fetch-head` and `--no-auto-maintenance`. The fixed shape
adds `gc.auto=0` and `maintenance.auto=false`, since workspaces borrow
the store's objects; `credential.interactive=false`; and the stall
bounds, https below a kilobyte a second for a minute and ssh in
BatchMode that cannot connect in 30 seconds or whose server stops
answering for a minute. A fetch is killed after an hour and any other
git after two minutes; git's standard error is read to its end, so a
chatty helper never ends a good fetch on a closed pipe, and once git
has exited its answer is what its pipes gave within a second, though a
helper it left behind still holds them. Every outside
git, the configuration copy and `init` included, runs in td-agent's
worker directory with discovery stopped there. `HOME` is kept, so the
human's `~/.ssh/config` and `~/.netrc` apply to the fetch as they do to
the human's own. The global file is rewritten at the worker's start, and
put in place whole, through git's own `config --file` writer from the
human's `user.name`, `user.email` and `credential.helper`,
`credential.username` and `credential.useHttpPath`, each also per URL,
read with `GIT_CONFIG_NOSYSTEM` and includes followed (`XDG_CONFIG_HOME`
is kept to find it). A base resolves to its commit with `rev-parse
--verify`, and a file is read from that commit by listing its literal
path with `ls-tree` and reading the listed blob: a tree, a link, a
submodule or a missing path is none, and one past 64 KiB refused. The
project instructions are `AGENTS.md`, else `CLAUDE.md`, at the commit's
top.

## 10. Network policy

A workspace has one of three network policies, shown in the status row:

- `off`: no proxy, no proxy variables; nothing leaves the jail.
- `allowlist` (the default): the proxy admits destinations, each a host
  and port (443 when unspecified), on the workspace's allowlist, which
  starts from `network_allowlist` in configuration. The shipped default
  names only download hosts: `static.crates.io`, `index.crates.io`,
  `static.rust-lang.org`, `pypi.org` (whose uploads go to
  `upload.pypi.org`), `files.pythonhosted.org`, `codeload.github.com`,
  `objects.githubusercontent.com`, `release-assets.githubusercontent.com`,
  `proxy.golang.org` and `sum.golang.org`. Hosts that also accept uploads
  with a token (`github.com`, `crates.io`, `registry.npmjs.org`) are not in
  it, because an injected token would make any of them a publication
  channel outside `git_push`; so `npm install`, for one, fails by default
  until the human admits its registry. Another
  destination is a crossing (§11): the connection waits while it is
  decided, and an "always" answer adds it to this workspace's allowlist.
- `open`: any destination the relay will reach. Only the human sets it,
  on a card or in a template (§7).

An allowlisted host is a standing decision by the human, and one that
accepts uploads remains a possible channel out; the card that adds one
says whether it does.

**Mechanism.** Inside an instance whose policy is not `off`, the tool
host listens on `127.0.0.1` in the instance's own network namespace and
speaks HTTP `CONNECT` and plain-HTTP proxying there. The environment
carries `https_proxy`, `http_proxy` and `all_proxy` naming it, in both
lower and upper case, and `no_proxy=localhost,127.0.0.1,::1`; a `CONNECT`
to a loopback name is refused by the tool host itself. Each accepted
connection is a channel of the instance's existing pipe to td-agent, which
checks the destination against the policy, decides or asks per §11, and
opens it through the egress relay. The jail therefore needs no Unix socket
and no route. Only the listed host and port are reached, so SSH or another
protocol tunnelled through `CONNECT` reaches only a host the human
admitted on a port the human admitted; a program that ignores the proxy
variables reaches nothing. Names are resolved by the relay, not in the
jail.

**The egress relay** is a new td-net applet beside fetchd, served by the
installed launch on a development host and as a unit on td. It takes a host
and port from td-agent over its socket, resolves, and refuses loopback,
link-local, unspecified, broadcast, multicast, RFC 1918, unique-local and
carrier-grade NAT addresses, and the machine's own addresses, read from
`/proc/net` without any new `unsafe`, after decoding IPv4-mapped,
IPv4-compatible, NAT64 (both the well-known and the local-use prefix),
6to4 and Teredo forms to the IPv4 address they carry; fetchd today refuses only
the first five of those (APPLICATIONS.md §W.8), so this predicate is new
and tested on its own. It then connects and splices bytes, under
per-connection deadlines, and holds no other policy; the policy is
td-agent's. With
fetchd it is where td-agent's traffic leaves, the git worker's transports
(§9) being the other way out, and its landing amends APPLICATIONS.md §W.8
with a destination-carrying sibling. It sees only ciphertext for TLS
destinations, so policy is by host name, as in Claude Code's and Codex's
proxies, and a host that fronts other domains is a residual risk.

## 11. Approval

The tier an action gets is decided by which boundary it crosses. It is never
decided by the acting model's own claim about its risk; a self-assessed
flag lets injected text lower its own bar.

| Action | `ask` mode | `auto` mode |
|---|---|---|
| reads, grep, glob, todo, question, git_fetch | run | run |
| listing, reading, waiting on and killing own background processes | run | run |
| history of its own conversation; `conversations` | run | run |
| file edits, sed, shell (background and local commits included) inside the jail | human | run |
| reading or messaging another conversation (§3) | human | classifier |
| `schedule` | human | human |
| network to a destination on the workspace allowlist | run | run |
| network to another destination, policy `allowlist` | human | classifier |
| git_push, unprotected branch, no force, clean scan | human | classifier |
| git_push to a protected branch, forced, or a scan match | human | human |
| `request_directory`, read-only | human | classifier |
| `request_directory`, read-write | human | human |
| network policy `open`, shared directories, limits, remotes, rules, modes | human | human |
| anything in an unconfined workspace, reads included | human | human |

Creating a conversation and its workspace from a template, and archiving
or deleting one, are the human's own acts (§7), never a model's, so the
table has no row for them; admitting a template's remote and confirming
what removing a workspace would lose are the human's cards there. A
crossing between conversations is the human's in both modes until
increment 13 brings the classifier's row and "always" answers (§3).

**Order.** Deny rules first, and they always win. Then ask rules, the
human's and the workspace's. Then the human-only rows, which no rule can
lower. Then the human's allow rules. Then the table.

**Rules.** A rule names a tool and, for `shell`, an argv prefix matched
against every pipeline element and every `&&`, `||` and `;` segment of the
parsed command; for network, a host and port; for `git_push`, a remote and
branch. A rule never matches the raw string. Shell rules are syntactic, so
they are not a boundary; the jail is. The matcher treats this exact set as
opaque: a command it cannot split; a function definition; an `alias` or
`trap`; a leading `NAME=value` assignment; `sh`, `bash`, `dash`, `zsh`,
`ksh`, `fish` or `td-sh` given `-c`, alone or clustered (`-lc`); `busybox`
with any applet; `eval`, `exec`, `command`, `builtin`, `.`, `source`,
`env`, `xargs`, `nohup`, `nice`, `ionice`, `taskset`, `chrt`, `setsid`,
`stdbuf`, `flock`, `time`, `timeout`, `unshare`, `chroot`, `nsenter`,
`sudo`, `doas`, `su`, `script`, `watch`, `strace`, `setpriv`, `prlimit`,
`systemd-run`, `bwrap` and `parallel` as command words; `find` with
`-exec`, `-execdir`, `-ok` or `-okdir`; `git` with `-c`, `-C`,
`--config-env`, `--exec-path`, `--git-dir`, `--work-tree` or
`--namespace`; command or process substitution; and backticks. The list
is reviewed code and stays closed; a wrapper it does not name is not
opaque, which is one more reason a deny rule is only advisory.
Accordingly:

- an allow rule never matches a command containing an opaque construct;
- a deny rule is advisory, and says so where it is made. When any shell
  deny rule exists, a command containing an opaque construct goes to the
  human in either mode rather than running.

Beyond that set, an allow rule for an interpreter or build tool (`make`,
`cargo test`, `python`) is inherently broad, since what it runs is
workspace code; the card that writes such a rule says so. A hostile
workspace's deny and ask rules can make every command reach the human,
which costs attention but never safety; the workspace card lists them.

Allow rules come only from the human, written by a card's "always"
actions. A deny written by "always deny" applies, at the human's choice
on the card, to this workspace or to every workspace, present and future,
so that no conversation sheds it by working in a new workspace. A
repository's own `.td-agent/rules`, read from the base commit (§7), may add
deny and ask rules and nothing else, so a hostile checkout can narrow what
runs but never widen it. A boundary the human states ("don't push")
becomes a rule through a card, not text in the transcript; a boundary
stated only in chat can be lost to compaction, and a rule cannot.

**As built (increment 13, a repository's rules).** `.td-agent/rules`
holds one rule a line: `deny` or `ask`, then a tool the tool host runs
(`read_file`, `write_file`, `edit_file`, `glob`, `grep`, `sed` or
`shell`), then, for `shell` alone, the words of an argv prefix, parted
by spaces or tabs. Blank lines and lines starting with `#` are skipped.
A word may not hold what the shell would read rather than pass, or what
the matcher could never match: a quote, `\`, `$`, a backtick, `;`, `&`,
`|`, `<`, `>`, a parenthesis, `#`, a glob or brace character, a leading
`~`, or a control. The git worker reads the file at each base beside the
project instructions, at most 16 KiB and 256 rules, each commit's
crossing once and all at most 32 KiB in one answer; the conversation
records them with the instructions, each commit's counted and written
once against the same bound, and the workspace card lists them. A file
not read whole (past its bound, not UTF-8, not a regular file, or with a
line refused, an `allow` among them) is refused whole and its reason
named on the card in td-agent's own words, as is a record made before
td-agent read rules; every call that changes the workspace or runs a
command then asks the human, saying why, so a broken file narrows and
never widens. A deny refuses the call before any card, whether or not it
could run yet, its answer to the model naming the rule and its
repository, and logs the approval `by` `rule` with that reason; an ask
puts the call on the human's card once it could run, the card saying
first which rule asks and the approval logged with that reason. A deny
or ask matches a command word by its name after any `/`, so `/bin/rm` is
`rm`, and one on `git` reads past git's options before the subcommand,
so `git --no-pager push` is `git push`; an allow matches the command
word as written, so `./git` is not `git`, and a command runs on the
human's allows only when each of its segments starts with one of them,
so `allow shell cargo test` and `allow shell cargo fmt` together cover
`cargo test && cargo fmt`. Composed so, one allowed program can feed
another through a pipe, `curl ... | git apply` running on an allow for
each: an allow for a program that does what its input says means more
for it. The words after it match word
for word, and a word an expansion or a glob decides (`$X`, `*.o`, a
leading `~`) matches none. Besides the set above, the matcher treats as
opaque what it reads as compound (a reserved word such as `if`, `{` or
`!`, a subshell, a here-document), a command word an expansion decides,
an append assignment `NAME+=value`, `$'…'` quoting, a `${…}` holding
more than a name, `&>` (which dash, as `sh`, reads as `&` and `>`),
`hash`, fish's `--command`, a shell given `-s` or no script, so reading
commands from its input, and a shell's or `find`'s option an expansion
decides, and an option before git's subcommand that may take the next
word as its argument: one written without `=` that the matcher does not
know to take none. The shell's joining of `\` and a newline is undone
first, and a newline after `&&`, `||` or `|` continues the command. A
command the matcher cannot see into asks while any shell deny or ask
rule exists, not only a deny. A segment of redirections alone, such
as `> notes.txt`, is kept as a segment with no words, which no rule
with words matches. A call that acts and that no allow of the human's
matches takes its workspace's mode's column (As built (increment 13,
modes)).

**As built (increment 13, the human's rules).** The human's rules are
the state directory's `rules`, read when the window starts: a header,
`[everywhere]` or `[<workspace>]`, then that scope's rules one a line as
a repository's are written, an `allow` among them only under a
workspace's header; blank lines and `#` comments are skipped, and a
control character other than a tab, in a comment too, refuses the file;
at most 256 KiB and 4,096 rules. A workspace's key, and so its header,
is `workspace <name>` for a repository workspace, whose forks share its
name, `directory <path>` for a directory, each byte of its path but
printable ASCII other than `%`, `[` and `]` written `%XX`, and
`conversation <id>` for a scratch one; a header of no such form refuses
the file, so a mistyped one is not a scope no workspace has. The window
sends every conversation the file whole as a numbered policy (`{"type":
"policy", "version", "rules"}`, or `"error"` for why it could not be
read), second on the socketpair after `setup` and again on every change,
and the conversation takes it at once, a turn under way or not. Until
the window has read the file, and when it cannot be read or would not
fit one frame, the policy is an error: it is said in the window, and
every conversation then asks before each call that acts. A conversation
judges its workspace's rules and the ones for every workspace with its
repositories', in the order above: a deny wins, then an ask, then the
asks for an opaque command and an unread file, then the human's allow,
which runs a call that acts with no card, its approval logged `by`
`rule` with the rule as the reason. A repeated call still goes to the
human. The cards' "always" answers write the file; the human may edit it
too, and it is read again when the window starts.

**As built (increment 13, card answers).** A card for a workspace's call
offers its "always" answers with what they would remember (`{"type":
"ask", ..., "always": {"allow", "rules"}}`): the tool alone, or for each
segment of a command its program and, for a program whose second word
names what it does (`git`, `cargo`, `make` and the like), that
subcommand, at most 8. It offers no allow for a command the matcher
cannot see into, a segment of redirections alone, or such a program with
no subcommand to name, nor for a word quoted with a space or tab in it,
which would read back as two, so that no allow is broader than the
program and subcommand; and none while a rule or an unread file asks,
since an allow would not run the call before them. A command the matcher
cannot split offers nothing. The card's rows run Cancel, `Always deny
here`, `Always deny everywhere`, `Always allow here`, then `Allow`, the
allows farthest from Cancel, and its last lines say what each adds, a
rule a line so that no line passes the dialog's bound, and, for an
interpreter or build tool, that an allow for it is broad. The window
adds an "always" answer to the file itself, the workspace's key taken
from the conversation's record rather than its process: below the file's
last header when that is the scope's, else under a new one at its end,
every other line kept, a rule the scope holds not added again. It sends
the decision with what it remembered, which the approval's reason names
(`always: …`), and then the file to every conversation, so the card's
own call is decided by the human. One it cannot add, the file unread
say, is said in the window, and the answer holds once. A card that waits
when the policy changes is judged again: a rule that now refuses its
call, or now lets it run, withdraws it, and its approval is the rule's;
a policy taken after the decision and before the call starts is judged
too, and only a deny in it then refuses the call, the decision standing
otherwise; an allow's reason names at most three of the rules that let a
call run. Deleting a conversation takes its workspace's sections out of
the file, a scratch or repository workspace being its own; a directory's
stay, the directory being the human's.

**As built (increment 13, crossings answered for good).** The human's
file holds a `[crossings]` section, each line `allow` or `deny`, `read`
or `message`, then the conversation that does it and the one it is done
to, by id: one operation, one way, one pair, so allowing A to read B
lets neither B read A nor A message B; a search is a read. A
conversation's crossing takes its standing answer before any card, a
deny winning, its approval the rule's; but once three messages to one
conversation have started since the human last wrote in the sender,
whether allowed, refused or decided on a card, each further one goes to
a card, so that two standing answers cannot keep two conversations
messaging each other. With no answer the card asks; with the human's
rules unread it says so and offers nothing to keep; else it asks,
offering `Always deny` and `Always allow` beside Cancel and Allow, and
no "everywhere". The window writes the answer under `[crossings]` with
the asking conversation as the one that does it, never a pair the
conversation's process names, and only for a conversation still there,
and the card's wait, and the check before the crossing runs, take a
changed policy as a tool call's do. Deleting a conversation takes every
crossing to or from it out of the file.

**As built (increment 13, modes).** A workspace's mode is a `mode ask`
or `mode auto` line under its header in the human's rules file, the last
one counting, and the configuration's `mode` where there is none;
`[everywhere]` and `[crossings]` hold none. The window sends the
configuration's mode with each policy, and the conversation judges by
its workspace's: in `auto`, a call that acts runs with no card once no
deny, ask, unread file, repetition or allow has decided it, since each
such call runs inside the jail, its approval logged `by` `mode` with the
reason. A crossing is never the mode's: in `auto` the table gives it to
the classifier (As built (increment 13, the classifier)).
A card that waits when the workspace goes to `auto` is taken back and
its call runs, as a rule's allow would. A call the mode or a rule let
run, the person not having answered it, is judged again when a policy
comes before it starts, from the one it was judged by: a deny refuses
it, and a card goes to the person. With the human's rules unread, or no
workspace, the mode is `ask`. Only the human sets a mode: Conversation >
Auto mode in this workspace, checked while it is in `auto` and offered
only with a workspace open, writes the line, every earlier `mode` line
under that header taken out and every other line kept, and the window
sends every conversation the file again. The item asks only when td-ui's
window's own pointer or keyboard chose it, as Help > Keys shows only
then: the control socket's keys and pointer reach it and choose nothing,
and nothing of a conversation's process asks for it. The status row's
`mode` says the open conversation's workspace's mode, `ask` for a
conversation with no workspace and while the human's rules cannot be
read, and the configuration's with none open. A deleted conversation's
workspace's mode goes with its section, but a directory's, which
outlives its conversations as its rules do; the blank line before a
section that goes goes with it.

**As built (increment 13, the classifier).** The classifier decides the
one row of the table's `classifier` column that exists so far,
crossings: in `auto` mode, a crossing no standing answer, ask, deny or
unread file has decided, and that is neither repeated nor braked, goes
to both stages; a repeated one, a fourth message to one conversation
since the human last wrote (the brake applies to the classifier as to a
standing answer), and one in `ask` mode, are the human's. The state is
built by the conversation process from its log alone: the human's
messages (`User` events, never another conversation's), the workspace's
mode and the rules in force with where each comes from, and the action
with what td-agent says of it and its two sides, `source`, whose content
it carries, and `receiver`, which that content reaches (a message's
sender and receiver; a read's or a search's other conversation and this
one), each with its conversation, workspace, remotes and model.
Everything a model wrote is in the untrusted field: the message or the
query whole, the tool calls made by tool (a name td-agent has no tool by
shown as `an unknown tool`) and the `path` each named, cut at 256 bytes,
the other conversation's title and the latest four messages other
conversations sent. A payload longer than 32 KiB, which no tool takes,
is the human's, as the stages would judge a part of it. The human's
messages are kept newest first within 32 KiB, each at most 8 KiB, and
the last 64 calls are named. Project instructions are not in it yet: no
workspace is marked trusted. Jev is asked only when `data_collection` is
`allow`, `jev_threshold` is set and `base_url` ends in `/v1`, and the
models list prices `classifier_fast_model` while a cost limit is set;
otherwise it is unavailable, and while `jev_required` the action goes to
the human with nothing asked. The reasoning stage is a chat completion
of `classifier_model` with `prompt/classifier.txt` as its system message
and the state as its user message, `max_tokens` 4096, `provider` as any
request's. Both requests are reserved together against the day, the
turn's and the conversation's limits, logged as `Request` events of
purpose `classify` with their whole bodies, sent at once, Jev's on a
thread of its own, and settled as a title request's is, Jev charged by
its reported cost, or by its input tokens alone; neither can be
interrupted while it runs. Jev's answers are read strictly, a
probability outside 0 to 1 being no answer, and allow when `request` is
`matches` and both its probability and that of not disclosing reach the
threshold. The reasoning stage's answer is one JSON object with a
`verdict` and a `reason`; a fence or prose around it is no answer. When
both allow, the crossing runs, its approval logged `by` `classifier`
with Jev's probabilities and both stages' words as the reason, once the
decision holds: a policy that came after the one the crossing was judged
by, one taken while the classifier waited included, has it judged again,
a deny refusing it. Otherwise the classifier's verdict is logged as an
approval of outcome `ask` `by` `classifier`, and the card says why, with
Jev's probabilities when it answered; the human's answer is logged after
it. A card waiting on a crossing is not taken back when its workspace
goes to `auto`. The status row says `classifier without Jev` beside the
mode while `jev_required` is `false`. `jev_threshold` has no shipped
value until increment 13's calibration records one, so Jev allows
nothing until the human sets one.

**Repetition.** Three consecutive calls of one tool with identical
arguments go to the human whatever the table says, as opencode's
`doom_loop` does: a loop is a symptom worth a person's look.

**As built (increment 13, repetition).** The run is counted over the
tools the tool host runs (`read_file`, `write_file`, `edit_file`,
`glob`, `grep`, `sed` and `shell`), back through earlier replies. The
human's message ends it; another conversation's message, a
notification or a process's end does not, so a loop a peer drives
stays visible. A reply cut off before it was whole is passed over,
since its calls never ran. Arguments compare as parsed JSON with each
object's members in key order, so respacing or reordering them does
not hide a repeat. The third call's card, and every later one's while
the run lasts, says first why it asks, and its approval is logged with
the reason `repeated`; refused, the call's answer is the human's
refusal, as for any card. The process tools run in the conversation's
own process and are not counted: waiting on a long process with
`process_wait` repeats by design. Nor are the conversation tools:
`send_message` and a search or read of another conversation are
crossings, every one the human's, on a card or by a standing answer
that a run of messages sends back to them (As built (increment 13,
crossings answered for good)), or in `auto` mode the classifier's
(As built (increment 13, the classifier)),
and searching or reading this conversation's log, listing
conversations or writing the todo list changes nothing outside it.
When the classifier decides crossings, a repeated one must still reach
the human.

**The classifier** decides only the rows the table gives it, and an action
it decides runs only when both of its stages allow. Both see the same
state, built from separated, labelled fields:

- the human's messages in the workspace's conversation; a task another
  conversation sent is in the untrusted field below, never here;
- the project instructions of §13, only for a workspace the human marked
  trusted, labelled as project text and not the human's authority;
- the workspace's policy: network policy and allowlist, admitted remotes,
  protected branches, rules;
- the tool calls already made, by name and path, with content arguments
  (file contents, edit strings, sed scripts) elided;
- for a push, the evidence of §9, computed outside the jail;
- the pending action;
- an untrusted field holding everything model-authored that the verdict
  depends on: the command line, the URL, the branch name, commit subjects
  and paths, and any message from another conversation or a schedule,
  which may carry a model's prose.

Neither stage sees the acting model's prose as such or any tool output;
this follows Claude Code's published design, where Codex's reviewer does
read tool output. Whether the action matches the request is judged against
the human's messages alone. That reduces rather than removes injection:
the untrusted field is model-authored, and an injected instruction can
shape it.

1. **Jev**, TypeSafe's decision model, which OpenRouter serves at
   `POST /api/alpha/decisions`, beside the API's `/v1` root (model
   `typesafe/jev-1.13`, configurable as
   `classifier_fast_model`), through fetchd like any other request. It
   takes the state and typed questions and answers with probabilities in
   well under a second, at a fraction of a chat model's price. td-agent
   asks a Choice, `request` of `matches`, `exceeds` or `unrelated` (the
   action against the human's request), and a yes-or-no, `discloses`
   (whether the action sends workspace content to a destination that is
   neither an admitted remote nor on the workspace's allowlist; a push to
   an admitted remote is therefore not by itself a disclosure). Content
   carried to another conversation, by a message or a read, is a
   disclosure when the receiving workspace's remotes, allowlist, network
   policy or model provider reach a destination the source's do not, and
   the state says which.
2. **The reasoning stage** is a chat model named in configuration
   (`classifier_model`; the shipped default a bring-your-own-policy safety
   model such as `openai/gpt-oss-safeguard-20b`), given the same state and
   td-agent's policy text, answering strict JSON `{"verdict":
   "allow"|"deny"|"escalate", "reason": "..."}`.

The two run at once. The action runs when Jev answers `matches` and not
`discloses`, each with probability at least `jev_threshold`, and the
reasoning stage answers `allow`. Any other answer from either, a malformed
reply, an error, or a timeout, sends the action to the human, with the
reasoning stage's reason when it gave one and Jev's probabilities. Jev is
in beta from a single provider and gives no reason; requiring both lets
it add its calibrated second opinion without ever being the only allower.
If Jev is unavailable, including because `data_collection = "deny"` leaves
it no provider, its rows go to the human; `jev_required = false` lets the
reasoning stage decide alone, and the status row says so. `jev_threshold`
has no shipped value until the fixture set of §17 has calibrated one; the
increment that lands the classifier records it.

**Outcome.** A refusal by the human reaches the acting model as the tool's
result with an instruction not to work around it. The circuit breaker
counts only actions the table gives the classifier: after three
consecutive such actions it did not allow, or twenty in a workspace, the
workspace drops to `ask` mode and says why; a human allow does not reset
the run, and only the human restores `auto`. Every verdict, with Jev's
probabilities, is in the log.

**Taint.** At most two of untrusted input, sensitive access and outbound
action should coexist without supervision. Every workspace holds its
worktrees, and every tool result its model reads is untrusted input, so
every workspace is treated as tainted from its first tool result. In
`ask` mode the human supervises every crossing. In `auto` mode, by the
user's choice, the classifier stands in for that supervision on the
crossings the table gives it, and on top of the human's standing
decisions: the allowlist, the admitted remotes, shared directories. That
is a probabilistic control, and its residual risks are stated rather than
hidden:

- a destination the human allowlisted, or the classifier admits, may
  serve or accept attacker-chosen content;
- the classifier sees `./run.sh` but never what the script contains, and
  a connection's destination but never the bytes sent;
- a push publishes whatever the branch holds, which the model wrote after
  reading untrusted input; the evidence and scan narrow that, not close it;
- a crossing the human or the classifier allows carries text from one
  conversation to another, so an injection read in one can be written
  into another's context; and a log holds what its conversation read
  from others, so allowing C to read B also lets C see what B read of A.
  Each step is a crossing of its own (§3); the text reaches the
  classifier only in the untrusted field, and the human's global denies
  follow it;
- a read-only directory's contents reach the provider and the workspace
  once granted, which is why the credential locations of §8 are refused
  outright rather than left to the classifier.

**Cards** are drawn by td-agent's chrome from toolkit widgets. Model text
is rendered as text in the message list, so it cannot draw one. A card
shows the exact action (for a push, the commit id, remote, branch and
evidence), the boundary it crosses, and the classifier's reason when there
is one. Its choices are: allow once, deny, always allow in this
workspace, always deny in this workspace, and always deny everywhere; a
crossing's "always" answers name its pair and direction instead (§3).
These approvals are not td elevation (principle 7). They govern what a
workspace's jail and the git worker may do, grant nothing beyond the
human's own authority, and carry no secure-attention claim. An action
needing elevation is out of td-agent's reach by design.

**As built (increment 10).** There is only `ask` mode (modes come with
increment 13), and the human
rows that arise are a workspace's `write_file`, `edit_file`, `sed` and
`shell`, and every conversation's crossings (§3, As built (peers)). For
each, the conversation process sends the window `{"type":
"ask", "call", "title", "details"}`, `call` the call's `tool_call`
record, and waits without a deadline, hearing the window meanwhile; the
window answers `{"type": "decision", "call", "allow"}`. A card shows the
exact action in td-agent's own lines, what runs before what it runs
over: the command with its directory and timeout, a file with its whole
new content, the text an edit replaces and its replacement, a sed
script and then its files, or a crossing's other conversation, by id
and title, with the message it would send or the page or query it would
read and that the other log comes into this conversation's context and
so to its model's provider. Each part shows at most 80 lines or 48 KiB
(a command, a file or a message 160), each line at most 2 KiB, and the
card at most 240 lines or 128 KiB, saying what it leaves out, so that no
part pushes another off the card. Control characters, every whitespace
character but a plain space, a tab included, and the invisible and
bidirectional characters show as `<U+XXXX>`, so that nothing that looks like a space
can hide where `sh` ends a word and one command cannot pass for another
that way; letters that look alike are not told apart. The window shows
the open conversation's first card as td-ui's confirmation dialog, at
most 100 cells by 24 rows, whenever nothing else is modal and it has the
keyboard, `Cancel` focused, so `Return` alone refuses, as `Escape` does;
`Allow` lets the call run. A card takes no key or press until 750 ms
have passed with none, so what the human was typing or clicking does not
decide it. Losing the keyboard sets the card aside, deciding nothing,
and it comes back with the keyboard. Another conversation's card waits,
its row saying `asks you`, until that conversation is opened; one the
window cannot draw, in a window too small, waits and says so once. The
choices are allow once and deny, and from increment 13 the "always"
choices (§11, As built (increment 13, card answers)). An interrupt, or the window closing, withdraws the card
(`{"type": "withdraw", "call"}`) and the call is answered as not run, as
is an allowed call whose interrupt came with its decision; a
conversation process that restarts, fails or is deleted takes its cards
with it. Each decision is an `approval` event before the call runs or is
answered: `allow` or `deny` by `human`, or `withdrawn` by `td-agent`
with why. A refusal reaches the model as the call's error, telling it
not to reach the same result another way and to ask how the person would
like to go on.

## 12. Tools

A conversation's tools are small and non-overlapping, following
Anthropic's guidance on writing tools for agents: fewer tools, natural
identifiers, actionable errors. Their definitions are fixed text in the
prefix. Every conversation has the conversation tools, `todo_write`,
the history tools, `conversations`, `send_message`, `question` and the
schedule tools; its workspace adds the file, shell, process, directory
and git tools, as far as the workspace has what they act on.

- **`read_file {path, offset?, limit?}`**: line-numbered output, at most
  2,000 lines or 100 KiB. A partial view says so and names the next
  offset. Reading a directory is an error that names `glob`.
- **`write_file {path, content}`**: creates or replaces a file. Replacing an
  existing file requires that this conversation read it, and the file is
  unchanged since: the tool host returns a digest of the whole file with
  every read, partial reads included, and refuses a write whose expected
  digest does not match. It is a correctness aid, not a security check,
  since the tool host is jail-controlled.
- **`edit_file {path, old_string, new_string, replace_all?}`**: exact,
  unique match, the Claude Code and `str_replace` design. It has the same
  read-before-write rule. A missing match or several matches is an error
  that says which and asks for more context. No fuzzy application.
- **`shell {command, timeout_ms?, workdir?, background?}`**: one `sh -c`
  per call, in a jail instance of its own, as mini-swe-agent and Claude
  Code run one process per call. The working directory defaults to the
  first worktree and does not persist between calls, and neither does
  anything a foreground command leaves running. Default timeout two
  minutes, maximum ten. The result carries the exit status and output,
  cut to a head and a tail with the omitted byte count named; the full
  output is kept in the log. With `background: true` the call returns at
  once with a process id instead, and the instance lives on (below).
- **`grep {pattern, path?, include?, exclude?, extended?, ignore_case?,
  context?}`**: td-txt's `grep -rn`, run in an instance of its own, with
  the structured arguments mapped to its options (`-E`, `-i`, `-C`,
  `--include`, `--exclude`), the pattern passed with `-e` and the paths
  after `--`. Its patterns are td-txt's POSIX basic and extended
  expressions, and its results are capped. Reusing td-txt keeps one grep
  in td, conformance-tested against GNU's corpus.
- **`sed {script, paths, extended?}`**: td-txt's `sed --sandbox -i` over
  the named files, the script passed with `-e` and the paths after `--`,
  for a substitution across many files that `edit_file` would take many
  calls to make. td-txt's sed has no command that starts a process, and
  `--sandbox` refuses `r`, `R`, `w`, `W` and the `w` flag, so a script
  reads and writes only the named files; its effects are recorded by the
  step snapshot like any edit.
- **`glob {pattern, path?}`**: implemented in the tool host in std, capped
  and sorted.
- **`todo_write {items: [{content, status}]}`**: the todo list (below).
- **`process_list`**, **`process_output`**, **`process_wait`** and
  **`process_kill`**: background processes (below).
- **`history_search`** and **`history_read`**: the conversation's full
  log (below).
- **`conversations`** and **`send_message`**: other conversations (§3).
- **`schedule`**, **`schedules`** and **`cancel_schedule`**: later, with
  schedules (§3).
- **`question {question, options?}`**: asks the human on a card and
  returns the answer, as opencode's `question` does; every conversation
  has it, from a later increment (§18).
- **`request_directory {path, write?}`**: asks for an extra host directory
  bound into this workspace's later instances, admitted per §8 and decided
  per §11.
- **`git_fetch`** and **`git_push`**: §9.

Paths are absolute. Worktrees and shared directories are bound at their
real paths, so the paths the model sees are the paths the human sees. A
relative path is an error naming the worktrees.

**Step snapshots and undo.** Before and after each model step that
changes files, the tool host records each worktree's state as a git tree,
with `git add -A` into a private index in the instance's `/tmp` and
`git write-tree`, into the workspace repository's objects, and commits the
tree onto `refs/td-agent/snapshots/<worktree>` so gc keeps it; the log
records the tree ids and the files that changed. This is opencode's
shadow-git design, without the shadow repository, since every worktree is
already a repository. The human can undo a step, which restores its
changed files from the before-tree, and redo it, through the tool host in
a jail instance, never as a write by an agent process. A snapshot is
jail-controlled data, good for undo within the same jail and for showing
diffs, and never trusted outside it. It covers tracked files and
untracked files git does not ignore: ignored files (`target/`, `.env`)
and shared directories are not snapshotted, and undo cannot restore them.
A directory or scratch workspace without git records pre-images of
`write_file`, `edit_file` and `sed` targets instead, which `--sandbox`
makes the whole of what `sed` can write.

**As built (increment 11, step snapshots).** A repository workspace's
step snapshots are recorded, and undone and redone (below); a
directory or scratch workspace's pre-images come later. A step whose reply
calls a tool that acts (`write_file`, `edit_file`, `sed`, `shell`) is
snapshotted before its first call and after its last, however its
calls end, each time in a fresh tool instance by td-agent's own call,
`snapshot` (`host::Call::Snapshot`, `src/snapshot.rs`), which no model
can make and no card asks: it names each ready worktree's checkout and
the host's git by the path it resolves to, as maintenance does, since
an instance's `PATH` need not hold one. For each, the tool host copies
the worktree's index, its time with it, to a private one in the
instance's `/tmp`, so the worktree's own, and what is staged in it,
are left alone while its stat cache spares rereading a tracked file
unchanged since and its sparse checkout keeps what lies outside it;
clears assume-unchanged marks in the copy, so a change under one is
taken; and runs `git add -A --sparse`, taking a file made outside a
sparse checkout too, and `git write-tree` with it. Git runs with no
configuration, ignore rules or attributes from the home or the system
(`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM`,
`GIT_ATTR_NOSYSTEM`, `core.excludesFile` and `core.attributesFile`
`/dev/null`), attributes from an empty tree (`GIT_ATTR_SOURCE`) and
`core.autocrlf=false`, so each blob is its file's bytes whatever a
`.gitattributes` says and what the model plants in its home neither
hides a file nor converts one, and with no hooks or fsmonitor; the
host's git must be 2.40 or later, since an older one ignores
`GIT_ATTR_SOURCE` without a word. It commits the tree onto
`refs/td-agent/snapshots/<worktree>`, its worktree's id, over what it
read there, unless that is a commit holding the tree already; a value
there that is no commit, which the jail can write, is replaced, not
taken as a parent. Against the trees before, it names the files
changed (`diff-tree -r -z --name-only --no-renames`), at most 40 per
worktree of at most 200 bytes each, within 16 KiB per worktree and
256 KiB in all as JSON escapes them, the rest counted, so the answer,
escaped again, fits a frame and the record a log line; the
conversation refuses an answer past those bounds. Each worktree the
step changed is
logged in one `snapshot` event of its reply: its checkout, its trees
before and after, and those names, which the window and
`history_read` show made visible, since a jail wrote them; the model
is not given it; the window shows one notice for the step. A snapshot
that cannot be taken, a tool instance that cannot start, git that
fails, or one past 60 seconds, is said in a notice once a process,
that step is not recorded, and no other is tried in the turn, so a
slow or failing one costs a turn once; a record with no room left in
the log, or that cannot be written, is the same. The removal survey
does not count a snapshot's commits as work.

**As built (increment 11, undo and redo).** `C-z` (the driven action
`undo`) undoes the open conversation's latest recorded step and
`C-S-z` (`redo`) redoes the latest undone, naming it by its
`snapshot` event's place; the window refuses either while a turn
runs, and both sides count the steps from the log alike
(`store::Steps`): each `snapshot` is done, an undo moves the latest
done to the undone, a redo the latest undone back, and a new
`snapshot` leaves nothing to redo. The conversation refuses, saying
why in a notice, a step that is not the latest to undo or redo, as a
request that waited out a turn may be, one whose worktrees are not
all ready, a removed workspace, no settings from the window, and a
log without room for the record and the notification at their most,
checked before a file is written. It asks a fresh tool instance, by
its own call `restore` (`host::Call::Restore`, `snapshot::restore`),
to bring each worktree the step changed from one tree to the other:
after for before to undo, the reverse to redo. The tool host first,
before writing any, checks each worktree: at the tree it is to leave,
written as a snapshot is, so nothing changed since, by a later step,
the human or a process, is overwritten, or at the tree it brings
already, an earlier try's, which it then leaves; no submodule among
what differs (`diff-tree --raw`, mode 160000), since git writes
none; and nothing a tree cannot hold, an ignored file, where it
writes: at a path the tree it brings adds, unless a file of the very
bytes it writes there (`hash-object --no-filters`) or a directory
holding only files it removes, nor on the way to one, unless a
directory or a file it removes. A tree comparison cannot see ignored
files, which `checkout-index -f` would otherwise write over or delete
in a directory in its way. Then, in each, it removes the files the
tree it brings lacks, through no link and never a directory, and the
directories that leaves empty; writes the rest that differ with `git
checkout-index -f` from a private index of that tree, attributes
from the empty tree; checks the worktree is now that tree; and moves
the snapshot ref to it. Each file written is its blob's bytes and
mode, since the repository's `config` and `info/`, which could name
a filter or attributes, are td-agent's, bound read-only in the jail
(§9). A path git names must be plain: no `..`, `.`, empty or `.git`
name. The worktree's own index, its commits and the ignored files
the checks allow are left as they are: an undo restores files, not a
commit the step made. A failure names its worktree and those
restored before it, and a retry passes over those. The window keeps
the conversation's process, though left, from asking until the
conversation says it is done (`Up::Restored`), after its record, a
count of those asked; a process that fails meanwhile is restarted
unasked, so its count ends. Why one failed is quoted, as a
notification quotes it. Done,
the conversation logs an `undo` or `redo` event naming the step and
tells the model in a notification, which wakes no turn: the person
undid, or redid, its step, and the files it changed, made visible
and at most 1000 characters. While a background process runs, an
undo or a redo is refused before any other check (As built
(increment 12, lifetime)).

**Background processes.** A `shell` call with `background: true` keeps
its instance running after the call returns, for a build, a watcher or a
server. It gets an id (`p1`, `p2`, ...) numbered from the log and never
reused within the conversation, restarts included, and the same policy,
mounts, network and approval as a foreground call; its standard input is
empty. The foreground timeouts do not apply: it runs until killed, or
for `timeout_ms` when the call gives one, at most 24 hours.

- `process_list`: the conversation's background processes, each with
  its id, command, start time, state (running, exited with its status,
  killed, or lost) and output size.
- `process_output {id, from?, max_bytes?}`: output from a byte offset
  counted from the process's start, bounded like `read_file`, naming the
  next offset and the range still retained. The conversation process
  keeps each process's output, standard error interleaved as a
  foreground call's is, through the one pipe, in
  its conversation directory up to `background_output_bytes` (default 16
  MiB), dropping the oldest beyond that; a read from before the retained
  range starts at its beginning and says how many bytes were dropped.
  The output is kept after the process ends and across restarts, until
  the conversation is archived or deleted; `history_search` does not
  cover it, though the exit notice's tail, being in the log, is.
- `process_wait {id, timeout_ms}`: returns when the process exits or the
  timeout passes (at most ten minutes), with its state and the tail of
  its output.
- `process_kill {id}`: tears its instance down, with every process in it.

A conversation runs at most `max_background` (default 4) at once. When
one exits, a notice with its status and output tail is delivered between
turns like a message (§3) and wakes the conversation if idle, unless it
was killed: whoever killed it knows. Background
processes are listed under their conversation in the window's tree, each
with a kill action and its output viewable read-only, and the status row
counts them. They end when killed, when their conversation is archived
or deleted, or when their conversation process exits; none survives
td-agent, and on restart the log records each still running as lost. A
background process keeps its conversation process running (§2). Each
instance has its own network namespace, so a server one call starts is
not reachable from a later call's instance; §19 records that.

A background process can change a worktree between and during steps, so
a step snapshot may include its changes, and the step's diff says that
background processes were running. Undo and redo are refused while any
background process of the workspace runs, since a restore could
overwrite what one wrote or be overwritten by it.

**As built (increment 12, lifetime).** `shell` takes `background`; with
it the call is `host::Call::Background`, its `timeout_ms` from 1 ms to
24 hours and 24 hours when left out, decided on a card of its own ("Run
a command in the background"). It is refused before any card when
`max_background` processes run (§15: 1 to 16, 4 by default, carried to
the conversation in its `Setup`). Otherwise the conversation launches
its instance as for any `shell`, from its main thread, logs a `process`
event with its number, the next after the log's highest, its `ToolCall`
and its command, and hands the instance to a thread of its own, which
watches it until its answer, its kill or its time with the grace a call
has (`bench::limit`). The call's result is its id. The tool host runs it
as a `shell`, but answers with how it ended alone. How it ended comes
back through the inbox: heard, the process runs no more for the cap,
`process_list` and `process_kill`, and it is logged as an `ended` event
between a turn's steps, before an undo or a redo, or while idle (then
synced), as one bounded line with every control named, since a replaced
tool host could say anything: its exit status as a call's says it
(`timed out after ...` when the tool host's own timeout ended it),
`killed`, `timed out` (the conversation's deadline), or `failed:` with
why. `process_list` shows the latest 50 of the log's processes, saying
how many earlier it leaves out, each with its state, start time and
command cut to 200 characters; and `process_kill` tells its watcher to
drop the instance, which ends everything in it; both are the
conversation's own tools, with no card. A conversation's process that
ends drops its watchers, and with them its instances; the next one to
open the log records each process still running there as `lost`
(`store::PROCESS_LOST`), as it records an interrupted call. The window
counts a conversation's running processes from those events (a restart,
or a hello, zeroes the count before the log replays) and keeps its
process while any runs. An undo or a redo is refused before any other
check while one runs, whichever step it names. The output store,
`process_output`, `process_wait` and the list's output size came next
(As built (increment 12, output)), then the exit notice,
`conversations`' count and the step's note (As built (increment 12,
notices)). The window's process list and the status row's count came
last (As built (increment 12, window)).

**As built (increment 12, output).** The tool host sends a background
call's output up as it comes, waiting for room rather than dropping what
does not fit, as a foreground call's is dropped from the live view; and
once the process ends it reads what is left until the pipe has been
quiet for 300 ms, however long each piece waits for room, for at most a
minute (`shell::run_whole`; its answer says when output was still coming
then), where a foreground call stops 300 ms after its end. The watcher
writes it to `processes/` in the conversation's directory
(`output::Writer`), in segment files of a quarter of
`background_output_bytes` (§15: 64 KiB to 1 GiB, 16 MiB by default,
carried in the `Setup`), each named for the offset it begins at
(`p1-00000000000000000000`), dropping the oldest while the rest hold
more than the bound, so between three quarters of it and all of it is
kept. An offset counts the output as kept, text, each byte the process
wrote that was not UTF-8 replaced, since the tool host's protocol
carries text. Files are made private, opened with no link followed, and
only segments named so are read; a writer first removes any its number
left, a process whose start a crash kept from the log. A write that
fails stops the keeping and the process's end says from which byte. A
read (`output::read`) lists the segments, starts at `from` or where what
is kept begins, and cuts at characters, a start inside one moving on to
the next, and takes at least one whole character, so a read always moves
on; a segment dropped while being read is listed again. `process_output`
says the process's state, the bytes read, how many were written, the
next offset, how many were dropped, and when an offset it was given lies
before what is kept or past what was written; 32 KiB by default, 100 KiB
at most, as `read_file`'s. `process_wait` waits, hearing the window,
until the process ends, the person interrupts, the window closes, or
`timeout_ms` (at most ten minutes) passes, and says how it stands with
the last 15 KiB of its output, a foreground call's shown tail.
`process_list` says each process's bytes written. Archiving the
conversation removes `processes/` under its lock once the archive is
stored (`StateDir::set_archived`), and deleting it removes the rest with
it.

**As built (increment 12, notices).** A process's end is logged as an
`ended` event carrying, besides how it ended, the last 2 KiB of its
output, each line made visible and marked `| ` so that none passes for a
line of td-agent's, its last lines kept within 4 KiB as framed, the
first that does not fit cut to the room left and the tail left out when
the log has no room for it; the model reads it as td-agent's news,
naming the process's command, cut to 200 characters. How it ended is
taken from its tool host, which is the jail's, only in the shapes
`shell::Exit::status` gives (`exit status N`, `killed by signal N`,
timed out or interrupted with one of those, or the note that output was
still coming); anything else, and a failure the tool host's client
gives, which may carry the jail's standard error, is quoted, at most 300
characters. Whether its end is known already, a kill or a watcher that
could not start, which its call said, is carried apart from that text,
and such an end wakes nothing. An end heard during a turn is logged
between its steps, where the next request reads it, and before an undo
or a redo, and so is the end of a process whose watcher could not start;
one heard while the conversation is idle starts a turn whose `of` is the
end, counted against the wake budget as a message's (`wake::spent`),
held as a message is when the conversation is paused or the budget is
spent (the event's `held`; resuming starts the held turn), and starting
none when there is no usable key, or a turn of the person's waits. The
end and its turn's start are both logged before either is sent, and the
window is told first (`Up::Waking`) that a turn comes, keeping the
process until its start arrives even when that end was its last
process's. `conversations` counts each conversation's running processes
from its log. A step's snapshot names the processes running at any time
after its reply (`store::running_since`), and the window and
`history_read` say it may hold their changes. The end of one heard as a
person undoes wakes nothing: the undo is theirs, and the notice is read
with their next turn.

**As built (increment 12, window).** The window keeps each
conversation's running processes from its events, the open one's and
those working in the background alike: a `process` event adds one, an
`ended` one takes it away, and a hello, a replay of the log or the
conversation's process failing starts the count again, and archiving or
deleting the conversation, which ends them, clears it. Each is a row
under its conversation's in the list, `p1` and its command made visible,
always shown, and the status row counts them all. A right press on one,
or `S-F10` with it selected, opens its menu: Show output, then Kill.
Return shows its output, as the menu's Show output does, and so does a
press on it while it is selected, by an earlier press, a double press
among them, or the keyboard; a press that selects it opens no
conversation. Kill sends `Down::Kill`, which the conversation acts on as
soon as it hears it, between turns or within one, ending the process as
`process_kill` does; its end wakes nothing. Show output has the window
read the process's last 32 KiB from the conversation's `processes/`
(`output::tail`), the files its watchers write and nothing else does
meanwhile, and shows it read-only in the workspace card's panel, named a
process output, in entries of 40 lines each headed by their numbers, the
range ahead of the first, each line made visible, closed by Escape. A
change to the conversations keeps a selected process selected, and its
end leaves its conversation selected.

**Todo list.** `todo_write` replaces the conversation's whole list with
items of `pending`, `in_progress`, `done` or `cancelled`, at most one in
progress and at most 50 items of 500 bytes each, the semantics of Codex's
`update_plan` and Claude Code's `TodoWrite`. The static text asks for one
on work of three or more steps. Each write is a log event, so the list
survives restarts and compaction (§14). It is drawn above the composer,
so the human can follow work without reading the transcript;
`conversations` (§3) shows a conversation its own item in progress and
no other's; and the human can clear it. It has no effect outside the
conversation and needs no approval.

**The conversation's log.** The model's context is a view of the log
(§6): compaction prunes and summarizes it (§14), and tool results are cut
for the model. The log itself keeps everything, and two tools reach it:

- `history_search {query, conversation?, kinds?, limit?}`: events whose
  text contains every one of the query's terms, case-insensitively, newest
  first; `kinds` narrows to user messages, messages from other
  conversations (and an older log's from the orchestrator, which the
  kind `orchestrator` names), schedule firings, notifications and
  notices, assistant text, tool calls, full tool results, approvals or compactions. In
  either tool, an approval shows the model only its outcome and who
  decided it (a rule, the classifier or the human), never Jev's
  probabilities or the reasoning stage's reason, so an injected model
  cannot tune against the classifier. Each hit carries its sequence
  number, kind, time and a bounded excerpt around the first match; at
  most `limit` hits (default 20, at most 100).
- `history_read {conversation?, from, offset?, count?, max_bytes?}`: the
  events from sequence number `from`, starting `offset` bytes into the
  first, rendered as text, bounded by `count` (default 20, at most 100)
  and `max_bytes` (default 32 KiB, at most 256 KiB). It ends with the
  cursor to continue from, a sequence number and an offset, or says the
  log is exhausted. A tool result comes back whole as the log retains it,
  paged by that cursor, not as it was cut for the model; a page never
  splits a UTF-8 sequence.

They run in the conversation process over the stored log, reading
another conversation's log directly (one writer, appends whole, §6). Over
the conversation's own log they cross nothing, since everything in it
was in the conversation already, and what they return is as trusted as
it was then: a tool result is still untrusted content. Another
conversation is reached as §3 says. Reasoning is searchable only where
the provider returned it as text. The stubs and summaries compaction
writes name the sequence numbers they replace, so a model can recover
what a summary dropped.

Planned later, each its own increment: `apply_patch`, taking Codex's patch
grammar as one string argument for models trained on it; `web_fetch`,
made by the conversation process through the fetch service as a network
crossing; a `task` tool for summarizing child conversations within a
workspace; and an MCP stdio client.

**As built (increment 8).** The conversation tools run in the
conversation process; their definitions are JSON schemas with
`additionalProperties: false`, in the prefix. An item's content is one
line, since the window draws an item to a line. `todo_write` answers with
the list as written. An event renders for the history tools as `#SEQ
KIND TIME` (UTC, ISO 8601) and its text: a message under its sender, a
reply's text and calls, a call's tool, a result's content whole, an
approval only as its outcome and who decided. `history_search` folds
case per character, so an excerpt of 120 bytes either side of the first
match falls on character boundaries in the original, and gives each
event at most once; a query is at most 1 KiB. `history_read`'s cursor
is a sequence number and a byte offset into that event's rendering; an
offset inside a character starts at that character, and a page always
takes at least one character, so it moves. A page is held to `max_bytes`
of text, and to 512 KiB as the tool result is logged, escaped. The todo
item in progress is shown by `conversations` for the caller's own
conversation alone, as the title is.
A member that is null is as if left out, since a model that fills
every member of a schema sends null for one it means to omit: an
optional one takes its default and a required one is missing. The
history tools' `conversation` is trimmed, and when empty or blank is the
caller's own log, as when left out; `send_message`'s `to` is trimmed
and refused when empty, never a default receiver. A conversation id
that does not parse is refused quoting the value, cut to 64 characters.

**As built (increment 9).** The tool host is `td-agent tool-host [--txt
PATH] [--root DIR]...`, every path absolute, serving over its standard
input and output; until increment 10 launches it in a jail, only the
tests start it, and no tool of this section is exposed to a model. Its
frames are `frame`'s, at most 1 MiB. Down, `{call: ID, tool, args}`
names a tool by the model's name for it, and `{cancel: ID}` kills that
call's process; up, `{output: ID, text}` carries a process's output as
it comes, at most 32 KiB a frame, and `{done: ID, text, kept, digest}`
or `{done: ID, error}` ends the call once. Each call runs on a thread of
its own, at most 16 at once: one more is refused as that call's error,
and a frame naming an id already running is not answered, since an
answer would end that call for its caller. One writer sends every frame,
a call's output before its end. Live output is best-effort: past 256
frames waiting for the writer it is dropped, so a conversation slow to
read never holds a call past its timeout, while a call's end is always
sent, by a guard that sends it even if the call's thread panics. A call
whose arguments are wrong is refused as its own error, and a result too
large for a frame is sent as one saying so; a frame that names no call
ends the host. The host ends when its input does, cancelling what still
runs and waiting for it. Live output is cut between characters.
`host::Client`, the conversation's end, reads at most 64 replies ahead
and passes on only replies to calls it has in flight, dropping any other
as the jail-controlled data it is.

The digest is SHA-256 in hex, the engine's implementation shared by
`#[path]`. A call that replaces or edits a file carries `expected`, the
digest of the conversation's last read or write of that path; with none
the file is refused as unread, and with another as changed since. Within
the host, a replacement's check and write are one step, so two calls
that read the same file cannot both replace it, and a new file is made
exclusively. A file tool takes only a regular file, opened without
waiting on a FIFO and checked again once open: a device, FIFO or socket
is refused, since it may never end. A read can be cancelled between
pieces. `read_file` reads in 64 KiB pieces and keeps at most 8 KiB of a
line, so no line is held whole; it numbers lines as `cat -n` does,
strips a line's `\r\n` to its text, and cuts a line past 2,000
characters naming how many more it had. Bytes that are not UTF-8 are
shown replaced and said; a NUL in the first 8 KiB makes the file binary,
shown as no lines with its size and digest. An empty file and an offset
past the end are said. A write creates missing parent directories and
writes the file in place, so its mode and links stay; it is not atomic.
`edit_file` takes UTF-8 files of at most 8 MiB, counted as read, naming
`sed` for others, and refuses an empty `old_string` and one equal to
`new_string`. `glob` matches its relative pattern, at most 1 KiB with at
most 16 `{` groups, under `path`, or the first worktree: `*` and `?`
within a segment, `[...]` with `!` or `^` negating and ranges, `**` for
any number of segments, hidden ones included, and `{a,b}`, at most 64
expansions; a leading dot is matched only by a dot. Matching backtracks
to the last star alone, so it takes at most the product of pattern and
name lengths. `{a}` is expanded to `a`, and a `{` inside `[...]` is
still a group. It enters no `.git` and follows no link to a directory,
and returns the matching entries that are not directories, a link to one
included, as absolute paths: it looks at no more than 200,000 entries,
sorts what matched and returns the first 1,000, saying when either bound
cut it short.

`shell` runs `sh -c` with its environment cleared to `PATH`, `HOME`,
`TMPDIR` and `LANG` as the host has them and `TERM=dumb`, standard input
empty and standard error on standard output's pipe, read through a queue
of 16 pieces so unread output waits in the pipe. `timeout_ms` is from 1
to 600,000, and a `workdir` that is not a directory is refused naming
it. The process, the cancel and the clock are looked at on every pass,
so a process that writes without pause still ends. A timeout or a cancel
kills `sh` alone; what it started is the jail instance's teardown's to
end (§8), and once `sh` has ended its output is read for 300 ms more,
then the call returns whatever a descendant still holds. The log keeps
the output's first and last 64 KiB as `kept`, not the whole of it, with
the bytes between counted, a character the cut splits among them; the
model is shown its first and last 15 KiB and that count, after the exit
status, a signal or the timeout asked for. td-txt runs under the
applet's name as its argv[0]. `grep` runs `-r -n -H`, naming the file
even when `path` is one, then `-E`, `-i`, `-C N` (at most 20),
`--include=` and `--exclude=` as asked, then `-e PATTERN -- PATH`, the
path defaulting to the first worktree; an exit status of 1 with no
output is "no matches", and at most 1,000 lines are shown, the rest
counted. `sed` runs `--sandbox -i`, `-E` as asked, then `-e SCRIPT --`
and its absolute paths, saying only how many files it ran over when it
prints nothing. Both have `shell`'s default timeout.

**As built (increment 10, the tools).** A conversation in a workspace is
given `read_file`, `write_file`, `edit_file`, `glob`, `grep`, `sed` and
`shell` after its conversation tools, defined as above without
`background`, which increment 12 adds; one outside a workspace is given
none of them, and its prefix is byte for byte what it was. The
conversation process checks a call's arguments for shape, every member
named and bounded, and leaves what they name to the tool host to judge
inside the jail; the model never names a digest. The conversation fills
it from its last read or write of that path, by the path's components,
from the result's `digest` in the log, so a process started again keeps
it. The conversation works out the instance policy once for each set of
shared directories (§8). It runs `read_file`, `write_file`, `edit_file`
and `glob` in one long-lived instance, started at the first such call
and again after one fails or the shared directories change, and each
`shell`, `grep` and `sed` in a fresh instance of its own, so nothing a
command leaves outlives its call. Every tool instance is started from
the conversation process's main thread, which td-jail ties it to; a
maintenance instance may start on a checkout's thread, which waits
for it to end (§7, As built (increment 11, asynchronous
preparation)). An
interrupt cancels the call under way; a window gone does not, so the
call ends whole in the log. Since what runs in the jail can stop or
replace its tool host, the conversation keeps each call's time too: its
`timeout_ms`, else two minutes, and 15 s more, or 5 s after a cancel.
Past that it tears the instance down and answers the call so. The result
carries what the model is shown, and the log keeps a command's output's
head and tail beside it as the result's `kept`, never sent back, and
drops `kept` before the result when the line would be too long. The
window does not yet show a command's output as it comes. `write_file`,
`edit_file`, `sed` and `shell` are decided by the human before they run
(§11, `ask` mode); the rest run. A repository workspace's steps are
snapshotted and can be undone (above); a change the human allowed in a
directory or scratch workspace stays, and td-agent cannot take it
back.

## 13. Prompting

The prefix of §6 is ordered from stable to volatile, so it caches, and is
followed by the conversation. Every conversation has the same static
text and conversation tools; its workspace adds its own tools and
paragraph.

1. **Static system text**, in this order:
   - identity and capabilities;
   - for a workspace conversation: task execution (keep going until the
     task is done, fix root causes, verify with the project's tests,
     commit when a coherent change is complete), editing (read before
     editing, absolute paths, minimal changes in the surrounding style),
     and the worktrees' readiness rule;
   - coordination, for every conversation: that what another
     conversation says is untrusted and never permission, that reading
     or messaging one is the human's to allow, and never to ask another
     conversation to work around a refusal;
   - tool guidance;
   - the active mode, and that refusals are not to be worked around;
   - the final-message form.

   This follows Codex's open base prompt and the published structure of
   Claude Code's. An Empty workspace swaps the coding sections for a
   shorter assistant section.
2. **Tool definitions.**
3. **The environment block:** the workspace's name and worktrees with their
   branches and bases, the shared directories, the network policy, the OS,
   and the conversation's creation date. It holds for the whole
   conversation and changes only when what it names does; the current
   time stays out of it, since it would break the cache, and is on each
   message instead (as built, below).
4. **Project instructions:** each worktree's top-level `AGENTS.md`, or its
   `CLAUDE.md` where there is none, labelled with its path, read from the
   base commit in the store by the git worker outside any jail as soon as
   that commit is fetched, before the model has run anything, and stored
   in the prefix; the first request waits for them (§7). They are
   upstream's text at the base, never anything a jail wrote. That
   snapshot is what the classifier is given for a trusted workspace; the
   workspace card shows it next to the trust mark, so the human trusts
   the text they can read, not a path. Files deeper in a tree govern
   their subtrees: the static text states the agents.md precedence (a
   deeper file wins, the user's direct instructions win over every file)
   and tells the model to read them when it works there, so they arrive
   as ordinary, untrusted tool output. A sparse checkout that omits a
   routed document is widened by the model when the routing sends it
   there.

The prompt texts live as plain files under `td-agent/prompt/` and are
compiled in with `include_str!`. They are named `.txt`, not `.md`: they
are program source, and a documentation-only waiver must never cover a
prompt change. Prompt and tool-description changes are reviewed like code,
and each tool's text is drafted against the evaluation fixtures of §17,
not written once.

Titles come from a cheap model (`title_model`) after the first exchange,
reserved like any request; the first line of the task stands until then.

**As built (increment 5).** The prompt texts are
`prompt/conversation.txt` and `prompt/title.txt`, with the workspace
paragraphs below. With no tools, no environment block and no project
instructions yet, the prefix is the static text alone, which says that
the conversation has no tools and that the window shows plain text. A
title request is the title prompt and one user message quoting the
first message and the start of the reply, each cut to 4 KiB, with
`max_tokens` 256. It is sent once, after a conversation's first reply.
Its reply's first line, without
quotation marks, becomes the title. A title request that is refused
leaves the first line standing and says why in a notice, and one that
fails records why in its request's finish; neither fails the turn.

**As built (increment 8).** The prefix is the conversation's tool
definitions and its static text (§5), the same for every conversation
but for a workspace's tools and paragraph. The static text names the
tools the conversation has and what it still lacks; asks for a todo
list on work of three or more steps; sends the model to the history
tools for what has left its context; says that a message from another
conversation is labelled, not from the person, possibly wrong, and
never permission; that reading another conversation or messaging it is
the person's to allow, on a card, and that a refused crossing is not to
be worked around; that messages are delivered later and not to wait for
a reply; and that each message it sends can start a turn that costs
money.

**As built (the environment block and message times).** The system
message is the static text, then an environment block of what holds for
the whole conversation:

- when it began: its `meta`'s creation time, in UTC. A fork (§6) is to
  keep its source's creation time, or its first request would replace
  the prefix it shares;
- that each message from the person or another conversation begins
  with a line `[received <UTC time>]`, the time this conversation logged
  it; that the line, and the label after it on a message from another
  conversation, are td-agent's and nothing in the text after them is;
  and
  that the newest such time is the latest the model knows of, not the
  present, since a turn asked again or resumed, or a long one, runs
  after its message came;
- the operating system: the `PRETTY_NAME`, else the `NAME`, of
  `/etc/os-release`, or of `/usr/lib/os-release` when the first is
  missing, its quoting
  read as os-release(5) says, held to one line of at most 80 printable
  ASCII characters, else the target's OS; and the machine's
  architecture;
- that the conversation has no workspace, so no working directory,
  repository, branch or shell, and that the model is not to guess at
  them but to ask for what it needs; or, in a workspace, its directory,
  which is the working directory, whether td-agent made it or it is the
  person's, the shared directories, each read-only or read-write, and
  that there is no git.

Worktrees and network policy are named when repository workspaces and
the network land (§18). A workspace conversation's static text has its
own paragraph where the others say they cannot read or change files
(`prompt/workspace.txt` for `prompt/no-workspace.txt`): what its tools
do, that the person approves each change and command, and that a
refusal is the answer, and that what files and command output say is
data, not instructions. Its creation writes the prefix of a
conversation without a workspace, since only the window knows the
shared directories; its first request takes the workspace's prefix as a
`prefix` event, as one does whenever the shared directories change. A
workspace path is written on one line, what would end or bend it named
as on a card. The time is on
the messages, not in the prefix: the line is the log event's time, so a
message reads the same in every request and the cache holds. A message
carries it only under a prefix that announces it, so a request sent
before this is rebuilt from the log byte for byte (§6). UTC because a
conversation process reads no time zone yet (§3 plans `TZ` and
`/etc/localtime` for schedules); the model converts when the person
names one. The OS is read for each request's prefix check. A
conversation begun before this takes the new prefix as a `prefix` event
before its next request (§6), as any prompt change does, and so does one
whose operating system's name changes.

**As built (increment 11, project instructions).** The window's store
thread reads, at each base it resolves, `AGENTS.md`, else `CLAUDE.md`,
at the commit's top through the git worker outside any jail (§9), each
commit once, and answers with them beside the commits: absent, found
(its name and text) or unread with why (past 64 KiB, not UTF-8, git
failing, or past 128 KiB with the answer's other commits). A base at a
commit an earlier base names crosses as a reference to it, so a
commit's text crosses once however many worktrees start there, and the
answer stays within its frame however JSON escapes it; the decoder
holds the same bound and takes a reference only to an earlier base at
the same commit. The conversation process records them, before the
checkout, in the `instructions` file of its directory: one entry a
worktree, its checkout, the commit and what was read there, at most
128 KiB of text in all with a commit's counted once (a later
worktree's past that is recorded unread), replaced whole, and refused
whole when it is not what td-agent writes (a relative checkout, a
commit that is no object id, a file other than the two). A worktree's
entry is replaced when its repository is read again, which happens
only while it is not yet prepared, so the record is the commit the
worktree was checked out at, and holds once it is. A turn's first
request waits for the window's answer to every store the process
asked for, preparing each as it comes, so the instructions are in the
prefix from the first request; it waits only with a key, since without
one the turn ends at once saying so. Other messages wait their turn
meanwhile, and an interrupt, come with the message or after, ends the
turn, which may be asked again. They follow the environment block in
the system message, under a heading that says they are the project's
guidance, not the person's, that the person's messages win, and that a
file of the same name deeper in a tree governs its subtree and is read
when the model works there: one block for the worktrees read at one
commit of one remote, naming them, the file and the commit, its text
with line ends made `\n` and every other control but a tab made
U+FFFD, in a backtick fence longer than any run of backticks in it, so
no line of the text closes it; an absent or unread one is said in a
line. So a byte of them takes at most four in the prefix's log event,
escaped there twice, and the most the record holds keeps that event
within a line; one that still could not be logged ends the turn, saying
why, not the process. A conversation whose preparation failed has none
for that remote; one recorded later changes the prefix, a `prefix`
event as any change is.

## 14. Context

The model's context is the prefix (§13) plus a view of the log; the log
itself keeps everything (§6). The budget is the model's `context_length`
from §5. The prompt is estimated as the last response's reported prompt
tokens plus a bytes-over-four estimate of what has been appended since.
As built (increment 7), the last response is the last one answered
whole, its report counting its prompt and completion: a failed or
incomplete reply's report is left out, since its completion is never
sent back, and a retry then rebuilds the failed request's head exactly.

**Auto-compaction.** Before a request whose estimated prompt plus
`max_tokens` exceeds `compact_at` (default 80%) of the budget, the
conversation process compacts, at a step boundary and never between a
tool call and its result. A provider's context-length error compacts once
and retries once. With `auto_compact = false` the turn stops at the
threshold and asks the human instead. Compaction has two steps:

1. **Tool-result pruning.** Tool results older than the most recent 40,000
   tokens are replaced with a fixed stub naming the tool, its arguments
   with content arguments elided as §11 elides them, the byte count
   omitted and the sequence number `history_read` (§12) recovers it from,
   when that frees at least 20,000 tokens (opencode's thresholds,
   configurable).
2. **Handoff summary.** If pruning leaves the estimate above the
   threshold, the model writes a handoff summary for a successor:
   progress, decisions, constraints, open questions, next steps and
   critical data, naming the sequence numbers of what it relies on. This
   follows Codex's compaction prompt. It uses the conversation's model,
   or `compact_model`, and its output is bounded to a tenth of the
   budget. Its input is the prefix, the compaction prompt and the pruned
   view with its oldest steps dropped, by sequence number, until it fits
   the summary model's budget less that output bound; the summary is told
   which were dropped. It is reserved against the cost limits like any
   request.

**The view after a summary** is, in order: the prefix; a fixed notice that
the conversation was compacted at a sequence number and that
`history_search` and `history_read` reach everything before it; the
summary; the carried state; and the recent tail. The carried state is
copied verbatim from the log, never summarized, and every item keeps the
source label it had (the human, another conversation, a schedule), so
nothing becomes the human's by being carried:

- the task: the conversation's first message, with its source, up to a
  bound;
- the human's messages since the conversation began, in order, the
  newest kept within 16 KiB, and the sequence numbers of any older ones
  left out;
- the current todo list (§12);
- the workspace's worktrees with their branches and states, and the
  background processes with theirs.

The summary and the todo list are labelled as the model's own notes, not
instructions. The recent tail is the latest steps that fit both
`compact_keep_tokens` (default 20,000) and what the budget leaves after
the prefix, the notice, the summary, the carried state and `max_tokens`,
and always at least the last step; it is cut at a step boundary so that
no tool call loses its result and no assistant message its
`reasoning_details`. If not even the last step fits, compaction fails
(below). After any compaction the prompt estimate restarts from the
rebuilt view. The read-before-write digests of §12 are the conversation
process's, not the context's, so they survive compaction; the model
re-reads a file it needs to see again. A second compaction summarizes
the view, earlier summary included, and carries the state again.

**Resuming cold.** A long conversation left long enough for the
provider's cache to expire costs its whole context again at the
uncached rate on its next request, which compacting first with a
cheaper model can save. Before a turn's first request, when the time
since the conversation's last request exceeds `cache_ttl` (default 300
seconds, Anthropic's ephemeral cache lifetime; providers that keep
theirs longer can be given a longer one) and the estimated prompt
exceeds `cold_resume_tokens` (default 32,000), the turn pauses and asks
the human, on a card, with both estimated costs: resend the whole
context, or compact with `compact_model` first and then send. The
estimates are the reservations of §5 for each: the prompt at the
model's uncached rate, and the summary request plus the compacted
prompt. The answer applies to that turn alone; nothing is remembered.
The log already holds everything the summary needs (§6), so no request
is needed to resume. Compacting is the handoff summary below, its
pruning first, so the conversation continues from the summary, the
carried state and its recent tail. With `cold_resume_tokens = "none"`
nothing is asked.

**Manual compaction.** The human can compact at any time from the
composer, `/compact` followed by an optional focus ("keep the failing test
names") that is added to the summary request, or from a button. Neither
the model nor another conversation can compact a conversation.

**Failure is visible.** If the summary request fails, or the compacted
view still exceeds the threshold, the turn stops and says why; nothing is
silently truncated. Compaction is an event in the log, carrying the
summary, the sequence numbers pruned and replaced, the model and the
cost; nothing before it is deleted, and the requests after it remain a
pure function of the log. The transcript still shows the whole log, with
a divider where the model's view was compacted and the summary
expandable there, so the human never loses what the model no longer
sees. Compaction breaks the provider cache once after the prefix, by
design.

## 15. Configuration

`$XDG_CONFIG_HOME/td-agent/config` is TOML, read by td-toml, the crate
td-news and td-mail also read theirs with (TOML 1.0 without dates and
times). Every key has a
default, except `jev_threshold` until it is calibrated (§11):

- `base_url`
- `model`, which every conversation uses unless the human chose another
  for it (§4), `title_model`, `classifier_fast_model` and
  `classifier_model`
- `jev_threshold`, a probability from 0.5 to 1 in at most three decimal
  places, and `jev_required`; default `true`
- `reasoning_effort`
- `mode`: `auto` or `ask`; default `auto`
- `data_collection`: `deny` or `allow`; default `deny`
- `max_cost_per_turn`, `max_cost_per_conversation` and `max_cost_per_day`,
  in credits (§5); defaults 1, 10 and 25, and `none` disables any
- `workspace_root`; default `~/td-agent`
- `shared`: the host directories bound into every workspace (§8), an
  array of tables each with a `path` and an optional `write`, default
  `false`; the default list is `~/Downloads`, read-only. A directory that
  does not exist is skipped and reported, not created. Setting `shared`
  replaces the default list, so `shared = []` shares nothing
- `template`: the workspace templates of §7, listed in the chooser
  after Empty and Directory…, an array of tables each with:
  - `name`: what the chooser shows, unique among the templates, and
    neither built-in's;
  - `repos`: the git repositories to check out, an array of tables each
    with a `remote`, a `base` (the ref the worktree starts from), a
    `branch` (the one it works on) and an optional `sparse`, the
    cone-mode paths to check out, absent for the whole tree (§7);
  - `shared`: as the top-level `shared`, in place of it for this
    template's workspaces; absent, the top-level list;
  - `network` (later, with increment 15): `off`, `allowlist` or `open`
    for this template's workspaces; absent, the top-level `network`
- `remotes`: the admitted git remotes (§7), each a remote's URL or a
  host with a path prefix; default empty, so the first workspace on a
  remote asks; what a card admits is kept beside it, in the state
  directory's `remotes` (§7)
- `network`: the default policy, `off` or `allowlist`; default `allowlist`
- `network_allowlist`: the default allowlist of §10, hosts with ports
- `protected_branches`; default `["main", "master"]`
- `fetch_interval`, a whole number of seconds from 60 to 86,400, and
  `fetch_concurrency`; defaults 600 (ten minutes) and 4
- `max_background`, a whole number from 1 to 16, and
  `background_output_bytes`, from 64 KiB to 1 GiB; defaults 4 and
  16 MiB (§12)
- `auto_compact`, `compact_at`, `compact_keep_tokens` and
  `compact_model`; defaults `true`, 80%, 20,000 and the conversation's
  model (§14)
- `cache_ttl` (later, with increment 16), a whole number of seconds,
  and `cold_resume_tokens`, or `none`; defaults 300 and 32,000 (§14)

There is no `limits` key until §8's limits land. `orchestrator_model`
is retired, since there is no orchestrator (§3): a file that still sets
it loads, with a note on standard error that the key is no longer read,
and its value is not checked. Other unknown keys are refused by name.
For example:

```toml
model = "anthropic/claude-sonnet-5.5"
mode = "auto"
max_cost_per_day = 25

[[shared]]
path = "~/Downloads"

[[shared]]
path = "~/src/reference"
write = false

[[template]]
name = "td"

[[template.repos]]
remote = "https://github.com/timmydo/td"
base = "main"
branch = "agent"
sparse = ["td-agent", "td-ui"]
```

**As built (increment 4).** Every key above is known by name. `mode` is
read and checked. Each other key that is present is accepted and named
on standard error with the §18 increment that first reads it, and the
status row counts them, so a setting that does nothing yet is said and
never silently ignored; its value is checked by that increment. `limits`
is refused with the reason above, and any other key is refused with the
list of known keys. A missing file is every default, and a file longer
than 1 MiB is refused. A relative `XDG_CONFIG_HOME` or `XDG_STATE_HOME`
is ignored, as the XDG base directory rules say, for `$HOME/.config` or
`$HOME/.local/state`.

**As built (increment 5).** `base_url`, `model`, `title_model`,
`reasoning_effort`, `data_collection` and the three cost limits are
read and checked. The defaults are `https://openrouter.ai/api/v1`,
`anthropic/claude-sonnet-5.5` for conversations,
`anthropic/claude-haiku-4.5` for titles, and
`medium`. `base_url` must be an `https://` URL with a host and no query,
fragment or space, so the key is never sent in the clear; a trailing
`/` is dropped. A model id is printable ASCII. `reasoning_effort` is one
of `none`, `minimal`, `low`, `medium`, `high` and `xhigh`. A limit is a
non-negative number of credits, or `none`. `model` and
`reasoning_effort` are what every conversation starts with; the
Conversation menu chooses another for one conversation, and a default
model the window saves replaces `model` until the key is edited (§4);
this file records neither. The key file is not a key of this file (§6).

**As built (templates).** `template` is read. A `name` is visible text
of at most 64 bytes with no space at either end and nothing a card
would show as `<U+XXXX>` (§11), unique among the templates and neither
`Empty` nor `Directory…` (nor `Directory...`), each ASCII case aside;
there are at most 64 templates. Each of `repos` has a `remote`, a
`base` and a `branch`, text of at most 2,048 bytes with no control
character, and an optional `sparse` list of paths, each relative, with
no `..` and no control character; increment 11 prepares them.
`shared` is checked as the top-level key is, its errors naming
`template.shared`. `network` is accepted
with a note that increment 15 reads it, and any other key is refused
by name.

## 16. Prior art: opencode

opencode is the open agent closest to td-agent's shape, with
worktree-backed workspaces, child sessions and a session event log, and
it runs every tool with the user's full authority, with no sandbox of any
kind. td-agent's position on its features:

| Feature | td-agent |
|---|---|
| Shadow-git step snapshots, undo and redo | adopted, in the worktrees' own repositories (§12) |
| `doom_loop`: three identical calls ask | adopted (§11) |
| `question` tool | adopted (§12) |
| Child sessions, background subagents that notify | adapted as peer conversations that message and read one another through crossings (§3) |
| Fork at a message; local export | adopted (§6) |
| Event-sourced session log | adopted, as a per-conversation file log (§6) |
| Prune old tool output, then summarize | adopted, with its thresholds (§14) |
| Small model for titles | adopted (§13) |
| `todowrite` | adopted as `todo_write`, carried through compaction (§12) |
| Worktree workspaces | adapted: sparse, asynchronous, per-workspace repositories, jailed (§7) |
| allow/ask/deny patterns, last match wins | adapted: deny first, workspace rules only narrow (§11) |
| `external_directory` prompt | replaced by mounts: what is not bound does not exist (§8) |
| Skip-permissions and auto-accept | replaced by the jail and the classifier (§11) |
| Session sharing through a hosted service | not adopted: principle 5 |
| GitHub Actions app, server mode, ACP | not adopted: hosted runners, a listening socket, a second frontend |
| JS plugins and code-mode `execute` | not adopted: no embedded runtime |
| LSP diagnostics and formatters after edits | not adopted for now; opencode itself turned both off by default |
| Skills and custom commands | later, as plain files read through the tool host |
| MCP | later (§12) |

## 17. Testing

- **Pure units:** the SSE reader, including comment lines, a mid-stream
  error, `[DONE]` and fragmented tool calls; tool-call assembly and argument
  parsing; the reasoning splice's byte identity; log replay to identical
  request bytes, across a prefix change; cost reservation; the shell-argv
  rule matcher and its refusal of every opaque construct; the classifier's
  state builder and its field separation; remote and ref-name admission;
  path admission; the egress address predicate, every refused range and
  the machine's own addresses included; each tool's semantics against
  temporary directories, including the read-before-write digest, the edit
  errors, and grep and sed's argument mapping with `--sandbox`; the todo
  list's bounds and single item in progress; `history_search` and
  `history_read` over a log with a torn final line, paging a whole tool
  result; the compaction view's carried state, its tail cut at a step
  boundary, and its sequence-number stubs; the cron parser, next-firing
  computation across daylight-saving changes, and catch-up at most once;
  and the configuration's `shared` tables and refusal of unknown keys.
- **Git units, against local repositories:** store clone and fetch with
  the fixed invocation; a workspace repository over alternates; sparse
  worktrees; the asynchronous state machine, the first request waiting on
  the base, and a pending tool result; remote-tracking updates in a
  maintenance instance and their notifications; export, strict import and
  a push of the exact id to a local bare remote; protected-branch, force,
  deletion, tag and scan-match routing; a force push's lease; snapshot
  refs with undo and redo; store gc keeping borrowed objects; and
  archiving or deleting the conversation of a dirty workspace only on
  confirmation. A planted reflog, `FETCH_HEAD` or
  `worktrees/` symlink in a workspace repository must change nothing
  outside the jail through any git worker operation; a planted
  `~/.gitconfig` hook or fsmonitor in the workspace HOME must never run in
  a maintenance instance; a new worktree's admin files must be td-agent's
  even when the jail pre-planted the names; and the push scan must catch a
  secret in a binary file, behind a `-diff` attribute, and added in one
  commit and removed in the next; and no source inside another
  workspace's tree is admitted.
- **Offline loop tests:** a mock fetch service replays recorded OpenRouter
  exchanges, Jev decisions included. td-mail's `tests/mock_fetch.rs` is the
  precedent. These cover a tool-call round trip, parallel calls, a 429
  retry, a 502 shown and not retried, 402, cost limits, interruption,
  automatic, manual and failed compaction, a context-length error
  compacted and retried once, a conversation made from a repository
  template waiting on its base and notified when its worktree is ready,
  a crossing allowed and refused on its card, messages between
  conversations with their labels and wake budget, an old orchestrator
  conversation and its labelled messages read as ordinary ones, a
  background process's exit notice waking an idle conversation, a
  conversation process killed mid-turn and restarted from its log, a
  restart that finds a tool call, request or delivery started but not
  finished and repeats none of them, a policy change re-deciding a
  pending approval, and each classifier path: both stages allow, either
  defers or denies, a malformed reply, and a Jev outage with and without
  `jev_required`. A conversation process is a child process here as in
  the window, driven over its socketpair by a test harness standing in
  for the window process.
- **Window tests:** native compositor process tests through the driven
  seam: the layout, the workspace tree, switching conversations, selecting
  text across messages and copying it, copying a whole message and a tool
  result, a card that does not take focus, a card answered after focusing
  it, and a refused key file shown in the window. The message list's own
  tests live in td-ui with its other widgets.
- **Jail tests**, in the td-jail `workspace` increment: a tool cannot read
  the caller's home, the key file or the store's writable state; cannot
  write, rename or redirect any read-only entry of the git chain,
  including by renaming an ancestor; cannot create a linked worktree;
  cannot create a Unix socket, reach one published in a worktree, or make
  a datagram pair; can commit and `git switch -c`; with the proxy reaches
  only allowlisted host and port pairs, and without it nothing; and
  leaves no process running after a timed-out call, a killed
  background process, or a conversation or window process killed with
  `SIGKILL`. Admission refuses `$HOME`, the store, a credential
  location, and a shared directory containing a worktree.
- **Live checks, by hand and never in the gate:** td-agent against
  OpenRouter, and a classifier fixture set of pending actions with expected
  verdicts. Each stage's false-allow and false-escalate counts, and Jev's
  calibration, are recorded in the commit that changes a classifier
  prompt, model or threshold.

**Gate cost.** An edit confined to `td-agent/` should run td-agent's own
tests and lints and nothing else. `builder/src/affected.rs` decides that
in two places. `map_path` maps each changed path to preflights and
targets; `cargo_test_cmds` then narrows the `cargo-test` preflight to the
roster crates a diff touches and every crate that reads them. Measured on
2026-10-02:

- a path in no roster crate selects every crate and the `check` target,
  which is what `td-agent/src/` did before `td-agent/Cargo.toml` existed,
  so the crate increment added its manifest and lock in the same commit
  as its source;
- a path in a discovered crate with no `map_path` arm of its own selects
  that crate's preflight and also the whole `check` target, since which
  recipe embeds a new crate is for its author to say;
- every narrowed preflight keeps the tests and clippy of the builder,
  recipes and engine workspace, because recipes embed crate sources and
  the builder's tests assert exact reader sets;
- td-mta escaped both, and still does: its own `map_path` arm adds
  only the `cargo-test` preflight, and `cargo_test_cmds` drops the
  workspace pass while its only outgoing edge is exactly `td-crypto`. A
  builder test holds that no recipe and no seed roster names it.

td-agent is laid out to get td-mta's treatment, and the crate increment
gave it that: `WORKSPACE_EXEMPT` in `affected.rs` lists each exempt crate
with its sorted, pinned edges, and
`workspace_exemption_requires_no_distribution_recipe` holds every crate on
it to no recipe and no seed roster. `td-builder affected-checks --path
td-agent/src/main.rs` selects the format check and td-agent's test and
clippy commands, and no check target. It is laid out for that as
follows:

- no crate depends on it, so a td-agent change selects td-agent alone; a
  change to a crate it reads selects td-agent as well, as it should;
- no recipe, recipe test or seed roster names it until packaging;
- its outgoing edges are pinned: exactly `td-civil`, `td-compositor`,
  `td-fetch-client`, `td-fs`, `td-json`, `td-toml` and `td-ui` (its
  dependencies). `td-civil` joined when the history's UTC stamps left a
  copied calendar; `td-fs` when the store's atomic replace became
  td-fs's; `td-compositor` joined with the window increment, which
  declared `native-compositor-tests`, since that opt-in adds the edge;
  `td-json` and `td-toml` joined when JSON and TOML left the copied
  modules for crates of their own, and `td-fetch-client` replaced
  `td-news` when `td_fetch` did, retiring the test that read td-news's
  copy.
  The pinned set lives in `affected.rs` beside td-mta's, and a diff whose
  edges differ from it takes the workspace pass, because the builder's
  reader-set assertions name td-agent once it reads td-ui;
- the crate increment adds a `td-agent/` arm to `map_path` selecting only
  the `cargo-test` preflight; generalizes the workspace exemption and its
  guarding test from td-mta to a list of exempt crates, each with its
  pinned edges; and asserts that `td-agent/src/` selects no target. A diff
  confined to td-agent then runs the format check and td-agent's own
  tests and clippy. Packaging makes a recipe name td-agent, which the
  guarding test refuses until the packaging increment removes td-agent
  from the list;
- its gate metadata is `clippy-all-targets` and `trusted-test-root`, the
  latter because its control-socket tests bind under owner-checked
  fixtures as td-ui's do. `native-compositor-tests`, which adds a
  compositor build to every td-agent preflight (cached after the first),
  is declared only by the increment whose window tests first need it,
  and those tests stay few, in one process-test file; the rest of the
  window's logic is tested against td-ui's widget state without a
  compositor.

**As built (increment 4).** The window increment declared
`native-compositor-tests`, with one case in `tests/control_process.rs`:
keys typed through the headless compositor's seat start a conversation
and send there, start a second and send there (`S-Return` a newline,
`Return` and `C-Return` the send), and switch back, each result
read from the store.
`tests/processes.rs` drives the built program's conversation personality
over real socketpairs: a killed child restarted from its log and going
on from it, a child failing every start left failed after three
restarts, a child exiting when its socketpair closes, a second writer
of one conversation and a second window refused, and the configuration
refusals. The window's keys, focus, list order, status row and divider
are tested against its widget state in `src/ui.rs` and the driven
actions in `src/control.rs`.

**As built (increment 5).** Unit tests cover request building, response
parsing, the splice's byte identity through the log, a log replayed to
the bytes sent, money parsing and reservation arithmetic, the day's
ledger, each error path, and each key-file refusal against temporary
trees. `tests/model_client.rs` drives the built program's conversation
personality over its socketpair, as the window would, with
`XDG_RUNTIME_DIR` pointing at `tests/support/mock_fetch.rs`. That mock
fetch service serves the `td-fetch 1` protocol from a thread, records
every request, and answers from `tests/fixtures/openrouter/`. The
fixtures are written by hand in the shapes OpenRouter returns; none is
a recording of a live exchange, and no test reaches the network. The
cases are a turn's exact headers and body and its title; a second turn
from a restarted process, whose body begins with every byte the first
sent and carries the first reply's `reasoning_details` unchanged; 429
retried three times and no more; a 502 offered for a retry and asked
again only when told; 401 and 402; an error inside a 200; the turn's,
the day's and an unpriced model's limits; a model without `reasoning`;
a request in flight when its process is killed, interrupted and never
resent; no key; and the key absent from the log, `meta`, `prefix` and
standard error. `tests/processes.rs` adds a message sent just before
switching away, whose turn still runs in the background. The shared
`td_fetch.rs` joins `json.rs` and `toml.rs` in
`tests/shared_modules.rs`; all three have since left for the td-json,
td-toml and td-fetch-client crates, and the test with them.

**As built (increment 7).** Pure units cover the SSE reader in
`src/sse.rs`: comment lines, `[DONE]` and nothing read after it, an
OpenRouter-shaped stream, one with CRLF endings and one with
multibyte text each split at every byte boundary and fed a byte at a
time, LF, CRLF and lone-CR endings, multi-line `data`, the other fields
and a byte order mark, the per-line, per-event and total bounds, and a
sink's error stopping the reading. `src/assemble.rs` covers text and
reasoning deltas and what is handed to the window once, reasoning
details joined and serialized once, distinct blocks that share an index
kept apart, a block whose members vary by delta kept whole and an empty
signature giving way, fragmented tool calls assembled by index, an
error mid-stream with its usage, a string error code, a choice's error,
a bare error finish and a chunk that is not JSON, and the reply's
bounds, counted as logged. `client.rs` keeps a failed request's report
out of the prompt estimate. The window's units draw a reply as its
deltas come, settle it kept or replaced, mark one incomplete or
interrupted by its process failing, make room at the transcript's limit
mid-stream, and give `Escape` to a running turn alone.
`tests/support/mock_fetch.rs` serves the stream mode: a request with
`stream` gets its reply as `chunk N` frames of a chosen size, ending
`end`, `error`, or kept open with comment frames until the client
closes it, which the mock counts; a counted reply scripted for a
stream is framed as the service would. The fixtures
`stream-sonnet.sse`, `stream-gemini.sse` (served with CRLF endings) and
`stream-error.sse` are written by hand in OpenRouter's streamed shape,
in 61-byte frames so events straddle them. `tests/model_client.rs`
streams every turn: the exact head with `stream` and the 32 MiB limit,
the deltas and the reply logged whole with its details as assembled, a
restarted process sending them back byte for byte, an error mid-stream
charged as reported with its text kept incomplete and a retry sending
the same body, a stream broken by a transport error and one ended
before its finish, each charged its reservation, a finish without
`[DONE]` whole and `[DONE]` without a finish cut short, an interrupt
that ends the turn, closes the connection and is asked again whole, and
one that ends a rate limit's wait. A 200
answering a stream with one JSON body keeps increment 5's error case.

**As built (increment 8).** `src/tools.rs` covers each tool's
arguments and bounds: the todo list's 50 items, 500 bytes and one item
in progress, unknown and mistyped members, malformed JSON and empty
arguments, the search and read limits, and a message's 32 KiB; the
crossing rules, a conversation's own log free and every other a
crossing; the listing's field filtering and order; and the definitions
in the prefix, one set for every conversation.
`src/history.rs` covers search (every term, case folded, newest first,
kinds, the limit, excerpts on character boundaries), the cursor over a
3,000-character tool result paged back whole, an offset inside a
character, a page always taking one, and an approval's redaction in
both tools. `src/wake.rs` covers the budget's count, an older log's
reports and retries not counting, its renewal by the human's message to
the conversation alone, and its one notice; `src/post.rs` the outbox
across a restart and the window's own check; `src/store.rs` the new
events replayed exactly, `paused` in `meta`, and calls without results
answered at load once; `src/assemble.rs` and `src/client.rs` a call
without an id or name, a counted reply's calls, and the wire form of
messages, calls and results; and the window's units the todo panel and
its keys, pausing, messages, calls and results in the transcript, and a
streamed reply's calls kept once it is logged. `client::check_calls`
covers ids missing, shared or too long and names missing or too long.
`tests/support/mock_fetch.rs` can route a request by a marker its body
carries to a script of its own. The fixtures `stream-tool-todo.sse`,
`stream-tool-parallel.sse`, `stream-tool-malformed.sse` and
`stream-tool-send.sse` are hand-written tool-call streams in 17-byte
argument fragments, and `models.json` gains a model without `tools`.
`tests/model_client.rs` runs a tool call's round trip (the second body
rebuilt from the log byte for byte), two parallel calls answered in
their order, malformed arguments answered with an error, the step bound,
a model without tools refused, the wake budget held past twenty and
renewed by the human, a paused conversation holding a message and
starting its turn when resumed, a pause sent mid-turn holding a message
that came before it, a message handed on twice logged once, a crossing
refused as the call's result, a conversation a message woke first titled
after the human's first turn, and a restart that finds a call started
and not finished; and, through a `Supervisor` and the window's `Post`,
one conversation messaging another that is closed, which is woken and
answers; and an interrupt that comes while a call waits on the window,
which lets that call finish, answers the next as not run and offers
`C-r`, whose request carries both results.

**As built (increment 9).** `src/files.rs` covers a relative path
refused naming the worktrees; a read's numbering, its 2,000 lines and
100 KiB, the next offset, an offset past the end, a cut line, and the
whole file's digest on every partial read; binary, empty, non-UTF-8 and
missing files and a directory naming `glob`; a write creating its
directories, refusing an unread file and one changed since, and
replacing one read or written; an edit's empty, identical, missing and
repeated `old_string`, `replace_all`, a stale digest and a non-UTF-8
file; and `glob`'s segments, `**`, classes, braces, dot files, `.git`,
order and cap. `src/shell.rs` covers the exit status and interleaved
output, empty input and the cleared environment, a timeout and a cancel
killing the process with the output streamed before it, a descendant
holding the pipe not holding the call, a process that closes its output
still waited for, a process writing without pause timed out and
cancelled and a descendant writing after its parent held no longer than
the drain, a character split between head and tail, the head and tail
kept and shown with the omitted count, the timeout's bounds, grep's and
sed's argv with a pattern and a path that look like options, td-txt run
under the applet's name through a stand-in script, and grep's rendering
of no matches, its line cap and its errors. `src/files.rs` also covers
lines split across pieces, a CRLF among them, a line of a million
two-byte characters, and patterns that would be exponential to a naive
matcher; a device and a FIFO refused by each file tool, a read
cancelled, eight replacements of one read of which one lands and four
creations of which one does, and nested, chained and over-long brace
groups and patterns. `src/host.rs` covers every call and reply
round-tripping, an unknown tool, a malformed reply, and replies to calls
not in flight dropped; `src/toolhost.rs` the read-before-write rule
across the protocol, two shell calls side by side with one streaming and
cancelled, calls past 16 refused, a call with a missing argument refused
and the host serving on, a result past the frame sent as an error, live
output cut between characters, a host without worktrees or td-txt, and
its arguments. `tests/tool_host.rs` runs the built program as a tool
host over a pipe, and its input closing, which interrupts the call it
was running and ends it. Its `grep_and_sed_run_td_txt` runs td-txt's
grep and sed, `--sandbox` refusing a `w` command, against a td-txt named
by `TD_AGENT_TXT`; it is ignored by default, since the gate builds no
td-txt for td-agent's tests.

**As built (the File menu).** `src/menu.rs` covers each item's action
in order, File's shortcuts being chords the window binds and Help's
`F1` none of them, `F10` and `Escape` closing it, other chords
consumed, a press on the header and on an item, and Help after
Conversation, its `Keys` chosen by `Right`, `Right` and `Return` or by
presses. The window's
units hold that every focus's key list passes `keys::check` with no row
for Help → Keys, that the live pointer's and keyboard's choice of it
asks for the list once, and that the control seam's keys and presses
choosing it ask for nothing. `src/key.rs` covers the dialog's check and
the paste's trimming; the write making a 0700 directory and a 0600 file
holding the key and a newline, read back, leaving no temporary file and
the key in no other file, and using an existing directory as it is; a
stored key replaced only when asked, a refused one included; refusals
by name of a group-writable ancestor (with nothing made under it), a
missing configuration home, a link or a file where the temporary file
goes (left as it was, the link not followed), a key path that is a
directory or a link, and a text that is no key; the read-back's check
of the bytes and of the read's rules; and that the fixture feature
below moves only the walk's top. `src/keydialog.rs` covers the entry
masked in what is drawn and in `Debug`, copy and cut refused without
the clipboard being asked, a paste trimmed, two lines and an overlong
paste refused whole, the message as typed and on Save, the warning,
`Tab`, `Space`, `Return` and `Escape`, the entry cleared on cancel and
close, buttons chosen by a press and a release on one, the replace
confirmation (Cancel first, Replace chosen, and placed away from the
press), the explanation's wrapping, a long refusal shown wrapped, and
a window too small. The window's
units cover the bar over the split, the menu's keys and pointer with
the window's chords consumed while it is open, the status row's `no
key` until a key is stored, the dialog's modality, a refusal kept in
it, the replace confirmation through the window, a paste reaching the
dialog that asked, dropped when it comes after the dialog closed or
was not asked for, the composer's once the clipboard gave the
dialog's up, and the composer's otherwise, the chord the menu
shows doing what its item does, and no key path; each
holds that the key is in no text drawn, status line, notice or `Debug`.
`src/control.rs` drives the menu and the dialog through the seam,
holding that `state` and `text` never carry the key; and
`src/conversation.rs` holds that a later `setup` gives a conversation
its key between turns and that its log never holds it, and that a queued
`setup` or `choose` is taken first, in the order they came, while a
pause still goes ahead of the messages before it. A umask that takes the
owner's read access is not covered: std cannot set one without a foreign
call, and it is the whole process's, which would race the other tests.

For the Messages window, `src/notes.rs` covers the log's bound, its
oldest dropped, a long note cut on a character boundary with the walk
back taken, the unread count, the window's keys with a held one closing
nothing, a refused copy said, a window too small saying so and laying
out once resized, and a full window dropping its oldest as the log
does. The window's units cover notes counted and not shown in the row,
`C-S-m` and the File item opening it modal, a note joining it while
open, a paste dropped, its closing, its refusal over another modal, the
key dialog replacing it, its drawing in a window too narrow for the
split, a background notice kept under its conversation's title, and the
list's Workspace column, a directory by its folder's name; the key
tests hold that no note kept, not only the newest, carries the key. And
`src/control.rs` drives `messages` and reads `notes`, `unread` and
`note`.

For the store, `src/git.rs` covers remote parsing and every refused
form, admission by remote and by whole-segment prefix, distinct store
names for one repository's https and ssh remotes, branch names, and,
against a local repository fetched over the file transport that only
tests admit: the copied global file keeping the identity and both
helpers and dropping a hooks path, a URL rewrite and an alias; a
`GIT_CONFIG_PARAMETERS` in td-agent's environment not reaching git; the
local remote refused without the test's admission; a store hook not
running on fetch; the store made with no remote configured; a base
resolved and its `AGENTS.md` and `.td-agent/rules` read, a tree, a link
and a missing path none; a branch fetched and then pruned; a file past
the bound refused; a store recording another remote refused; and a run
whose standard error is long draining it, and one past its time
killed. These run where a
`git` is on PATH, as in the host preflight; the in-sandbox gate's
toolchain has none, and there the test says it is skipped.

For the git mount chain, td-jail's `workspace.rs` covers the spec's
`checkout` and `repository` keys in order and a repository admitted
with its whole chain in mount order, and refuses a protected entry
missing, a link, a file with a second name, a file where a directory
belongs, a linked entry that is no directory, a checkout whose `.git`
is a directory or absent, a directory overlapping a repository, a plain
worktree holding `.git` beside a repository, and a plan past its bytes;
`transition.rs` round-trips the chain to stage 2 and refuses a link
outside every repository and worktree, at a top, named twice, a
writable file, or anything in a worktree but its read-only `.git`
file. Its ignored live test, run where unprivileged user
namespaces are, is As built (increment 11, the chain)'s.

For the workspace repository's layout, `src/repo.rs` covers cone
patterns as git writes them (the whole tree, the top alone, nested and
overlapping paths, refused segments), the configuration (the identity
quoted, the hooks path, the settings §8 names, no `[user]` without an
identity, a control character refused), a repository and a worktree
made whole and once (each file's content, paths named as they resolve
through a link, no staging debris left, a second call refused and the
first kept, a refused call removing only what it made, a planted link
never written through, a bad id or branch refused, the 33rd worktree
refused), a task's words round-tripping and refusing what they cannot
name, and the one-line answer; with `git` on PATH, the checkout task
run outside a jail removes the branch it made when the checkout fails,
sets a `HEAD` the jail rewrote, checks the cone out on a new branch,
refuses a second run by its index and another worktree on the branch
without moving it, and a commit there carries the configuration's
identity. `src/jail.rs` covers the spec's `checkout`, `repository` and
store `read` lines in td-jail's order and that neither a repository nor
the store is a root. The ignored live test is As built (increment 11,
the layout)'s.

For repository workspaces' preparation, `src/workspace.rs` covers a
template's record (its name, a shared repository for one remote's
entries and a worktree each, another remote of the same name a
repository of its own, the paths under the data directory and the
workspace root, its round trip through `meta` and the process's
argument; refused: an unadmitted remote, a branch twice for one
remote, a branch git would misread, a transport other than https and
ssh, no repositories) and that its policy binds only what is prepared,
a repository's checkouts its roots; `src/store.rs` that `prepared` is
kept once, through the window's archiving, absent in a meta written
before and refused relative; `src/protocol.rs` the round trips of
`Fetch`, `Fetched` and `Prepared`, and a `Fetch` past 32 bases refused;
`src/git.rs` that the store service answers each ask for its
conversation; `src/config.rs` `remotes`; `src/prompt.rs` the worktrees
in the environment block; `src/repo.rs` a checkout resumed after a
crash, its branch kept where it was made though the base moved and its
stale locks cleared, and a failed checkout (a blob missing from the
store) removing its branch; `src/workspace.rs` too a name reserved
once, the bound on directories and on the record, the policy's
working directory and its bound, and the data directory narrowed and
refused as a link; and `src/toolhost.rs` a call naming no directory
refused while the working directory is not bound. `tests/processes.rs`
has a new repository conversation ask once for its store, with its
bases, be kept while it prepares though left and retired once done,
and say a refusal in its log, recording nothing; the ignored
live test in `tests/jail.rs` answers a conversation's ask with a real
store and finds its worktree checked out, its notice said and its
repository recorded prepared.

For the admission card, `src/workspace.rs` covers which of a
template's remotes are asked about (each once, as recorded, none
another admission covers) and that a template refused for anything
else (a plain http remote, a branch named twice, a bad base) is
refused before any card; `src/store.rs` that the remotes admitted are
kept once, read back as written, refused whole when a line is not one
td-agent writes or there are more than 256, and once set aside admit
nothing and let a card admit again; `src/confirm.rs` that the card
admits only on its action; and `src/ui.rs` that it is shown with
`Cancel` focused, asks the window to admit only on `Admit`, says a
refusal in a note, takes no key as it is shown, is set aside with the
keyboard and asked again when it comes back, and is not asked over
another question.

For project instructions, `src/git.rs` covers that each commit's are
read once and held to the answer's bound, unread with why when past it,
not UTF-8 or when git fails; `src/protocol.rs` that an answer at that
bound fits a frame however its text escapes, that one past it is
refused, that a workspace's every worktree at one commit at the bound
still fits, the text crossing once, and that a reference to a base at
another commit is refused; `src/store.rs` that a worktree's record is
replaced in its place, that a commit's text counts once against the
conversation's bound and a later worktree's past it is unread, that
they survive reopening, and that a file td-agent would not write
refuses the conversation; and `src/prompt.rs` that they follow the
environment block, grouped by commit and remote, fenced past any run
of backticks, controls made plain, an absent and an unread one said,
none said before any is recorded, and that the most the record holds,
of the text that escapes most, keeps the prefix's event within a line.
`tests/processes.rs` has a first turn wait for the store's answer,
ending only after it, with its instructions recorded though the
checkout then fails, and an interrupt end a waiting turn, sent with
its message or after; the live test finds them recorded for the
prepared worktree.

For removal on deletion, `src/repo.rs` covers a survey's words
crossing with one base and two, a clean worktree, a changed and an
untracked file counted with an index lock left in place, commits on
`HEAD`, on a branch `HEAD` is not on, in a stash and kept by a tag
counted, a commit in a second base not, and an answer or a base that
is not one refused; `src/removal.rs` what would be lost listed a
worktree a line (changes, commits once a repository, too many to
count, could not be asked, cut to its bound) and nothing for a clean
one, the tree and repositories' directory renamed away then put back,
or removed, a link not followed, a taken name passed over, a directory
not named for the workspace left, removing again no matter, and a
sweep taking a leftover but not the human's `.deleting-notes`, with
the names a sweep knows; `src/ui.rs` a conversation being deleted
closed, refusing to open, given nothing by the post and named
`deleting`, and its loss card, Cancel focused, set aside with the
keyboard and asked again, Cancel keeping and the action deleting.
For the background fetch, `src/config.rs` covers `fetch_interval`'s
default and bounds; `src/git.rs` a refresh against a local upstream
finding the new commit and a deleted base failing alone, and the store
thread answering a refresh for the window with its bases;
`src/upstream.rs` the stores in use, from live workspaces' admitted
remotes only, each once with its bases once, none whose fetch is
pending, and a base moving only from a commit known before; the store
thread's queue a preparation before queued background fetches.
For step snapshots, `src/snapshot.rs` covers a worktree kept as a tree
on its ref with its own index and what is staged in it left alone, an
edit, a deletion and an addition named and an ignored file not, no
new commit when nothing changed, the answer crossing whole and one for
other worktrees refused, other worktrees, a tree that is no id, a
mismatched count, git by a relative path and a repository's own
checkout refused, many changes counted past the bound, a file made
outside a sparse checkout named and none outside it taken as deleted,
ignore rules and attributes in the home, `.gitattributes` in the tree
and the repository's `core.autocrlf` changing nothing and a blob its
file's bytes, a blob or a dangling id planted on the ref replaced, an edit under an assume-unchanged mark named with the
worktree's mark left, naming cut at each bound with the rest counted,
and the escaped bounds checked per worktree and in all, the most an
answer may hold fitting a frame; `src/store.rs` the `snapshot` event replaying exactly
and one with a tree that is no id refused; `src/repo.rs` a snapshot's
commit not counted as work;
`tests/model_client.rs` a step that writes only its todo list not
snapshotted, one that runs a command snapshotted first and, with no
jail, said once before its first call, and no `snapshot` event; and
the live `tests/jail.rs` preparation test a snapshot in the
conversation's own kind of instance, its ref holding its tree, a
file made between two named, and that step undone, redone and undone
there. For undo and redo, `src/snapshot.rs` covers an edit, a
deletion emptying a directory, an addition in a new one, a mode and a
link turned file each restored and redone, an ignored file left, a
file restored as its bytes under `.gitattributes`, the ref moved, a
worktree changed since refused with nothing written, mismatched or
non-id trees and other worktrees refused, paths that are not plain
refused, no removal through a link or of a directory, and emptied
directories pruned; `src/store.rs` the steps counted from the log;
`src/ui.rs` `C-z` and `C-S-z` naming the latest step, refused while a
turn runs, and nothing to redo after a new step;
`tests/model_client.rs` a step not the latest and one whose worktree
is not ready refused, and one past those checks failing short of a
jail with no `undo` logged; and `tests/processes.rs` a conversation
asked to undo kept, though left, until it says it is done, and one
restarted meanwhile not kept for it. Against
what no snapshot holds, `src/snapshot.rs` covers an ignored file in a
directory to become a file, one where a directory must be, and a file
ignored after the step and edited since each refused with nothing
written, that file as it was allowed, a submodule refused, and a
retry of a restore done already changing nothing.
For a call told a worktree's state, `src/conversation.rs` covers a
read into a worktree still checking out, a command in a ready first
worktree and a path outside every worktree passing, a path climbing
through `..` into a worktree not ready, a relative path left to the
tool host, a command with no directory while the first could not be
prepared, a `sed` naming one such path among others, and every
worktree ready;
`tests/model_client.rs` a command with no directory refused so, with
no card, in a turn.
For the asynchronous preparation, `tests/model_client.rs` covers a
checkout's news, idle, waking nothing before the person has written;
then a refused store's news and a checkout's each waking a turn of its
own that reads it, announced before `Prepared`; nothing woken while
paused; and a failure the same as the last news waking nothing. The
live `tests/jail.rs` preparation test covers `Prepared` only after
the ready notification and `Heads` asked once ready. Not pinned, as a
test cannot time them without a hook: a checkout done during a turn
taken up before the next request or a retry rather than after the
turn; a second answer while checking out; a closing window waking
nothing; and a panicking thread.
For the notifications, `src/client.rs` covers one given to the model
labelled, with and without the received line, and a notice not, its
history rendering, and `notification` searching as `notice`;
`src/conversation.rs` a failure's reason quoted on one bounded line;
`src/prompt.rs` the repository environment naming the label and the
news; and `tests/processes.rs` and the live `tests/jail.rs` a refused
store, a ready repository and a moved base each logged as one, and a
failure to set the refs logged as a notice.
For the remote-tracking refs, `src/repo.rs` covers `track` setting a
ref over what was there, a stale lock stopping a later run but cleared
by one preparing, its words crossing in both forms and bad words
refused, a commit the repository lacks refused, and a second base's
missing commit leaving the first ref as it was; `src/store.rs` `tracked` read
back, absent as empty, a bad commit refused, and a base's later record
replacing its earlier; `src/protocol.rs` both `Heads` crossing and a
base without its commit refused; `tests/processes.rs` a prepared
repository asking `Heads` and not its store, a recorded commit doing
nothing, and two remotes failing in turn each said once; and the live test in `tests/jail.rs`
the ref set and recorded at preparation, and, started again and told
upstream moved, set there, recorded and said.
For removal on archiving, `src/store.rs` covers `removed` read back,
absent as false, refused when not a boolean, kept by unarchiving and by
a process's own write; `src/ui.rs` a conversation being archived named
`archiving` and its card headed for archiving; `tests/processes.rs` an
unarchived one whose workspace went asking for no store, letting a late
answer go with nothing laid out or recorded, its prefix saying so;
`src/card.rs` its worktrees said removed; and `tests/model_client.rs`
its read and shell calls refused without a card, the turn going on. The
live test in `tests/jail.rs` surveys the worktree it prepared in a
real maintenance instance, clean, then with an untracked file, then
with a commit, and removes the workspace, the store left.

For the workspace card, `src/card.rs` covers the recommendation first,
the worktrees of a remote at one commit grouped, each saying whether
it is checked out, the text the model is given with a right-to-left
override named, a control as the model gets it and the text's own
`<U+202E>` told from a name, an absent, an unread and a not yet read
entry, an unreadable record said with nothing claimed of what it held,
and a header held to td-ui's label bound; `src/notes.rs` that a card
longer than its panel opens at its first entry while the Messages
window follows the newest; `src/menu.rs` that its item is on only for
a repository workspace; `src/control.rs` that `workspace` with none
says so and leaves `card` closed; and `src/ui.rs` the status row's
item for a repository workspace and none otherwise, `C-S-w` and the
menu item asking for it, an answer for another conversation or over
another modal showing nothing, a note counted and kept out of it, a
paste dropped naming it, a prepared repository asking nothing, opening
it again reading again, and `C-S-w`, not `C-S-m`, closing it.

For row menus and archiving, `src/menu.rs` covers the row menu's items
for a live and an archived conversation, each activating its action,
and Show archived checked while they show; `src/store.rs` holds that
`archived` is written under the conversation's lock, refused while its
process holds it, kept by a process that rewrites `meta`, and never
makes a conversation the store does not hold; `src/post.rs` refuses a
message to an archived conversation. The window's units cover a right
press on a row opening that row's menu without opening it, one
elsewhere opening nothing, `S-F10` under the open row, Archive and
Unarchive asked of the window, Delete… putting the question of that
row's conversation, an archived conversation hidden, shown with Show
archived, refused when opened, passed over by `C-PageDown` and not
the most recent, `S-F10` reaching the selected archived row from the
list, kept selected through an update, the open one closed when
archived, and the bar's menu back at
`F10` after a row menu; `src/menu.rs` also holds `S-F10` closing a row
menu. The window's own archiving, its processes ended, its refusal for
a conversation whose `meta` is not yet written, its lock failure and a
read-back after a failed sync, is not unit-tested: the window session
has no test seam of its own; and `src/control.rs` drives `row-menu`
and `show-archived` and reads `archived`, `shown` and `row-menu`.

`tests/control_process.rs` gains two native cases. In the default build,
`F10` through the seat opens the File menu, `Down` and `Return` open the
dialog, a key typed on the seat reaches the masked entry, which the
window shows as bullets and the control socket's state as a length,
`Escape` closes it emptied, and no file holds the key. A test's `/` is
shared (the trusted-root fixture's is mode 1777, and the test's tree
lies under `/tmp`), which the key file's walk refuses, so the case that
saves runs in a build of its own: the gate metadata names
`native-compositor-fixture-feature = "test-key-root"`, whose one effect
is that the walk checks from the directory `TD_AGENT_TEST_KEY_ROOT`
names, which the case makes canonical, rather than `/`; nothing ships
with it. That case,
`native_compositor::fixture::`, sends a message without a key and sees
its turn stop for want of one; stores a key typed into the dialog,
finding `openrouter.key` mode 0600 with one link, holding the key and
a newline, in a directory made 0700, with no temporary file, and the
status row no longer asking; sends again and sees the running
conversation's turn go past the key, to the price check, which stops
it with no models list fetched; and finds the key in no other file
under the test's directory and not on standard error. The fixture
build costs every td-agent native run a second build of the crate, in a
target directory of its own, its library tests and its strict Clippy.

**As built (templates).** `src/config.rs` covers templates read in
order, their own shared lists, an empty one included, `network`'s note,
and each refusal (no name, an empty, padded, control-bearing or
invisible one, a built-in's, a duplicate in any case, an unknown key,
`repos` that is no list or lacks a field, a `sparse` that is no list,
absolute, climbing or control-bearing, a shared list that is no list
or has a relative path, too many), and which list a workspace binds:
a template's own, the top-level one for a template naming none, none
for one gone, through the setup's round trip; `tests/model_client.rs`
holds that a conversation process made from a template sends its own
list, and none once its template is gone, never the top-level one;
`tests/processes.rs` that a template workspace reaches the meta;
`src/ui.rs` covers the chooser's rows in order with Empty selected,
the note, what each choice asks, Directory… opening the folder chooser
even with a card waiting, `Escape`, and `C-n` with no workspaces
starting one with none; `src/window.rs` covers a repository template's
refusal; `src/control.rs` drives `new` both ways; `src/confirm.rs`
covers what the deletion question says of each kind of workspace; and
`src/workspace.rs` round-trips a template workspace, refuses a bad
`template:` argument, and labels one `template NAME`. The native
compositor tests run without the jail, so their `C-n` starts a
conversation with no workspace as before.

The live check is by hand. Run td-agent as `./install-apps` installs it
from a checkout, with the key written as one line to
`$XDG_CONFIG_HOME/td-agent/openrouter.key`, mode 0600, or stored from
File → Set OpenRouter key…. A message to a new
conversation gets a reply, with its usage and cost on it. Asked to plan
three steps, a model writes a todo list, drawn above the composer;
asked to tell another conversation something, it asks on a card to
send it a message and, allowed, sends it, and that conversation answers
in a turn of its own. A new conversation's first reply is followed by a
title from `title_model`. The status row shows the model, the context
used, the cost, today's total and the key's credit. The conversation's
`log` under `$XDG_STATE_HOME/td-agent/` holds the request, the reply
with its `reasoning_details`, and the usage.

For the review command, `src/review.rs` covers its options (each once,
`-` naming the input, `--` before a FILE starting with `-`, an option's
value never another option, bounds and refusals), the input's bound and
an empty input refused, its prompt estimate (a third of ASCII bytes,
every other byte whole), its plan (the default and the model's own
completion limit, a worst case past `max_cost_per_turn` refused, an
unlisted or unpriced model refused while that limit is set and planned
without one, effort refused for a model without reasoning), the
request's body (the model, no cache request, effort only when asked, the
instruction naming the markers and the commit id, the commit quoted
between them, a commit holding them refused), a stream as OpenRouter
sends it (a processing comment, an event split across chunks, the finish
then the usage) written as it comes with its model and provider named,
and as no review: `[DONE]` or an end with no finish, an error inside the
stream, an error reply said by its status and message, a 429 asked
again `client::RETRIES` times after the provider's `Retry-After` and
not when it asks for longer than td-agent waits, a counted reply with
no finish, and one that stopped empty, was
cut at its limit or was filtered.

## 18. Increments

Each is one green, separately landable commit, except the revisions of
2, each its own. The td-agent increments stack on one rolling branch; 3
and 6 touch only td-ui and td-net and land from branches of their own,
in parallel with it.

1. **The design**, and the AGENTS.md route to it.
2. **The revisions**, two commits: coordination, workspaces, git,
   network, shared directories, the two-stage classifier and the chat
   components; then a process per conversation with limits deferred, the
   conversation features of §3, §12 and §14, and the gate cost of §17.
3. **td-ui chat components.** The message list with selection and whole-
   message copy, tested in td-ui, amending td-ui/DESIGN.md.
4. **Window, processes and store.** The crate with its manifest, lock and
   gate metadata, and the generalized exemption of §17; the window process
   and conversation processes over their socketpairs, a conversation
   restarted from its log; the split window with the conversation tree,
   message list and composer; the store with local echo and no model;
   driven window tests; and the host launch, td-net's launch roster
   learning the name (`net/src/launch.rs`) and `./install-apps` installing
   it (`builder/src/install_apps.rs`), adding no logic in shell.
5. **Model client over `td-fetch 1`.** Non-streaming chat with no tools, in
   the conversation process; the key file; the models list; usage, cost
   limits and credit in the status row; titles; mock fetch tests. This is
   the first live OpenRouter use.
6. **Fetch streaming in td-net**, with its §W.8 amendment.
7. **The SSE reader in td-agent.**
8. **Conversation tools.** The todo list; `history_search` and
   `history_read`; `conversations` and `send_message`, with their labels
   and the wake budget. These are the first tools exposed to a model, and
   none touches a file. A conversation counts as a workspace of its own,
   and every read or message that §3 makes a crossing goes to the human
   from the peers step below.
9. **Tool host and tool semantics.** The framed, multiplexed protocol and
   the §12 file and shell tools, tested in-process against fixtures,
   td-txt included. No such tool is exposed to a model.
10. **The `workspace` jail.** The td-jail launch kind with its
    APPLICATIONS.md (§C, §X) and UNSAFE.md amendments: its seccomp
    variant, shared directories from configuration, and the host-mode
    divergences of §8, with no cgroup work, since instances stay in their
    launcher's cgroup (§8); the launch's td-jail, td-txt and
    the tool host; directory and scratch workspaces, with file and shell
    tools exposed for the first time, in `ask` mode.

    Then, before increment 11, small steps of their own, each one
    commit:

    - **Peers.** No orchestrator and no conversation made at startup;
      the same tools and prompt for every conversation; `report`
      removed; every crossing of §3 decided by the human on a card, in
      both modes; an old orchestrator conversation made ordinary, its
      old labels kept; `orchestrator_model` retired with a note (§3,
      §15).
    - **Templates.** `[[template]]` in configuration and the chooser at
      `C-n` and File → `New conversation…`, with Empty and Directory…
      built in and replacing File's two workspace items; a template
      naming repositories refused until increment 11 (§7, §15).
    - **Messages window.** The status row keeps only items of a fixed
      width (the state, model and effort, context, cost, today, credit,
      mode, limits and background count); td-agent's notes to the
      human, which the row showed cut to fit, go instead to a
      Messages window that keeps them whole, in order and with their
      times, which the human opens to read them (§4).
    - **Context menus and archive.** A context menu on a conversation's
      row with Archive and Delete…, archived conversations hidden from
      the list with a way to show them and unarchive one, and `archived`
      in `meta` (§4, §6, §7).
11. **Repository store and workspaces.** The git worker and maintenance
    instances; admitted remotes; the store and its background fetch;
    workspace repositories over alternates with the git mount chain;
    sparse worktrees prepared asynchronously; local commits and step
    snapshots; repository templates, refused until now, and the
    workspace card; the worktree cleanup on archiving or deleting a
    repository workspace's conversation; and the notifications of §3.
12. **Background processes.** `background` on `shell`, the process tools,
    the output store, exit notices, and the window's process list.
13. **Rules and auto mode.** Rules, cards, repetition, both classifier
    stages, the circuit breaker, the calibrated `jev_threshold`, the
    workspace card's trust mark and its listing of each repository's
    `.td-agent/rules`, and,
    for the crossings between conversations, "always" answers for a
    pair and direction and the classifier's deciding them in `auto`
    mode (§3).
14. **Push and fetch tools.** The publish repository, export and strict
    import, evidence and scan, and the bound push; and `git_fetch` (§9),
    the one fetch a model asks for beside increment 11's background
    fetch.
15. **Network.** The egress relay applet with its §W.8 amendment and its
    address predicate, the in-jail proxy, the policies and allowlist
    crossings.
16. **Compaction.** Automatic and manual compaction, the carried state,
    stubs that `history_read` resolves, and the card that offers to
    compact a long conversation whose cache has gone cold before
    resending it (§14, "Resuming cold").
17. **Packaging.** A recipe, an application package with
    `sockets=wayland;fetch` and the egress socket, the portal credential,
    the in-td jail path of §8 with its td-authd amendment, and a boot
    oracle. git ships on td as an explicitly reviewed non-Rust package
    (AGENTS.md), with its TLS closure and the frame-pointer and debug
    companion obligations of `td-profiler/DESIGN.md`, named in that
    landing.

After these: resource limits (§8), schedules (§3), the `question` tool
(§12), a loopback shared by a
conversation's instances (§19), `web_fetch`, `apply_patch`, child
conversations within a workspace, skills and custom commands, the MCP
client, moving a conversation between workspaces, and a native Anthropic
Messages dialect.

## 19. Open questions

- **Host system trees.** The exact §X amendment that lets a `workspace`
  instance bind the host's system trees and `/etc` read-only.
- **Resource limits.** For the later limits of §8: how the launch obtains
  a delegated cgroup v2 subtree, from a systemd user manager's transient
  scope (whose delegated controllers depend on its version and must be
  checked) or, on hosts without one (elogind, Shepherd), from an
  administrator; whether td-net's launch should create the scope
  itself; the defaults per conversation, per workspace and in total; and
  what the status row shows where nothing is delegated.
- **Background servers.** Each jail instance has its own network
  namespace, so a server a background process starts is unreachable from
  the conversation's later calls. Sharing one loopback across a
  conversation's instances needs td-jail to start an instance in an
  existing network namespace, and on a host each instance's user
  namespace is its own, so the namespace's owner has to be arranged; the
  alternative is a conversation-long instance that runs every call.
- **Other harnesses' sessions.** Whether td-agent should see sessions
  outside its own state directory: another td-agent's, or another agent
  harness's, by reading their logs read-only. Messaging them would need a
  socket outside a jail; seeing them would need a reader per format and
  admission of their directories, which hold other models' context and
  may hold credentials.
- **Wake budget.** Whether twenty wakes between human messages (§3) is
  the right bound for conversations messaging each other, and whether
  it should count turns or cost.
- **The jail inside td.** The root request listener td-authd would need to
  start a workspace instance for a jailed td-agent, how the workspace root
  and shared directories become portal grants, and how the git worker
  reaches remotes from a jailed td-agent, which holds no network and no
  SSH agent.
- **Partial clones.** The store is a full clone. A blobless clone would
  make large repositories cheap, but a sparse checkout widened in the jail
  would then need objects the jail cannot fetch; the git worker would have
  to prefetch blobs for the widened paths.
- **Store growth.** The store never prunes while a workspace borrows from
  it; whether per-workspace keep refs in the store should replace that.
- **Jev.** It is in beta from a single provider. Its calibration on the
  fixture set decides `jev_threshold`, and whether its answers stay stable
  when a pending action's untrusted field is written to steer it.
- **Human review.** Whether td-agent should offer the human a review
  checkout of the publish repository, so that writing git commands never
  run in a jail-written worktree.
- **Transcript rendering.** Plain text first; which Markdown subset, if any,
  is worth rendering in the message list. Rendering never fetches.
