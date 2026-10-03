# td-agent

td-agent is td's agent harness: one td-ui window whose left side lists
conversations and whose right side is the active conversation or a new
one, driving language models reached through a pay-per-token API key
(OpenRouter first). One conversation is the **orchestrator**: the human
talks to it, and it creates **workspaces** for new work, each a set of
sparse git worktrees with its own conversation, and sends updates to
existing ones. A workspace conversation's tool calls execute in a jail
whose policy belongs to the workspace. It is a coding agent first and a
general assistant second, on the same loop. This document is the
normative contract for the program and the starting point for successive
agents; the root `AGENTS.md` and `DEVELOPMENT.md` still govern changes and
submission.

## Status

Increments 3 to 8 of §18 are built: td-ui's message list; the
crate with its gate, the window and conversation processes, the store
and `./agent`; the model client over `td-fetch 1`, with the key file,
the models list, cost limits, credit and titles; td-net's streamed
fetch; streamed replies over it, drawn as they arrive and interrupted
by `Escape`; and the conversation tools, the first a model is given:
the todo list, `history_search` and `history_read`, `conversations`,
`send_message` and `report`, with the wake budget and pausing. After
them came the window's File menu and the dialog that stores the
OpenRouter key from it (§4, §6), then the Conversation menu, which
chooses each conversation's model, from a picker over the models list,
and its reasoning effort (§4), the system context, shown folded at
the head of the transcript, and the diagnostics export (§4). Where
building them
settled a point the design left open, the section says so under "As
built". No recipe names td-agent yet. The decisions below that were the
user's to make were made on 2026-10-01 and 2026-10-02:

- **Use:** both coding and general assistance, coding first.
- **Run target:** an unjailed checkout launch on a development host first
  (`./agent`, the `./news` and `./mail` shape of APPLICATIONS.md §X.7), so
  the harness can be exercised against OpenRouter at once; packaging as a
  jailed td application follows as its own increment.
- **Coordination:** a central orchestrator creates workspaces for new work
  and sends updates to existing workspace conversations (§3).
