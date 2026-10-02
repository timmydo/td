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

Design only: no crate, recipe or entry script exists yet. The decisions
below that were the user's to make were made on 2026-10-01 and
2026-10-02:

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
- **Tool execution:** a jail per workspace, with cgroup memory, CPU and
  process limits (§8), a network policy wider than none (§10), and host
  directories such as `~/Downloads` shared in (§8).
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
td's source-built stage2 toolchain. Its manifest declares one dependency,
`td-ui = { path = "../td-ui" }`, and its lock lists exactly td-agent and
td-ui. It carries no TLS, resolves no names and opens no network
connection itself: every request to a model provider goes through the td
fetch service
(APPLICATIONS.md §W.8), as td-news and td-mail do, so it needs no
dependency sign-off and no td-crypto admission. It becomes the third
carrier of three of the modules td-news and td-mail share byte for byte,
`json`, the flat-TOML parser `toml` and `td_fetch`, and the recipe test
that holds those copies identical (`recipes/src/recipes/td-mail.rs`) gains
it. Its `grep` and `sed` tools are td-txt, the same multicall the image
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
  | td-agent                         |--> fetchd -----> model provider
  |  orchestrator conversation       |--> egress -----> permitted hosts
  |  workspace conversations         |--> git worker -> git remotes
  |  approval engine, store          |
  +----------------------------------+
                | one framed pipe per jail instance
                v
  +----------------------------------+
  | td-jail instance, workspace      |
  |  tool host -> sh, td-txt, git    |
  |  worktrees rw, git pointers ro,  |
  |  system ro, loopback proxy only, |
  |  cgroup leaf                     |
  +----------------------------------+