- **Workspaces:** several sparse-checkout git worktrees per workspace, with
  asynchronous fetch, from the first workspace increment (§7). Agents can
  commit and push (§9).
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
td's source-built stage2 toolchain. Its manifest declares five
dependencies, all td crates by path: `td-civil`, `td-fetch-client`,
`td-json`, `td-toml` and `td-ui`, and its lock lists exactly those and
td-agent. It
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
the check before closing, §7) runs in the workspace's own conversation
process; when that conversation has no process, the window process
starts one for the purpose, without the key, and it exits when the work
is done; if the human opens the conversation
meanwhile, the window process hands that process the key and it goes on
as the conversation's process. A card goes up the socketpair as a
request and its answer comes back down. The orchestrator is a
conversation process with no jail instances.

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
  std opens every file close-on-exec, so no child inherits one. It needs
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
  [--create <role>]`, with its end of the socketpair as standard input
  and output. It replays its log up the socketpair (a `hello`, then every
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
conversations. Up, `send` carries a `send_message` or `report` (an id
of the sender's, the receiver's id, the text, and a report's status),
and `query` asks for the states the window knows; down, `sent` answers
a send, queued or refused with why, and `states` answers a query.
`message` hands a receiver a message from another conversation (its
delivery id, sender, sender's role, text and status), which it
acknowledges with `delivered` once logged, or refuses; `pause` pauses
or resumes the open conversation, and `clear_todo` clears its todo
list. `hello` says whether the conversation is paused, and carries
its prefix for the transcript (§4, the system context). The window
checks every send again, whatever the sender checked (the receiver
exists, the crossing rules, the bounds), and writes it whole to the
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

## 3. The orchestrator and conversations

The orchestrator is a conversation like any other in its log and model
client, with three differences: it is always present, pinned at the top of
the list; it has no jail and no file tools; and its tools act on
workspaces and conversations rather than files.

**Its tools:**

- `create_workspace {name, task, repos: [{remote, base, branch, sparse:
  [paths]}], model?, network?, unshared?}`: creates a workspace (§7) and its
  conversation, which starts on `task` as soon as each base commit is
  fetched while its worktrees are still being checked out. A remote must
  be admitted (§7); `network` may name `off` or `allowlist`, never `open`
  (§10).
- `conversations`, `send_message`, `history_search` and `history_read`,
  which every conversation has (below and §12), reaching every
  conversation without a crossing. For the orchestrator `conversations`
  also gives each workspace's worktree states, branch, commits ahead of
  and behind its base, and last report.
- `todo_write`, its plan across workspaces (§12).
- `fetch {remote?}`: an immediate store fetch (§7).
- `close_workspace {name}`: archives the conversation and removes the
  worktrees once they are reported clean and pushed, or the human
  confirms what would be lost (§7).
- `ask_user {question, options?}`: a structured question, answered on a
  card.

**Its inputs.** The human's messages, messages from other conversations
(below), and notifications queued from workspaces: a conversation's
`report` (§12), a turn that finished, failed or is waiting for approval,
a worktree that became ready or failed, and a base branch that advanced
after a store fetch. A notification wakes the orchestrator for a turn of
its own unless the human has paused it, so work proceeds while the human
is away and pauses where a human card is open.

**Its trust.** Everything a workspace conversation says to it, reports
and excerpts alike, is untrusted content: a workspace's model read
untrusted input before writing it. The orchestrator can therefore pass an
injection from one workspace to another, and the design does not pretend
otherwise. What bounds that is that the orchestrator holds no authority a
workspace lacks: it cannot answer a card, change a workspace's rules,
network policy or limits beyond the configured defaults, admit a remote,
or push. Every crossing is decided per workspace by §11, whoever asked for
it. Nor can it shed a restriction by starting over: the human's "deny
everywhere" rules apply to every workspace it creates, and while any
workspace has dropped to `ask` mode, creating another goes to the human. A
message from the orchestrator reaches a workspace's model as a user-role
message labelled with its source, and reaches the classifier only in its
untrusted field (§11).

The human can open any workspace conversation and talk to it directly, and
create a workspace by hand through the same card the orchestrator's
`create_workspace` fills in.

**Between conversations.** Every conversation can see the others and
message them, not only through the orchestrator:

- `conversations`: each conversation's id, workspace, state (idle,
  running, waiting for approval, paused, failed), background process
  count, cost and last activity, which td-agent itself writes.
  Model-written fields, the title, the todo item in progress (§12) and
  background command lines, are shown only for the caller's own
  workspace, and to the orchestrator for every workspace, so the listing
  is not a channel between workspaces.
- `send_message {to, text}`: queues a message for another conversation,
  which the window process routes. A message is delivered between turns:
  when the receiver's running turn ends, or at once if it is idle, it
  starts a turn as a user-role message labelled with its sender. Messages
  are asynchronous; a reply is a `send_message` back, which reaches the
  sender the same way. `report` (§12) is a `send_message` to the
  orchestrator with a status. A message is at most 32 KiB and a receiver
  holds at most 16 undelivered; a send beyond either, or to an archived
  or closed conversation, fails with a result that says which.
- `history_search` and `history_read` (§12) take another conversation's
  id.

A message from another conversation is untrusted content, whoever sent
it, and reaches the receiver's classifier only in its untrusted field
(§11); it is never the human's authority. Reading another conversation's
log brings that conversation's content, its tool output included, into
the reader's context and so to the reader's provider and anything the
reader may later publish. So:

- a workspace conversation reads and messages every conversation of its
  own workspace (the workspace's conversation, its forks, and theirs),
  and messages the orchestrator, without a crossing;
- reading the orchestrator's log is a crossing for a workspace, since
  that log holds every workspace's reports and whatever the orchestrator
  read from them;
- reading or messaging another workspace's conversation is a crossing
  (§11), whose `discloses` question covers carrying content to a
  workspace that can publish where the source cannot. An "always" answer
  admits that direction and that operation only: allowing A to read B
  lets neither B read A nor A message B;
- the orchestrator reads and messages every conversation without a
  crossing, since relaying is its work. It can therefore carry content
  from one workspace to another, which §11's residual risks state; the
  per-pair crossing stops a workspace reaching another directly, not
  through the orchestrator.

**Wakes.** Two models can wake each other indefinitely, and so can a
model and its own background processes. Every turn started by a message
from another conversation or by a background exit notice counts against
the receiver's wake budget, derived from the log: after twenty since the
human last wrote to that conversation or to the orchestrator, those
deliveries queue without starting turns and the human is notified. Two
kinds of turn do not count, because something else bounds them: a firing
of a schedule the human approved, bounded by its own times, and the
orchestrator's notifications from workspaces, each caused by a
workspace's own turn, bounded by that workspace's budget, or by a
checkout or fetch, which no model can repeat at will. A base branch
advancing wakes the orchestrator but is only a notice to a workspace
conversation, waiting for its next turn. The human can pause any
conversation; a paused conversation starts no turn until resumed,
messages and notices to it queue, and schedule firings to it are skipped
(below). Every turn is reserved against the cost limits as any other.

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
  else `/etc/localtime`; a target conversation, the orchestrator by
  default; the message text; and `catch_up`, default false;
- the human creates one through a card or the composer. A model asks for
  one with `schedule {cron | at, text, to?, catch_up?}`, which is a
  human-only crossing in both modes, because a schedule spends money
  unattended; the card shows the next three times it fires and the
  catch-up choice. The approval is stored with the schedule, binding its
  target, times and text, and each firing gives the classifier that
  record as the human's standing decision to run this task at this time;
  it does not make a model-written text the human's instruction.
  `schedules` lists, and `cancel_schedule` cancels, only the schedules
  targeting the caller's own conversation, the orchestrator's covering
  all; cancelling any other is the human's;
- a schedule whose target is archived or closed stops firing and is
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

**As built (increment 8).** There are no workspaces yet, so every
conversation is a workspace of its own and the orchestrator is the only
other party it reaches. The orchestrator has `conversations`,
`send_message`, `history_search`, `history_read` and `todo_write`; a
conversation has `todo_write`, `history_search`, `history_read`,
`conversations`, `send_message` and `report`. The crossing rules are
`tools::crossing`, applied by the caller and again by the window: a
conversation reads its own log, and the orchestrator reads and messages
every conversation; any conversation messages the orchestrator; reading
the orchestrator's log, and reading or messaging another conversation,
are refused, saying that crossings are decided from a later increment
and are not to be reached another way. No conversation messages
itself. Archived and closed conversations do not exist until
`close_workspace` (increment 11): a send to an id the store does not
hold is refused by name, and the archived and closed checks join with
them. A report's status is `in_progress`, `done` or `blocked`, and a
report goes to the orchestrator alone. `conversations` lists at most
200, the orchestrator first and then the most recently active, with
how many more there are.

- **A message** reaches its receiver's model as a user-role message whose
  first line is its label: `[a message from the orchestrator, not from
  the person]`, `[a message from conversation ID, not from the person]`
  or `[a report from conversation ID, status S, not from the person]`.
  The receiver logs it as a `message` event (§6) and decides there and
  then, between turns, whether it starts one.
- **The wake budget** is counted from the receiver's log: the first turn
  of each message from another conversation, not a report, after the
  human's last message in that log and after their last message to the
  orchestrator, read from its log by time, a turn in the same second as
  that message counting as before it. A turn the human asks again
  (`C-r`) does not count. A report does not count: it is the
  orchestrator's notification of a conversation's own turn, which that
  conversation's budget bounds; nor does the orchestrator's own budget
  stop it. Past twenty, a message is logged `held` without starting a
  turn, and the first held since the budget was renewed adds a notice
  saying so, which the window also shows when the conversation is in
  the background. A held message is in the log, so the next turn's
  request carries it; the human writing to the conversation or to the
  orchestrator renews the budget.
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

## 4. Window and layout

td-agent is a td-ui widget window (td-ui/DESIGN.md, "Widget window"). A
horizontal `split::Controller` divides it. The preferred share persists in
the state directory, which the toolkit's widget leaves to the consumer.

**Left: conversations.** The orchestrator first, then the workspaces, most
recently active first, as a tree table (td-ui/DESIGN.md, "Shared tree
table"): each workspace row shows its name, conversation state (idle,
running, waiting for approval, failed) and branch, and opens to its
worktrees with their state (fetching, checking out, ready, failed), any
forked conversations, and its background processes (§12). A row waiting
for approval is marked distinctly, so a run left in the background is
visible from the list.

**Right: the active conversation.** From top to bottom:

- the transcript, a message list (below). It holds user, orchestrator and
  assistant messages; reasoning, collapsed to one line until opened; tool
  calls as blocks with their arguments, status and a bounded excerpt of
  their result; verdicts; messages from other conversations and
  schedules, labelled with their source; and a divider where compaction
  ran (§14).
- approval and question cards (§11), composed from the toolkit's existing
  action buttons and wrapped text block, never transcript text.
- the todo list (§12), collapsed to its item in progress until opened.
- the composer, an editable pane. `C-Return` sends; `Return` is a newline.
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

**Keys.** `C-n` new workspace card, `C-PageUp`/`C-PageDown` previous/next
conversation, `Escape` interrupt the running turn. A card never takes focus
by itself, so a `y` typed into the composer as a card appears answers
nothing; the human focuses the card (`C-Space` or the pointer) and then
answers `y` or `n`.

The window is operable through td-ui's driven control socket (td-ui/DESIGN.md,
"The semantic seam"), which is how the native compositor tests and an
agent drive it. That socket can answer cards, so it is created under the
caller's runtime directory with the toolkit's ownership contract, and no
jail instance is ever given that directory.

The window is the window process's alone; conversation processes draw
nothing (§2).

**As built (increment 4).** There are no workspaces yet, so each
conversation counts as a workspace of its own: the list is the
orchestrator, then every other conversation, most recently active first,
with the state of the open one (`starting`, `idle`, `restarting` or
`failed`; a closed one shows none), and `C-n` starts a conversation
rather than a workspace card. `F6` and `S-F6` move the focus between the
list, the transcript and the composer, and `Return` on a list row opens
it. The transcript holds what td-ui's message list bounds it to (16 MiB
of text); past that it drops its oldest messages an eighth at a time and
says so, and the log keeps every one. The status row is the state, a
notice when there is one, `no model`, the mode, `no limits` and `0
background`. The split's share is the file `layout` in the state
directory. The driven control socket is opt-in, `--control-socket PATH`,
and its actions are `new`, `previous`, `next`, `send`, `focus-next` and
`focus-previous`.

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
from another conversation under its sender, a report with its status,
and one held with the verdict `held: paused` or `held: wake budget`; it
gets the status of the turn it starts as a human message does. A reply
that calls tools carries a `tool calls` excerpt naming each call with
its arguments, and each result is a `tool NAME` block, an excerpt of
the result whose copy action copies the whole, marked `error` when it is
one. A pause, a resumption and a cleared list are `td-agent` notices.

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

- `New conversation`, shown with `C-n`, which does what `C-n` does;
- `Set OpenRouter key…`, which opens the key dialog below; it has no
  chord;
- `Export diagnostics`, the diagnostics export (below, "As built (the
  diagnostics export)"); it has no chord;
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
host-run path the dialog serves. Masking keeps it off the screen and
out of what the window shows the driven seam; it does not stop another
client drawing a look-alike, which is why the dialog is the host-run
path's alone and the key on td is the portal credential (§6).

Without a key the status row says `no key: File → Set OpenRouter key…
(F10)` after the state, until one is stored, when the notice says where
it was stored and that every conversation uses it from now on.

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
model and reasoning effort. The configuration's `model` (or
`orchestrator_model`) and `reasoning_effort` (§15) are what a
conversation starts with and keeps until the human chooses otherwise,
and a choice is the open conversation's alone: it moves to no other,
and the configuration file is never written. The default model, which
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
  next turn. The orchestrator keeps `orchestrator_model`. What it is
  set over is the configuration's `model` key as the file says it at
  that moment, read again, or that the key is left out, so a change to
  td-agent's built-in default is no edit. At start the saved default
  holds while that key is unchanged; a `model` edited since is the
  newer and wins, and the window forgets the saved default and says so
  in a notice. The configuration file is not written.
- **Refusals** that name where a model was set (§5) say `the
  conversation's model (Conversation → Model…)` for a chosen one,
  `orchestrator_model` for the orchestrator's otherwise, and `` `model`
  or the default model (Conversation → Default model…) `` for another's.
- **The status row** names the open conversation's model and then its
  effort (`anthropic/claude-sonnet-5.5 medium`), or `no reasoning` for
  a model that does not take one, and its context length is looked up
  for whichever model that is.
- **Driven.** The actions gain `model`, which has no chord and opens
  the picker through the item's own path; the state gains `picker` (the
  selected model, `nothing`, or `none` when closed), `query` (the
  filter), `model` and `effort`; and `default-model`, which opens the
  default's picker, `picking` (`default`, `conversation` or `none`) and
  `default`, the default model.

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
conversation…` deletes the open conversation for good. It is off for the
orchestrator, which is the one conversation always there, and with none
open.

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
- **The deletion** is the window's, and never the orchestrator's, in
  this order: the conversation's processes, open, in the background or
  retiring, are killed and waited for; its
  ledger reservations go; then the
  store deletes its directory (§6). The store takes the conversation's
  lock first, so it never deletes under a writer, holds it to the end,
  and renames the directory out of the list before removing it; one
  whose directory was never made is deleted already. Only then does
  what the outbox holds for it go. The window drops its row and, when it
  was the one open, opens the orchestrator. A deletion refused before
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
  models cache, `spend` and `layout`, a crash's leftover temporaries,
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
classifier's first stage uses OpenRouter's decision endpoint instead
(§11).

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
request and summed per turn, per conversation and across the orchestrator
and its workspaces. `GET /api/v1/key` supplies the remaining credit for the
status row. `GET /api/v1/models` is fetched at startup and cached in the
state directory. It supplies `context_length`,
`top_provider.max_completion_tokens`, `pricing`, and
`supported_parameters`; a model lacking `tools` or `max_tokens` is
refused for a workspace or the orchestrator with that reason.

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

- **Host-run:** `$XDG_CONFIG_HOME/td-agent/openrouter.key`, holding one
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
  and creation time.
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
  sender, sender's role, text, a report's status, and `held` (`paused`
  or `budget`) when it started no turn; its delivery id is logged once,
  as a user message's is.
- `tool_call`: the reply it belongs to, the call's id and tool, logged
  and synced before the call runs.
- `tool_result`: the reply, the call's id and tool, the `tool_call` it
  finishes (0 for a call never started), the content as returned to the
  model, and whether it is an error.
- `todo`: the whole list as written, marked `cleared` when the human
  cleared it.
- `pause`: the human paused or resumed the conversation.
- `approval`: the decision on a call, by whom, and Jev's probabilities
  and the reason; nothing writes one until increment 13, and the
  history tools already show only its outcome and who decided.

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
  not read. td-agent runs only as `./agent` on a host until the
  packaging increment, so there is no jailed run for the window to tell
  apart today and the item is always shown; the packaging increment
  hides it in the jailed run, or has it say that the key is set with
  `td-secret set agent/openrouter`.

## 7. Workspaces

A workspace is a directory of git worktrees, a policy, and one
conversation. It is created by the orchestrator's `create_workspace`, or
by the human through the same card, and lives until closed.

**Layout.**

```text
~/td-agent/<name>/                    the workspace tree, the human's to read
  <repo>/                             a sparse linked worktree per entry
  <repo>-<branch>/                    a second worktree of the same repo