```

There are four parties, and the design is the separation between them:

1. **The agent process** owns the window, the conversation store, the model
   client, the API key, the fetch grant, the repository store and every
   decision of §11. It never executes anything a model wrote and never
   reads or writes workspace file content. It runs git outside a jail
   only on repositories no jail can write, with the fixed invocation of
   §9, and everything else of git in maintenance instances.
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

## 3. The orchestrator

The orchestrator is a conversation like any other in its log and model
client, with three differences: it is always present, pinned at the top of
the list; it has no jail and no file tools; and its tools act on
workspaces and conversations rather than files.

**Its tools:**

- `create_workspace {name, task, repos: [{remote, base, branch, sparse:
  [paths]}], model?, network?}`: creates a workspace (§7) and its
  conversation, which starts on `task` as soon as each base commit is
  fetched while its worktrees are still being checked out. A remote must
  be admitted (§7); `network` may name `off` or `allowlist`, never `open`
  (§10).
- `send {conversation, text}`: appends a message to a workspace
  conversation, labelled as from the orchestrator, and wakes it if idle.
- `list`: each workspace with its conversation state, worktree states,
  branch, commits ahead of and behind its base, last report, and cost.
- `read {conversation, last?}`: a bounded excerpt of a workspace
  conversation's recent messages.
- `fetch {remote?}`: an immediate store fetch (§7).
- `close_workspace {name}`: archives the conversation and removes the
  worktrees once they are reported clean and pushed, or the human
  confirms what would be lost (§7).
- `ask_user {question, options?}`: a structured question, answered on a
  card.

**Its inputs.** The human's messages, and notifications queued from
workspaces: a conversation's `report` (§12), a turn that finished, failed
or is waiting for approval, a worktree that became ready or failed, and a
base branch that advanced after a store fetch. A notification wakes the
orchestrator for a turn of its own unless the human has paused it, so
work proceeds while the human is away and pauses where a human card is
open.

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

## 4. Window and layout

td-agent is a td-ui widget window (td-ui/DESIGN.md, "Widget window"). A
horizontal `split::Controller` divides it. The preferred share persists in
the state directory, which the toolkit's widget leaves to the consumer.

**Left: conversations.** The orchestrator first, then the workspaces, most
recently active first, as a tree table (td-ui/DESIGN.md, "Shared tree
table"): each workspace row shows its name, conversation state (idle,
running, waiting for approval, failed) and branch, and opens to its
worktrees with their state (fetching, checking out, ready, failed) and any
forked conversations. A row waiting for approval is marked distinctly, so
a run left in the background is visible from the list.

**Right: the active conversation.** From top to bottom:

- the transcript, a message list (below). It holds user, orchestrator and
  assistant messages; reasoning, collapsed to one line until opened; tool
  calls as blocks with their arguments, status and a bounded excerpt of
  their result; and verdicts.
- approval and question cards (§11), composed from the toolkit's existing
  action buttons and wrapped text block, never transcript text.
- the composer, an editable pane. `C-Return` sends; `Return` is a newline.
- a status row: the model, the mode (`ask` or `auto`), the network policy,
  whether the workspace is unconfined, context used against the model's
  length, cgroup usage against its limits, the conversation's cost, and
  the key's remaining credit.

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

One process serves every conversation; a second td-agent process on the
same state directory is refused by a lock.

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
included, since OpenRouter requires them), `tool_choice: "auto"`,
`parallel_tool_calls: true`, `max_tokens`, `reasoning` with an effort from
configuration, and `provider: {require_parameters: true}` so a request is
never routed to a provider that would silently drop `tools` or
`reasoning`. `provider.data_collection` is configuration (§15); the
shipped default is `deny`.

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
`supported_parameters`; a model lacking `tools` is refused for a
workspace or the orchestrator with that reason.

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

**Transport and streaming.** `td-fetch 1` is one buffered request per
connection, with a body cap and a five-minute deadline over the whole
exchange. It cannot stream. The first model-client increment uses it with
`stream: false`: correct, but silent until each response completes, and a
response that runs past five minutes fails. Streaming is its own work,
in two increments (§18):

1. **td-net:** the fetch service gains a streamed response mode. The request
   head carries `stream`, and the reply after its head is a sequence of
   `chunk N` frames ending in `end` or `error reason`. The total deadline is
   replaced by an idle deadline between bytes, with a bounded total. That
   increment amends APPLICATIONS.md §W.8 in the same landing.
2. **td-agent:** an SSE reader over those frames. It handles `data:` lines,
   skips `:` comment lines (OpenRouter's `: OPENROUTER PROCESSING`), stops at
   `data: [DONE]`, and bounds each event. Tool-call fragments are assembled
   by `index`. Text and reasoning deltas are drawn as they arrive.

Interrupting closes the fetch connection. Not every provider stops
generating, or billing, when the stream closes; the interrupt says so.

## 6. Credentials and the conversation store

**The API key** is held only by the agent process.

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
sees it. Each conversation is a directory named by a random id, holding:

- `meta`: title, workspace, model, mode, parent conversation when forked,
  and creation time.
- `prefix`: the exact bytes of the request prefix (§13), written once at
  creation and never rewritten. A later td-agent whose prompt or tool
  definitions differ appends a new prefix as a log event instead, which
  costs that conversation one cache miss and keeps every earlier request
  reproducible.
- `log`: an append-only newline-delimited JSON event log, each event
  carrying a sequence number. Each event is one of:
  - a user message, or a message from the orchestrator;
  - a request as sent: its prefix version, its parameters (model,
    reasoning, provider, `max_tokens`), and the messages after the prefix;
  - an assistant message as received, with its raw `reasoning_details`;
  - a tool call;
  - an approval decision, with who decided it (rule, classifier stage, or
    human), its probabilities where Jev gave them, and the reason;
  - a tool result as returned to the model, and the full result when the
    returned one was cut;
  - a step snapshot (§12);
  - a notification delivered to the orchestrator;
  - usage and cost;
  - a compaction (§14);
  - an interruption, an undo or a redo.

Every request ever sent is a pure function of `prefix` and `log`, so
replaying the log reproduces the exact bytes previously sent, which is
what keeps provider caches warm across restarts. Appends are written whole
and fsynced at turn boundaries. A torn final line is dropped on load, and
dropping it is reported. The log doubles as the audit trail: no jail
instance can write it, and the classifier's and the human's verdicts are
in it. A conversation can be forked at any message, which copies its
prefix and log up to there into a new conversation, and exported to a
local file; nothing is shared through any service.


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

A workspace's jail is a policy, started as td-jail instances. One
long-lived instance serves the file tools; it runs the tool host alone,
which starts no process, and is replaced when the ready worktrees change.
Each `shell`, `grep`, `sed`, snapshot and maintenance call is an instance
of its own. td-jail tears an instance down with every process in it when
its entry exits or its launcher dies, so a timeout or an interruption,
which kills the instance, also ends every descendant, and nothing a command
started can go on changing the workspace after its call has returned.
td-agent needs no process-group signalling of its own to get this.

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
  opt out of any shared directory.
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
- **Limits:** a leaf of the workspace's cgroup (below).
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

**Limits.** td-agent's cgroup subtree has three levels: a total, under it
one node per workspace, and under each a leaf per instance; td-agent's own
process sits in a separate leaf, so no node with children holds a process.
Each workspace node sets `memory.high`, `memory.max`, `cpu.max` and
`pids.max`, and `memory.swap.max` where the kernel has swap (td's does
not), defaulting to 4 GiB with throttling from
3.5 GiB, no swap, two CPUs and 1024 processes, configurable per workspace
within a total that caps all workspaces together (default: three quarters
of memory and of CPUs). Each instance leaf sets `memory.oom.group=1`, so
an out-of-memory kill takes a whole instance, the kernel's choice of one
within the workspace, and its tool result says so; CPU is throttled, never
killed. The status row shows usage against the limits.

None of this exists yet. td-jail today creates one level,
`<owner>/<instance>`, refuses deeper membership, takes limits only from an
authenticated package's resources, and sets no swap limit; and its host
configuration requires `cgroup-root=none` (APPLICATIONS.md §X.1). The
`workspace` kind therefore adds, on td and on a host alike, a caller-named
nested node with caller-chosen limits under the total, and on a host a
`cgroup-root` naming a delegated cgroup v2 hierarchy the caller may write.
`./agent` launches td-agent into one: under a systemd user manager a
transient delegated scope, whose delegated controllers depend on the
systemd version and are checked, not assumed; elsewhere a subtree an
administrator delegates. Without the memory, CPU and process controllers,
tool execution is refused by name unless the human sets `limits = "none"`
in configuration, which the status row then shows.

**Mechanism.** td has one confinement implementation, td-jail
(APPLICATIONS.md §C), and td-agent does not grow a second one. The jail is
a new td-jail launch kind, `workspace`, whose policy is the list above.
It grants no Wayland, bus, audio, fetch or tty, binds the admitted sources
and the git chain, and runs the tool host as its entry. That kind is
specified and landed in td-jail, with its APPLICATIONS.md and UNSAFE.md
amendments, in the increment that first exposes a tool (§18).

**On a development host.** td-jail's `--host` launch (APPLICATIONS.md §X.1)
already runs for an unprivileged caller inside a user namespace it
creates; the capability its stage 1 raises is the new namespace's. The
`workspace` kind differs from §X.1's host application launch in ways its
§X amendment must name:

- the delegated `cgroup-root` above;
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

**The git worker** is the part of the agent process that runs git. It
does so in two places, and the line between them is the design:

- **Outside any jail**, only on repositories no jail can write: cloning
  and fetching the store, importing into and pushing from the publish
  repository, and computing a push's evidence there. On a development host
  it runs the host's `git`; on td, the image's.
- **Inside a maintenance instance**, a `workspace` jail instance with no
  network that runs only the git worker's fixed commands, every git
  command that touches a workspace repository: checking out a worktree
  td-agent created (§8) and its sparse patterns, updating remote-tracking
  refs from the store (bound read-only), gc, the ahead-and-behind counts
  `list` shows, the cleanliness check before closing, and exporting a
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
   limits the compressed pack only, so the import runs in a cgroup leaf of
   td-agent's own with a memory limit, which bounds what inflation and
   delta resolution can take.
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
| reads, grep, glob, plan, question, report, git_fetch | run | run |
| orchestrator: list, read, fetch, ask_user | run | run |
| file edits, sed, shell (local commits included) inside the jail | human | run |
| network to a destination on the workspace allowlist | run | run |
| network to another destination, policy `allowlist` | human | classifier |
| git_push, unprotected branch, no force, clean scan | human | classifier |
| git_push to a protected branch, forced, or a scan match | human | human |
| `request_directory`, read-only | human | classifier |
| `request_directory`, read-write | human | human |
| orchestrator: create a workspace on an admitted remote, within defaults; send | human | run |
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
  and paths, and any message from the orchestrator, which may carry a
  workspace model's prose.

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
   an admitted remote is therefore not by itself a disclosure).
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
  one can be written into another's task; it reaches the classifier only in
  the untrusted field, and the human's global denies follow it;
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
- **`shell {command, timeout_ms?, workdir?}`**: one `sh -c` per call, in a
  jail instance of its own, as mini-swe-agent and Claude Code run one
  process per call. The working directory defaults to the first worktree
  and does not persist between calls, and neither does anything the
  command leaves running. Default timeout two minutes, maximum ten. The
  result carries the exit status and output, cut to a head and a tail
  with the omitted byte count named; the full output is kept in the log.
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
- **`update_plan {plan: [{step, status}]}`**: Codex's semantics. It has no
  effect outside the conversation; the plan is drawn above the composer.
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
a jail instance, never as a write by the agent process. A snapshot is
jail-controlled data, good for undo within the same jail and for showing
diffs, and never trusted outside it. It covers tracked files and
untracked files git does not ignore: ignored files (`target/`, `.env`)
and shared directories are not snapshotted, and undo cannot restore them.
A directory or scratch workspace without git records pre-images of
`write_file`, `edit_file` and `sed` targets instead, which `--sandbox`
makes the whole of what `sed` can write.

Planned later, each its own increment: `apply_patch`, taking Codex's patch
grammar as one string argument for models trained on it; `web_fetch`,
made by the agent process through the fetch service as a network
crossing; a `task` tool for summarizing child conversations within a
workspace; and an MCP stdio client.

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
   and the conversation's creation date. It is fixed at creation; current-
   time changes would break the cache.
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

## 14. Context

The context budget is the model's `context_length` from §5. Before a
request whose estimated prompt exceeds 80% of the budget, compaction runs
in two steps:

1. **Tool-result pruning.** Tool results older than the most recent 40,000
   tokens are replaced with a fixed stub naming the tool, its arguments
   and the byte count omitted, when that frees at least 20,000 tokens
   (opencode's thresholds, configurable).
2. **Handoff summary.** If pruning is not enough, the model writes a
   handoff summary for a successor: progress, decisions, constraints, next
   steps and critical data. This follows Codex's compaction prompt. The
   new history is the prefix, the summary, and the recent user and
   orchestrator messages up to a bound.

Compaction is an event in the log; nothing before it is deleted, and the
requests after it remain a pure function of the log. Pruning breaks the
provider cache once, by design. The human can also compact on demand.

## 15. Configuration

`$XDG_CONFIG_HOME/td-agent/config` is in the flat TOML subset td-news and
td-mail parse. Every key has a default, except `jev_threshold` until it
is calibrated (§11):

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
- `shared`: host directories bound into every workspace, each read-only
  unless marked writable; default `["~/Downloads"]`, read-only
- `remotes`: the admitted git remotes (§7); default empty, so the first
  workspace on a remote asks
- `network`: the default policy, `off` or `allowlist`; default `allowlist`
- `network_allowlist`: the default allowlist of §10, hosts with ports
- `protected_branches`; default `["main", "master"]`
- `fetch_interval` and `fetch_concurrency`; defaults ten minutes and 4
- `limits`: per-workspace memory, CPUs and processes, and the totals; or
  `"none"` on a host without a delegated cgroup (§8)

Unknown keys are refused by name.

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
  errors, and grep and sed's argument mapping with `--sandbox`.
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
  compaction, an orchestrator creating a workspace and receiving its
  report, an orchestrator refused a new workspace while another has
  dropped to `ask`, and each classifier path: both stages allow, either
  defers or denies, a malformed reply, and a Jev outage with and without
  `jev_required`.
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
  only allowlisted host and port pairs, and without it nothing; is
  throttled at its CPU limit and loses only its own instance at its memory
  limit; and leaves no process running after a timed-out call. Admission
  refuses `$HOME`, the store, a credential location, and a shared
  directory containing a worktree.
- **Live checks, by hand and never in the gate:** `./agent` against
  OpenRouter, and a classifier fixture set of pending actions with expected
  verdicts. Each stage's false-allow and false-escalate counts, and Jev's
  calibration, are recorded in the commit that changes a classifier
  prompt, model or threshold.

## 18. Increments

Each is one green, separately landable commit on `td-agent-rolling`.

1. **The design**, and the AGENTS.md route to it.
2. **This revision**: the orchestrator, workspaces, git, network, limits,
   shared directories, the two-stage classifier and the chat components.
3. **td-ui chat components.** The message list with selection and whole-
   message copy, tested in td-ui, amending td-ui/DESIGN.md.
4. **Window and store.** The crate; the split window with the conversation
   tree, message list and composer; the store with local echo and no model;
   driven window tests; and `./agent`. Its logic is `td-builder host-run`
   learning the name (`builder/src/host_run.rs`, and td-net's launch roster
   in `net/src/launch.rs`); the script is only the `./news` bootstrap shape,
   adding no logic in shell.
5. **Model client over `td-fetch 1`.** Non-streaming chat with no tools; the
   key file; the models list; usage, cost limits and credit in the status
   row; titles; mock fetch tests. This is the first live OpenRouter use.
6. **Fetch streaming in td-net**, with its §W.8 amendment.
7. **The SSE reader in td-agent.**
8. **Tool host and tool semantics.** The framed, multiplexed protocol and
   the §12 tools, tested in-process against fixtures, td-txt included. No
   tool is exposed to a model.
9. **The `workspace` jail.** The td-jail launch kind with its
   APPLICATIONS.md (§C, §P, §X) and UNSAFE.md amendments: its seccomp
   variant, the nested cgroup with caller-chosen limits and the delegated
   host root, shared directories, and the host-mode divergences of §8;
   `./agent`'s launch building td-jail, td-txt and the tool host; directory
   and scratch workspaces, with tools exposed for the first time in `ask`
   mode.
10. **Repository store and workspaces.** The git worker and maintenance
    instances; admitted remotes; the store and its background fetch;
    workspace repositories over alternates with the git mount chain;
    sparse worktrees prepared asynchronously; local commits and step
    snapshots; the orchestrator with `create_workspace`, `send`, `list`,
    `read`, `close_workspace` and notifications.
11. **Rules and auto mode.** Rules, cards, repetition, both classifier
    stages, the circuit breaker, and the calibrated `jev_threshold`.
12. **Push and fetch tools.** The publish repository, export and strict
    import, evidence and scan, and the bound push.
13. **Network.** The egress relay applet with its §W.8 amendment and its
    address predicate, the in-jail proxy, the policies and allowlist
    crossings.
14. **Compaction.**
15. **Packaging.** A recipe, an application package with
    `sockets=wayland;fetch` and the egress socket, the portal credential,
    the in-td jail path of §8 with its td-authd amendment, and a boot
    oracle. git ships on td as an explicitly reviewed non-Rust package
    (AGENTS.md), with its TLS closure and the frame-pointer and debug
    companion obligations of `td-profiler/DESIGN.md`, named in that
    landing.

After these: `web_fetch`, `apply_patch`, child conversations within a
workspace, skills and custom commands, the MCP client, moving a
conversation between workspaces, and a native Anthropic Messages dialect.

## 19. Open questions

- **Host system trees.** The exact §X amendment that lets a `workspace`
  instance bind the host's system trees and `/etc` read-only.
- **Host cgroup delegation.** How `./agent` obtains a delegated subtree on
  hosts without a systemd user manager (elogind, Shepherd), and whether
  td-builder's host-run should create the transient scope itself.
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
- **Streaming deadlines.** The idle and total bounds for the fetch stream
  mode, and whether they are per request or the client's to ask for.
- **Transcript rendering.** Plain text first; which Markdown subset, if any,
  is worth rendering in the message list. Rendering never fetches.