$XDG_DATA_HOME/td-agent/
  store/<repo>.git                    one bare repository per remote
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
  which the agent may widen from inside the jail with `git sparse-checkout
  add`; `set`, `init` and `disable` write configuration and fail there.
- The workspace tree's root is `workspace_root` (default `~/td-agent`),
  admitted like any source (§8).

**Admitted remotes.** A remote is the human's decision: a URL, or a host
with a path prefix matched on whole path segments after normalization, in
`remotes` in configuration or added through a card.
Only `https` and `ssh` transports are admitted, never `file`, a local path,
`ext` or plain `http`. A `create_workspace` naming any other remote is a
human-only crossing in both modes (§11), because cloning it runs outside
any jail with the human's credentials and bypasses the egress relay. Branch
and base names are checked as git ref names before use and passed after
`--end-of-options`.

**Asynchronous from the first increment.** `create_workspace` returns at
once. Each worktree then moves through `fetching` (clone or fetch of the
store), `checking-out` (workspace repository, worktree, sparse checkout,
done in a maintenance instance, §9) and `ready`, or `failed` with the
reason. Several worktrees, and several workspaces, prepare at once,
bounded by `fetch_concurrency`. The conversation's first model request
waits only for `fetching`: its project instructions and the repository's
`.td-agent/rules` are read from each base commit in the store by the git
worker outside any jail, with `cat-file` on the trusted store (§13); they
are upstream's content at the base and need no checkout. A tool call
that touches a worktree still checking out gets a result that names its
state, and the conversation and the orchestrator are each notified when
it becomes ready. A jail instance binds a worktree only once it is ready;
the long-lived file-tool instance is replaced whenever the set of ready
worktrees changes.

**Keeping current.** The git worker fetches each store remote in the
background every `fetch_interval` (default ten minutes) and on demand
(`fetch` from the orchestrator, `git_fetch` from a workspace). It then
updates each workspace repository's remote-tracking refs from the store in
a maintenance instance, so a workspace sees new upstream commits without
any network in its jail, and notifies the orchestrator and each workspace
conversation whose base advanced. Rebasing is the conversation's own work.

**Directory and scratch workspaces.** A human may instead admit an
existing directory that is not a git repository, or ask for a scratch
workspace under the jail directory for a general-assistant conversation.
Neither has git management. A directory whose top holds a `.git` is
refused with a pointer to a repository workspace.

**Closing.** `close_workspace` stops the workspace's instances, then asks a
maintenance instance whether each worktree is clean and each branch's tip
has been pushed. That answer is jail-controlled, so a workspace that
reports anything uncommitted, untracked or unpushed, or whose answer
cannot be read, is closed only on the human's confirmation listing what
would be lost. td-agent then removes the workspace tree
`~/td-agent/<name>/` with its worktrees, the workspace repository, the
publish repository and the workspace's jail HOME with a
walk of its own that never follows a symbolic link and restores the
owner's permissions on a directory the jail left unreadable before
descending; it never runs `git worktree remove`, which would run git over
jail-written content. The store is left alone.

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
  between workspaces and is the human's explicit choice. A workspace may
  leave out any shared directory, through `create_workspace`'s `unshared`
  list or the workspace card, which narrows and needs no decision.
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
  and admits `socketpair(AF_UNIX, SOCK_STREAM, ...)` alone, the type
  compared with `SOCK_NONBLOCK` and `SOCK_CLOEXEC` masked off. A network
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
  variables of §10.

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
any instance binds it: the repository's files, empty where git expects
none, and for each new worktree its directory, its `.git` file and its
`worktrees/<id>/` with `gitdir`, `commondir` and an empty
`config.worktree` (the protected entries) and `HEAD` (written once,
writable afterwards), each with a fresh `mkdir` or an exclusive,
no-follow create, so nothing the jail planted is reused. These are plain
files td-agent writes, not git run on the repository. `worktrees/` is
read-only in every instance, maintenance included, so no jailed process
creates, moves or prunes a linked worktree; a maintenance instance then
only checks the new worktree out.

The repository's `config` is td-agent's, read-only, and sets what a jailed
git needs and nothing it could misuse: the human's `user.name` and
`user.email`, `core.sparseCheckout` and `core.sparseCheckoutCone`,
`branch.autoSetupMerge=false` (so `git switch -c` does not try to write
it), `submodule.recurse=false`, `core.fsmonitor=false`, `core.hooksPath`
naming an empty read-only directory, `gc.auto=0`, `maintenance.auto=false`,
and, for the git worker's own gc, `gc.writeCommitGraph=false`,
`repack.updateServerInfo=false` and `gc.worktreePruneExpire=never`, since
`objects/info/` and `info/` are read-only.
There is no `extensions.worktreeConfig`, so no `config.worktree` is ever
read. What therefore fails in the jail, by design: `git config`, `git
remote`, `git sparse-checkout set`, adding worktrees and submodules, and
gc; the git worker does gc in a maintenance instance.

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
  repositories, or the caller's runtime directory, other than the parts of
  the data directory the git chain binds;
- one that is, contains, or lies inside a credential location: `~/.ssh`,
  `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.config/gcloud`, `~/.config/gh`,
  `~/.netrc`, `~/.git-credentials`, `~/.config/git/credentials`,
  `~/.cargo/credentials` and `~/.cargo/credentials.toml`, `~/.npmrc`,
  `~/.pypirc`, `~/.docker`, `~/.kube`, `~/.password-store`,
  `~/.local/share/keyrings`, td's own credential and secret stores, or a
  browser profile;
- a shared or extra directory that contains a worktree or any part of the
  git chain, which would make a protected entry's ancestors renamable;
- a shared or extra directory, or a directory workspace, that is,
  contains, or lies inside `workspace_root`, so that no workspace reaches
  another's tree; and td-agent creates no worktree under a path a live
  grant covers.

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
  instance runs the checkout's own tool host and td-txt, built by the same
  `td-builder host-run` launch that builds td-agent and bound read-only
  into the instance, with no package, no Wayland and no bus;
- §X refuses to borrow the host's own `/etc` or system trees, but a coding
  agent's tools on a host are the host's: its compiler, git and shell. The
  `workspace` kind binds them read-only, for that kind alone, as an
  availability divergence;
- `./agent`'s launch builds td-jail from the checkout and writes the host
  configuration td-agent passes to it, in the launch's own runtime
  directory.

If that amendment is not made, tool execution is refused by name on a
host. There is no silent unconfined fallback.

**Unconfined workspaces.** The one way to run tools without a jail is a
choice the human makes when creating a directory or scratch workspace;
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
workspace's own conversation process (§2).

- **Outside any jail**, only on repositories no jail can write: cloning
  and fetching the store, importing into and pushing from the publish
  repository, and computing a push's evidence there. On a development host
  it runs the host's `git`; on td, the image's.
- **Inside a maintenance instance**, a `workspace` jail instance with no
  network that runs only the git worker's fixed commands, every git
  command that touches a workspace repository: checking out a worktree
  td-agent created (§8) and its sparse patterns, updating remote-tracking
  refs from the store (bound read-only), gc, the ahead-and-behind counts
  `conversations` shows, the cleanliness check before closing, and exporting a
  commit for a push. Its git reads no configuration the model can write:
  `HOME` is an empty read-only directory, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_CONFIG_GLOBAL=/dev/null`, hooks and fsmonitor are forced off on
  the command line as in the repository's configuration, and `GIT_DIR`
  and `GIT_WORK_TREE` are set, so a nested `.git` the model made cannot
  steer discovery. Everything it
  reports is still jail-controlled data, since the refs and objects it
  reads are the model's, and is shown as such.

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

**Fetching** is `git_fetch {worktree}`: an immediate store fetch and
remote-tracking update for that workspace, which moves data in from an
admitted remote and needs no approval.

**Object lifetime.** Workspace and publish repositories borrow the
store's objects, and store gc cannot see their refs, so the store never
prunes while any workspace exists: its gc repacks with unreachable
objects kept. Snapshots (§12) are kept reachable by
`refs/td-agent/snapshots/<conversation>` in the workspace repository, and
workspace gc, in a maintenance instance, keeps what those refs reach.

A repository workspace's history is therefore the agent's to make and the
human's, or the classifier's, to publish, as in td's own workflow, where a
pushed branch is the submission.

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
- `open`: any destination the relay will reach. Only the human sets it.

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
host-run launch on a development host and as a unit on td. It takes a host
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
| reads, grep, glob, todo, question, report, git_fetch | run | run |
| listing, reading, waiting on and killing own background processes | run | run |
| history of its own conversation; `conversations` | run | run |
| reading or messaging a conversation of its own workspace; messaging the orchestrator | run | run |
| orchestrator: fetch, ask_user, reading or messaging any conversation | run | run |
| file edits, sed, shell (background and local commits included) inside the jail | human | run |
| reading or messaging another workspace's conversation; reading the orchestrator's log | human | classifier |
| `schedule` | human | human |
| network to a destination on the workspace allowlist | run | run |
| network to another destination, policy `allowlist` | human | classifier |
| git_push, unprotected branch, no force, clean scan | human | classifier |
| git_push to a protected branch, forced, or a scan match | human | human |
| `request_directory`, read-only | human | classifier |
| `request_directory`, read-write | human | human |
| orchestrator: create a workspace on an admitted remote, within defaults | human | run |
| orchestrator: create a workspace on any other remote, or beyond defaults | human | human |
| orchestrator: close a workspace not reported clean and pushed | human | human |
| network policy `open`, shared directories, limits, remotes, rules, modes | human | human |
| anything in an unconfined workspace, reads included | human | human |

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
so that the orchestrator cannot shed it by creating a new workspace. A
repository's own `.td-agent/rules`, read from the base commit (§7), may add
deny and ask rules and nothing else, so a hostile checkout can narrow what
runs but never widen it. A boundary the human states ("don't push")
becomes a rule through a card, not text in the transcript; a boundary
stated only in chat can be lost to compaction, and a rule cannot.

**Repetition.** Three consecutive calls of one tool with identical
arguments go to the human whatever the table says, as opencode's
`doom_loop` does: a loop is a symptom worth a person's look.

**The classifier** decides only the rows the table gives it, and an action
it decides runs only when both of its stages allow. Both see the same
state, built from separated, labelled fields:

- the human's messages: in the workspace conversation, and in the
  orchestrator conversation up to and since the workspace's creation, so
  that a task the orchestrator wrote can be traced to what the human
  asked;
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
   `POST /api/v1/systemone` (model `typesafe/jev-1.13`, configurable as
   `classifier_fast_model`), through fetchd like any other request. It
   takes the state and typed questions and answers with probabilities in
   well under a second, at a fraction of a chat model's price. td-agent
   asks a Choice, `request` of `matches`, `exceeds` or `unrelated` (the
   action against the human's request), and a yes-or-no, `discloses`
   (whether the action sends workspace content to a destination that is
   neither an admitted remote nor on the workspace's allowlist; a push to
   an admitted remote is therefore not by itself a disclosure). Content
   carried to another workspace, by a message or a read, is a disclosure
   when the receiving workspace's remotes, allowlist or network policy
   reach a destination the source's do not, and the state says which.
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
the run, and only the human restores `auto`, and while any workspace is
dropped, the orchestrator's workspace creation goes to the human. Every
verdict, with Jev's probabilities, is in the log.

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
- the orchestrator relays text between workspaces, so an injection read in
  one can be written into another's task, and content one workspace may
  not send another directly can reach it through the orchestrator's
  reads and messages, which cross nothing (§3); it reaches the classifier
  only in the untrusted field, and the human's global denies follow it;
- a read-only directory's contents reach the provider and the workspace
  once granted, which is why the credential locations of §8 are refused
  outright rather than left to the classifier.

**Cards** are drawn by td-agent's chrome from toolkit widgets. Model text
is rendered as text in the message list, so it cannot draw one. A card
shows the exact action (for a push, the commit id, remote, branch and
evidence), the boundary it crosses, and the classifier's reason when there
is one. Its choices are: allow once, deny, always allow in this
workspace, always deny in this workspace, and always deny everywhere.
These approvals are not td elevation (principle 7). They govern what a
workspace's jail and the git worker may do, grant nothing beyond the
human's own authority, and carry no secure-attention claim. An action
needing elevation is out of td-agent's reach by design.

## 12. Tools

A workspace conversation's tools are small and non-overlapping, following
Anthropic's guidance on writing tools for agents: fewer tools, natural
identifiers, actionable errors. Their definitions are fixed text in the
prefix.

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
  returns the answer, as opencode's `question` does.
- **`report {status, summary}`**: sends a report to the orchestrator.
- **`request_directory {path, write?}`**: asks for an extra host directory
  bound into this workspace's later instances, admitted per §8 and decided
  per §11.
- **`git_fetch`** and **`git_push`**: §9.

Paths are absolute. Worktrees and shared directories are bound at their
real paths, so the paths the model sees are the paths the human sees. A
relative path is an error naming the worktrees.

**Step snapshots and undo.** Before and after each model step that
changes files, the tool host records each worktree's state as a git tree,
with `git add -A` into a private index in the workspace's jail home and
`git write-tree`, into the workspace repository's objects, and commits the
tree onto `refs/td-agent/snapshots/<conversation>` so gc keeps it; the log
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
  keeps each process's output, standard error interleaved and marked, in
  its conversation directory up to `background_output_bytes` (default 16
  MiB), dropping the oldest beyond that; a read from before the retained
  range starts at its beginning and says how many bytes were dropped.
  The output is kept after the process ends and across restarts, until
  the conversation is archived or closed; `history_search` does not
  cover it, though the exit notice's tail, being in the log, is.
- `process_wait {id, timeout_ms}`: returns when the process exits or the
  timeout passes (at most ten minutes), with its state and the tail of
  its output.
- `process_kill {id}`: tears its instance down, with every process in it.

A conversation runs at most `max_background` (default 4) at once. When
one exits, a notice with its status and output tail is delivered between
turns like a message (§3) and wakes the conversation if idle. Background
processes are listed under their conversation in the window's tree, each
with a kill action and its output viewable read-only, and the status row
counts them. They end when killed, when their conversation is closed or
archived, or when their conversation process exits; none survives
td-agent, and on restart the log records each still running as lost. A
background process keeps its conversation process running (§2). Each
instance has its own network namespace, so a server one call starts is
not reachable from a later call's instance; §19 records that.

A background process can change a worktree between and during steps, so
a step snapshot may include its changes, and the step's diff says that
background processes were running. Undo and redo are refused while any
background process of the workspace runs, since a restore could
overwrite what one wrote or be overwritten by it.

**Todo list.** `todo_write` replaces the conversation's whole list with
items of `pending`, `in_progress`, `done` or `cancelled`, at most one in
progress and at most 50 items of 500 bytes each, the semantics of Codex's
`update_plan` and Claude Code's `TodoWrite`. The static text asks for one
on work of three or more steps. Each write is a log event, so the list
survives restarts and compaction (§14). It is drawn above the composer;
`conversations` (§3) shows each conversation's item in progress, so the
orchestrator and the human can follow work without reading transcripts;
and the human can clear it. It has no effect outside the conversation and
needs no approval. The orchestrator's list is its plan across workspaces.

**The conversation's log.** The model's context is a view of the log
(§6): compaction prunes and summarizes it (§14), and tool results are cut
for the model. The log itself keeps everything, and two tools reach it:

- `history_search {query, conversation?, kinds?, limit?}`: events whose
  text contains every one of the query's terms, case-insensitively, newest
  first; `kinds` narrows to user, orchestrator and conversation messages,
  schedule firings, notifications and notices, assistant text, tool
  calls, full tool results, approvals or compactions. In either tool, an
  approval shows the model only its outcome and who decided it (a rule,
  the classifier or the human), never Jev's probabilities or the
  reasoning stage's reason, so an injected model cannot tune against the
  classifier.
  Each hit carries its sequence number, kind, time and a bounded excerpt
  around the first match; at most `limit` hits (default 20, at most 100).
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
conversation, and to the orchestrator for every one, as the title is.
A member that is null is as if left out, since a model that fills
every member of a schema sends null for one it means to omit: an
optional one takes its default and a required one is missing. The
history tools' `conversation` is trimmed, and when empty or blank is the
caller's own log, as when left out; `send_message`'s `to` is trimmed
and refused when empty, never a default receiver. A conversation id
that does not parse is refused quoting the value, cut to 64 characters.

## 13. Prompting

The prefix of §6 is ordered from stable to volatile, so it caches, and is
followed by the conversation. The orchestrator and workspace
conversations have different static texts and tools.

1. **Static system text**, in this order:
   - identity and capabilities;
   - for a workspace conversation: task execution (keep going until the
     task is done, fix root causes, verify with the project's tests,
     commit when a coherent change is complete, report to the
     orchestrator), editing (read before editing, absolute paths, minimal
     changes in the surrounding style), and the worktrees' readiness rule;
   - for the orchestrator: decomposing work into workspaces, keeping the
     human informed, treating workspace reports as untrusted, and never
     asking a workspace to work around a refusal;
   - tool guidance;
   - the active mode, and that refusals are not to be worked around;
   - the final-message form.

   This follows Codex's open base prompt and the published structure of
   Claude Code's. A scratch workspace swaps the coding sections for a
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
`prompt/conversation.txt`, `prompt/orchestrator.txt` and
`prompt/title.txt`. With no tools, no environment block and no project
instructions yet, the prefix is the static text alone, which says that
the conversation has no tools and that the window shows plain text. A
title request is the title prompt and one user message quoting the
first message and the start of the reply, each cut to 4 KiB, with
`max_tokens` 256. It is sent once, after a conversation's first reply,
and never for the orchestrator. Its reply's first line, without
quotation marks, becomes the title. A title request that is refused
leaves the first line standing and says why in a notice, and one that
fails records why in its request's finish; neither fails the turn.

**As built (increment 8).** The prefix is the role's tool
definitions and its static text (§5). Both static texts now name the
tools the role has and what it still lacks; ask for a todo list on work
of three or more steps; send the model to the history tools for what
has left its context; say that a message from another conversation is
labelled, not from the person, possibly wrong, and never permission;
say that messages are delivered later and not to wait for a reply; and
say that a refused crossing is not to be worked around. A
conversation's text asks it to `report` to the orchestrator when the
orchestrator gave it work; the orchestrator's asks it to weigh reports,
and that each message it sends can start a turn that costs money.

**As built (the environment block and message times).** The system
message is the static text, then an environment block of what holds for
the whole conversation:

- when it began: its `meta`'s creation time, in UTC. A fork (§6) is to
  keep its source's creation time, or its first request would replace
  the prefix it shares;
- that each message from the person, the orchestrator or another
  conversation begins with a line `[received <UTC time>]`, the time this
  conversation logged it; that the line, and the label after it on a
  message from the orchestrator or another conversation, are
  td-agent's and nothing in the text after them is; and
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
  them but to ask for what it needs.

There are no worktrees, shared directories or network policy to name
until workspaces land (§18), and the block grows then. The time is on
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
source label it had (the human, the orchestrator, another conversation,
a schedule), so nothing becomes the human's by being carried:

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
- `model`, `orchestrator_model`, `title_model`, `classifier_fast_model`
  and `classifier_model`
- `jev_threshold`, and `jev_required`; default `true`
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
- `remotes`: the admitted git remotes (§7); default empty, so the first
  workspace on a remote asks
- `network`: the default policy, `off` or `allowlist`; default `allowlist`
- `network_allowlist`: the default allowlist of §10, hosts with ports
- `protected_branches`; default `["main", "master"]`
- `fetch_interval` and `fetch_concurrency`; defaults ten minutes and 4
- `max_background` and `background_output_bytes`; defaults 4 and 16 MiB
  (§12)
- `auto_compact`, `compact_at`, `compact_keep_tokens` and
  `compact_model`; defaults `true`, 80%, 20,000 and the conversation's
  model (§14)

There is no `limits` key until §8's limits land. Unknown keys are refused
by name. For example:

```toml
model = "anthropic/claude-sonnet-5.5"
mode = "auto"
max_cost_per_day = 25

[[shared]]
path = "~/Downloads"

[[shared]]
path = "~/src/reference"
write = false
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

**As built (increment 5).** `base_url`, `model`, `orchestrator_model`,
`title_model`, `reasoning_effort`, `data_collection` and the three cost
limits are read and checked. The defaults are
`https://openrouter.ai/api/v1`, `anthropic/claude-sonnet-5.5` for both
conversation models, `anthropic/claude-haiku-4.5` for titles, and
`medium`. `base_url` must be an `https://` URL with a host and no query,
fragment or space, so the key is never sent in the clear; a trailing
`/` is dropped. A model id is printable ASCII. `reasoning_effort` is one
of `none`, `minimal`, `low`, `medium`, `high` and `xhigh`. A limit is a
non-negative number of credits, or `none`. `model`, `orchestrator_model`
and `reasoning_effort` are what a conversation starts with; the
Conversation menu chooses another for one conversation, and a default
model the window saves replaces `model` until the key is edited (§4);
this file records neither. The key file is not a key of
this file (§6).

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
| Child sessions, background subagents that notify | adapted as the orchestrator and its workspaces (§3) |
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
  refs with undo and redo; store gc keeping borrowed objects; and closing
  a dirty workspace only on confirmation. A planted reflog, `FETCH_HEAD` or
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
  compacted and retried once, an orchestrator creating a workspace and
  receiving its report, an orchestrator refused a new workspace while
  another has dropped to `ask`, messages between conversations with their
  labels and wake budget, a background process's exit notice waking an
  idle conversation, a conversation process killed mid-turn and restarted
  from its log, a restart that finds a tool call, request or delivery
  started but not finished and repeats none of them, a policy change
  re-deciding a pending approval, and each classifier path: both stages
  allow, either defers or denies, a malformed reply, and a Jev outage
  with and without `jev_required`. A conversation process is a child
  process here as in the window, driven over its socketpair by a test
  harness standing in for the window process.
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
- **Live checks, by hand and never in the gate:** `./agent` against
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
  `td-fetch-client`, `td-json`, `td-toml` and `td-ui` (its
  dependencies). `td-civil` joined when the history's UTC stamps left a
  copied calendar; `td-compositor` joined with the window increment, which
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
keys typed through the headless compositor's seat send a message to the
orchestrator, start a conversation and send there (`Return` a newline,
`C-Return` the send), and switch back, each result read from the store.
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
arguments, the search and read limits, a message's 32 KiB and a
report's statuses; the crossing rules for each pair of roles; the
listing's field filtering; and the definitions in the prefix.
`src/history.rs` covers search (every term, case folded, newest first,
kinds, the limit, excerpts on character boundaries), the cursor over a
3,000-character tool result paged back whole, an offset inside a
character, a page always taking one, and an approval's redaction in
both tools. `src/wake.rs` covers the budget's count, reports and
retries not counting, its renewal in the conversation and through the
orchestrator, and its one notice; `src/post.rs` the outbox across a
restart and the window's own check; `src/store.rs` the new events
replayed exactly, `paused` in `meta`, and calls without results
answered at load once; `src/assemble.rs` and `src/client.rs` a call
without an id or name, a counted reply's calls, and the wire form of
messages, calls and results; and the window's units the todo panel and
its keys, pausing, messages, calls and results in the transcript, and
a streamed reply's calls kept once it is logged. `client::check_calls`
covers ids missing, shared or too long and names missing or too long.
`tests/support/mock_fetch.rs` can route a request by a marker its body
carries to a script of its own. The fixtures `stream-tool-todo.sse`,
`stream-tool-parallel.sse`, `stream-tool-malformed.sse` and
`stream-tool-send.sse` are hand-written tool-call streams in 17-byte
argument fragments, and `models.json` gains a model without `tools`.
`tests/model_client.rs` runs a tool call's round trip (the second body
rebuilt from the log byte for byte), two parallel calls answered in
their order, malformed arguments answered with an error, the step
bound, a model without tools refused, the wake budget held past twenty
and renewed by the human, a paused conversation holding a message and
starting its turn when resumed, a pause sent mid-turn holding a message
that came before it, a message handed on twice logged once, a crossing
refused as the call's result, a conversation a message woke first
titled after the human's first turn, and a restart that finds a call
started and not finished; and, through a `Supervisor` and the window's
`Post`, the orchestrator messaging a closed conversation, which is
woken and answers; and an interrupt that comes while a call waits on
the window, which lets that call finish, answers the next as not run
and offers `C-r`, whose request carries both results.

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
orchestrator's turn go past the key, to the price check, which stops
it with no models list fetched; and finds the key in no other file
under the test's directory and not on standard error. The fixture
build costs every td-agent native run a second build of the crate, in a
target directory of its own, its library tests and its strict Clippy.

The live check is by hand. Run `./agent` from a checkout, with the key
written as one line to `$XDG_CONFIG_HOME/td-agent/openrouter.key`, mode
0600, or stored from File → Set OpenRouter key…. A message to the
orchestrator gets a reply, with its usage and cost on it. Asked to plan three steps, a model writes a todo
list, drawn above the composer; asked to tell a conversation something,
the orchestrator sends it a message, which that conversation answers in
a turn of its own. A new conversation's first reply is followed by a
title from `title_model`. The status row shows the model, the context
used, the cost, today's total and the key's credit. The conversation's
`log` under `$XDG_STATE_HOME/td-agent/` holds the request, the reply
with its `reasoning_details`, and the usage.

## 18. Increments

Each is one green, separately landable commit, except the revisions of
2, each its own. The td-agent increments stack on one rolling branch; 3
and 6 touch only td-ui and td-net and land from branches of their own,
in parallel with it.

1. **The design**, and the AGENTS.md route to it.
2. **The revisions**, two commits: the orchestrator, workspaces, git,
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
   driven window tests; and `./agent`. Its logic is `td-builder host-run`
   learning the name (`builder/src/host_run.rs`, and td-net's launch roster
   in `net/src/launch.rs`); the script is only the `./news` bootstrap shape,
   adding no logic in shell.
5. **Model client over `td-fetch 1`.** Non-streaming chat with no tools, in
   the conversation process; the key file; the models list; usage, cost
   limits and credit in the status row; titles; mock fetch tests. This is
   the first live OpenRouter use.
6. **Fetch streaming in td-net**, with its §W.8 amendment.
7. **The SSE reader in td-agent.**
8. **Conversation tools.** The todo list; `history_search` and
   `history_read`; `conversations` and `send_message`, with their labels
   and the wake budget. These are the first tools exposed to a model, and
   none touches a file. Until increment 13 adds their crossings, every
   read or message that §11 makes a crossing is refused with that reason,
   and a conversation outside any workspace counts as a workspace of its
   own.
9. **Tool host and tool semantics.** The framed, multiplexed protocol and
   the §12 file and shell tools, tested in-process against fixtures,
   td-txt included. No such tool is exposed to a model.
10. **The `workspace` jail.** The td-jail launch kind with its
    APPLICATIONS.md (§C, §X) and UNSAFE.md amendments: its seccomp
    variant, shared directories from configuration, and the host-mode
    divergences of §8, with no cgroup work, since instances stay in their
    launcher's cgroup (§8); `./agent`'s launch building td-jail, td-txt and
    the tool host; directory and scratch workspaces, with file and shell
    tools exposed for the first time, in `ask` mode.
11. **Repository store and workspaces.** The git worker and maintenance
    instances; admitted remotes; the store and its background fetch;
    workspace repositories over alternates with the git mount chain;
    sparse worktrees prepared asynchronously; local commits and step
    snapshots; the orchestrator's `create_workspace`, `close_workspace` and
    notifications.
12. **Background processes.** `background` on `shell`, the process tools,
    the output store, exit notices, and the window's process list.
13. **Rules and auto mode.** Rules, cards, repetition, both classifier
    stages, the circuit breaker, the calibrated `jev_threshold`, and the
    crossings for reads and messages between workspaces and of the
    orchestrator's log.
14. **Push and fetch tools.** The publish repository, export and strict
    import, evidence and scan, and the bound push.
15. **Network.** The egress relay applet with its §W.8 amendment and its
    address predicate, the in-jail proxy, the policies and allowlist
    crossings.
16. **Compaction.** Automatic and manual compaction, the carried state, and
    stubs that `history_read` resolves.
17. **Packaging.** A recipe, an application package with
    `sockets=wayland;fetch` and the egress socket, the portal credential,
    the in-td jail path of §8 with its td-authd amendment, and a boot
    oracle. git ships on td as an explicitly reviewed non-Rust package
    (AGENTS.md), with its TLS closure and the frame-pointer and debug
    companion obligations of `td-profiler/DESIGN.md`, named in that
    landing.

After these: resource limits (§8), schedules (§3), a loopback shared by a
conversation's instances (§19), `web_fetch`, `apply_patch`, child
conversations within a workspace, skills and custom commands, the MCP
client, moving a conversation between workspaces, and a native Anthropic
Messages dialect.

## 19. Open questions

- **Host system trees.** The exact §X amendment that lets a `workspace`
  instance bind the host's system trees and `/etc` read-only.
- **Resource limits.** For the later limits of §8: how `./agent` obtains
  a delegated cgroup v2 subtree, from a systemd user manager's transient
  scope (whose delegated controllers depend on its version and must be
  checked) or, on hosts without one (elogind, Shepherd), from an
  administrator; whether td-builder's host-run should create the scope
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
