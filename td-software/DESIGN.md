# td-software

**Status: being built in §6's increments.** Each *As built* note
describes code that exists and is normative, like any routed design
document (`AGENTS.md` routes here now that the image ships the
component). The rest is a proposal for the repository user's review,
not yet implemented or normative: where it and `AGENTS.md`,
`APPLICATIONS.md` or `td-authd/DESIGN.md` differ, they describe the
code and it describes a possible change.

td-software is a td-ui program and is to be the only place a session
owner installs, updates and removes applications. The repository user
decided that install-on-demand goes away: starting an application that
is not installed never fetches it. The compositor's launcher would show
what td-software installed, gaining an entry after an install and
losing it after a removal.

Three problems prompted it:

- Firefox's first launch spent minutes downloading from Flathub with
  nothing on screen. Per §W.9, td-jail waits for `td-app update` when no
  generation serves the declaration, and the card's request `0d` is
  answered at once (`84`).
- Claude Code is being removed from td entirely, in a separate change,
  so td-software offers it in no form.
- A jailed application that cannot be started from a terminal should not
  have a `/bin/<app>` entry.

## 1. What "installed" means

**Today.** `APPLICATIONS.md` §A says there is no application-manager
binary, and §B.5 says "there is no install". Every application the
machine can run is fixed by the deployment. Applications differ only in
where their bytes come from:

| kind | example | declaration | bytes |
|---|---|---|---|
| image-shipped, source-built | mail, news | recipe, in the image | `/td/store`, in `root.erofs` (§B.1) |
| Flathub | Firefox | `FLATHUB_SOURCES` row and spec, in the image | the account's `.td/flathub/` cache (§W.9) |

The Claude release stream (§W.9) is the third kind today; its removal
from td takes it out of scope here.

These are fixed by the deployment, and this proposal keeps them fixed:

- the registry `/etc/td-applications.tsv` and the launcher table
  `/etc/td-launcher.tsv` (§A.0, "Launcher integration");
- `/etc/td-principals.tsv`, which assigns Firefox 65536, mail 65537,
  news 65538 (and, until its removal, Claude 65539) (§L);
- the accounts, cgroups and runtimes that firstboot prepares at sysinit
  (td-authd/DESIGN.md, "Application runtime preparation" and
  "Application state preparation");
- the specs, policies and the ledger of reservations, which never reuses
  a UID.

**Proposed.** A Flathub application is installed when its
account's cache has a `current` generation. Install, update and remove
change only that cache, which the account owns. The registry and
principal table still list every declaration the image ships, so the set
an owner can install is the reviewed set in git. It changes through `td
update` and `td upgrade`, as principle 6 and §W say. Image-shipped
source-built applications are always installed. td-software lists them
but cannot remove them, because their bytes are part of the deployment.

As a result there is no mutable registry, no allocation of accounts at
run time, and no new elevation operation. The only new mutable thing is
which of the image's declarations currently have a payload, and §5
derives the launcher from that.

## 2. Non-goals

These stay out because principle 5, §B.6 and §Z refuse them:

- No repository, catalogue server, mirror or index that td hosts.
  td-software does not browse Flathub and does not take arbitrary refs.
- No authority of its own. It never runs a payload, never writes a cache
  or table, and never runs an upstream `Exec=` line.
- No package tier for store-shipped applications, which is §W.2's
  deferred column.

## 3. Trust boundaries

**Process.** td-software runs as the human, UID 1000, like td-taskmgr or
td-dua. Fixed private request `22`, beside `07` to `0c`, starts it
(td-authd/DESIGN.md, "Fixed program launch prerequisite"). It gets no
root credentials, no capability and no authority channel (§L).

**Who writes the cache.** The cache is still written only by td-app,
running as the application's account. Root starts it the way it starts
`firefox-update` today: a oneshot that runs `td-app update NAME --force`
as `tda65536` (§W.9). Proposal: replace the per-application units with
one fixed root operation, modelled on `application-start`:

```text
application-cache OWNER APP install|update|remove|delete-data|status
```

Root selects the account from the immutable registry and the ledger, as
`check-launch-application` does, and runs `/bin/td-app VERB APP` with
the same credential drop. No caller supplies an executable, UID, path or
ref.

*As built (increment 1):* td-authd/DESIGN.md, "Application cache
operation". `install` and `update` both run `td-app update APP
--force`; root answers `status` for mail and news itself, as
`kind=image`, and refuses the other verbs for them. A request waits
behind a running check of the same application and is bounded only
while the operation lives, so the forwarding increment sends one
request per application at a time and never kills one early.

**How a request arrives.** td-software sends `install NAME`, `update
NAME`, `remove NAME`, `delete-data NAME`, `status NAME` and `launch NAME`
on the compositor
control socket. That socket admits only exact human kernel credentials
(td-compositor/DESIGN.md §15, "Access control"), and §W.7 (2) already
plans to expose launches there. The compositor forwards each request
over its pinned private channel. Root validates the name against the
image's registry.

This keeps §L's "no public launch listener". It also generalizes `0d`
and `0e` from one fixed name to a validated one.

*As built (increment 2, the request path):* td-compositor/DESIGN.md §15,
"Application requests", and td-authd/DESIGN.md, "Application requests".
The control socket takes the six verbs above; the compositor forwards
`status`, `install`, `update`, `remove` and `delete-data` as private
request `24`, a verb byte and the name, and root runs `application-cache`
for it as a process of its own, at most one operation per name and
only on its own read of the name's status, ended in the last 30
seconds, that shows no other writer or failed, and answers at once:
`86` for a started operation, or for a status the operation running,
the last one's outcome and refusal, and the last record root read, read
again at most every two seconds or once an operation ends. `launch`
stays the compositor's: it does what the card's application entry does,
for the configured application alone, until the third increment carries
a name in the start.

**Consent.** None of these requests is elevation. §W.9's argument still
holds: running the application's own account to write its own cache
grants the human nothing new. Install and update do exactly what a first
launch and the forced check did before. Remove deletes only what a
reinstall can fetch again, and keeps `/var/lib/td/applications/APP_UID`
state (§4.2). Delete data deletes only state the application's own
account wrote under its own home. No td-software operation takes the
secure path: each changes only what one application account owns, never
the human's files, a system location or another application.

The cost is that any human-UID program can reach the control socket. Such
a program could remove Firefox's payload, start a large download, or
delete an application's state, which cannot be undone. It could not
reach the human's files, an admission rule, a system location or
another account. td-software asks for Remove and Delete data with td-ui's
confirmation dialog; that dialog protects against a slip, not against a
hostile program running as the human, which the repository user accepted
in exchange for no secure prompt.

**Status.** The human cannot read the cache: each application home is
mode 0700 and owned by its account, and so is the cache (§W.9). A new
read-only `td-app status APP` verb would take no lock, as `ready` takes
none. Root would run it through the operation above and relay its
bounded typed answer. The answer gives:

- the kind;
- `current` and `previous`, with refs, commits or versions, timestamps
  and deploy sizes;
- `checked`;
- the last refusal;
- progress: the stage, plus bytes and objects done out of their bounds.

td-app prints no progress today, only final lines. A running check would
write a bounded progress record under the cache's `.work/`, and `status`
would read it. td-software polls status while an install runs, and
treats whatever comes back as untrusted, bounded data.

*As built (increment 1):* `status` never waits for or holds the lock; it
reports progress only while a shared try-lock, released at once, finds a
writer holding it, so a record a killed check left never reads as
running. "Out of their bounds" is the graph's objects done out of those
known so far and of the object bound, and its bytes out of the transfer
bound: OSTree names no total before the trees are read. The record's
exact shape is §W.9's.

## 4. Behaviour

### 4.1 Launch without a fetch

`td-app ready` keeps answering a serving generation or `absent`. The
change is in td-jail: on `absent` it stops waiting for `td-app update`.
Instead it refuses with a named diagnostic: "NAME is not installed;
install it in td-software". td-jail also stops starting the unwaited
daily check beside a launch: a launch never touches the network, and an
application updates only when the owner asks in td-software (§4.2),
which shows when each was last checked.

There is a case to design. After a system update, `ready` also answers
`absent` for a generation that no longer serves because the declaration
or policy changed (`checks=`, §W.9). That application is
still installed. Proposal: `status` reports it as installed but stale.
The launch then runs an offline revalidation, which reruns rule 5 over
the deploy trees already present and fetches nothing, and launches if
that passes. If it fails, the launch sends the person to td-software,
whose update fetches as before.

### 4.2 Install, update and remove

- **Install** runs `td-app update APP --force` on an account with no
  `current`. Every admission rule of §W.9 applies unchanged, including
  the feed transport under the autotest token.
- **Update** is the same command on an installed account, and the only
  way an installed application updates. It replaces the launcher's
  per-application UPDATE row and the daily check beside a launch.
  td-software offers "Check now" per application and shows `checked`;
  it schedules nothing in the background.
- **Remove** is a new td-app verb. It clears `current` and `previous`,
  and collects every generation, deploy and object graph that no launch
  holds. Held generations follow §W.9's lease and go when the last
  launch ends.

  It keeps `admitted`, so a reinstall cannot admit a commit older than
  one already admitted. It also keeps the jail state under
  `/var/lib/td/applications/APP_UID/.td/app/NAME`, so a reinstall
  finds the profile again.
- **Delete data** is a separate action on a removed application that
  deletes that jail state, run as the application's account. It cannot
  be undone; td-software confirms it with td-ui's dialog and it takes no
  secure path (§3). It refuses while an instance of the application
  runs. *As built:* td-app knows that from §W.9's lease: every launch
  holds its generation's selection, so once `remove` has run and a
  collection under the cache lock leaves a generation, an instance runs.
  It also refuses while `current` or `previous` exists.

## 5. A launcher that follows installs

**Today.** The card has a terminal entry, one image-named application
(`--launcher-application`), that application's UPDATE row, six
desktop-program entries and close. That is exactly the ten entries the
card reserves, and `registry_entries_are_searchable_and_fit_the_card`
refuses overflow (td-compositor/DESIGN.md, the launcher). The compositor
reads no launcher table: "fully table-driven compositor discovery
remains later work" (§A.0, "Launcher integration").

**The source stays immutable; the selection is root's.** The compositor
would read the builder-authenticated `/etc/td-launcher.tsv` for each
row's name, display name and search terms. It would ask root which of
those names are installed. That answer contains only names from that
table, and root derives it from each account's cache, never from
anything td-software says. Root computes it with `status` at session
start and again after every install, update or remove it performs (as
built, on demand from its status records: §6, increment 3). A
name outside the table, or a malformed answer, fails the generation, as
any malformed private-channel response does today. No mutable file
holds the selection: the caches are the record, so nothing can disagree
with them across a reboot.

**How the compositor learns of a change.** Every install and remove
reaches root through the compositor. The compositor therefore asks for
the installed set again when that request's handle retires, and at
startup. It needs no file watch and no event that root pushes. A change
made some other way, such as an oracle's install unit, shows up the next
time the compositor asks. The proposal is that it also asks each time
the launcher opens, which costs one bounded exchange.

**Matching windows.** Activating an entry raises a mapped surface whose
app id matches. Today the image passes that id as
`--application-app-id` for Firefox alone. Matching by table needs the
manifest's alias (§A, "Application identity") as a fourth launcher-table
column, or a second immutable table built by the same compiler (as
built, the fourth column: §6, increment 3). Each
entry's start request then carries its name instead of being the
name-free `0d`.

**The slot limit.** The launcher table admits up to 256 applications.
The proposal:

- The card keeps fixed entries: the terminal first, then td-software,
  the six desktop programs, and close.
- Installed applications follow, sorted by display name.
- The card shows a window of rows that scrolls with the selection.
- The existing filter narrows the list. The keyboard already walks
  matches the card did not draw.
- The UPDATE row goes away.
- The test becomes "the window fits, and every entry is reachable by
  filter and selection", with a stated maximum row count.
- A search that matches only an application that is not installed shows
  one row, "Install NAME", which opens td-software at that entry.

## 6. Increments and migration

Principle 4 asks that a migration cut over completely and delete the old
mechanism in the same landing. Every increment below stays green on its
own, and install-on-demand is deleted only in the last one, so Firefox
can be installed at every commit.

1. **td-app.** Add the `status` verb, the progress record, the `remove`
   and `delete-data` verbs and offline revalidation. Add the root `application-cache`
   operation. Nothing that users see changes, and on-demand still
   works.
2. **td-software.** The program, its launch request and its card entry.
   It lists applications and their status, and can install, update,
   remove, delete data and launch through the control socket. It runs beside
   on-demand: both paths go through the same td-app admission into the
   same cache, so they cannot disagree.

   *As built (the program):* `td-software/` is a td-ui window, std plus
   td-civil, td-fs and td-ui, with td-app's status record compiled in
   from `engine/src/flathub_app.rs`. It reads the image's registry and
   launcher table (bounded to the builder's 1 MiB each) and lists every
   application: the Flathub declarations first, then those the image
   ships, which show as built in with no action and whose status it
   never asks. For a Flathub one it shows the standing (reading, not
   installed, installed, or installed but needing a check after a
   system update); first what is changing (a status it cannot read now,
   a writer's progress, root's operation and the last one's outcome),
   then the current version's commits, dates and sizes, the previous
   version kept, when it was last checked and the last refusal.
   Progress is the stage, then objects done of those found so far and
   of their bound and bytes of the transfer bound, saying that Flathub
   names no total in advance.

   Its buttons are Install for one not installed, with Delete data once
   a check has run or been refused and no previous version is kept (a
   removed application), and Launch, Check now and Remove for an
   installed one; when every read root made in this window failed,
   which a damaged download can cause, Remove alone. A failed read
   keeps the last record root read and its buttons, but not a writer's
   progress: a writer killed mid-download can be what left the cache
   unreadable, and Remove must not wait on it.
   While something writes the cache, or root's record predates the
   operation that just ended, only Launch stays on, and not during a
   removal. Remove and Delete data ask first in td-ui's confirmation
   dialog, which a change to the application's state closes at once,
   unanswered; nothing else asks. Return launches or installs, and every
   button has a key (F1 lists them).

   Each request is one control-socket conversation
   (`$TD_CONTROL_SOCKET`) on a worker thread, its answer bounded to
   seven seconds and 8 KiB once connected. It asks a status every second
   while something writes, an operation is asked for, or root's record
   is unread, older or failed; every 15 seconds otherwise (60 if root
   says the image ships it after all); and five seconds after a
   failure, which the list and details then show. An `error` says root
   did not act on a request; an `unavailable`, or a conversation cut
   after the request was sent, may have reached root, so the next
   status decides whether the operation ran, ended or never started,
   and how each operation ended is reported from `last=`. Root's
   refusals (§3) reach the status row as text.

   *As built (the card entry):* the image ships the static binary at
   `/bin/td-software` (recipes `td-software` and `td-software-test`),
   and the card's SOFTWARE entry, in authority mode, sends td-authd's
   request `22`, which starts it as the human from `/` with only
   `WAYLAND_DISPLAY` and `TD_CONTROL_SOCKET` added to td-login's
   environment (td-authd/DESIGN.md). The card's UPDATE row stays until
   the cutover (step 4) removes it with `0e`, so the card grows by one
   row.
3. **A table-driven card.** The compositor reads the immutable table,
   asks for the installed set and uses the scrolling window from §5.
   Starts carry a validated name. The image stops passing
   `--launcher-application`.

   *As built (the table and the installed set):* the launcher table's
   fourth column is the app id an application's windows announce: the
   manifest's alias, or for an application without one its entry's file
   name, which a td-ui program gives its window (APPLICATIONS.md §A.0).
   No second table was needed. Root answers the installed set as request
   `23` (td-authd/DESIGN.md, "Application requests"): a row for each of
   the registry's Flathub declarations, with whether its last status read
   found `current` and whether that may change soon, followed through
   `24`'s own per-name entries rather than a request of their own. So
   the set costs no more reads than a status, an operation on a name
   and the set never wait on each other, and the 16 names root follows
   bound it: td-software names only Flathub declarations too, and a
   test holds them to 8, so the two never fill what root follows; a
   registry with more answers no set. A name the answer leaves out is
   one the image ships, which is always installed, so an answer bounded
   by the compiled-in declarations still covers a table of 256.
4. **Cutover, in one landing.**
   - td-jail stops fetching on `absent` and stops starting the check
     beside a launch (`check_beside`). td-app's 24-hour throttle and
     its unforced `update` form go with it; `checked` stays as the
     last-check time td-software shows.
   - The UPDATE row and `0e` go, as do the per-application update
     units.
   - Under the autotest token, `firefox-fetch` becomes a root
     `application-cache … install` run, so the oracles install
     through the path that ships.
   - One oracle installs through td-software's request path.
   - §W.9 and the matching sections of td-authd/DESIGN.md and
     td-compositor/DESIGN.md are amended in the same landing.

**Existing machines.** The cutover changes no cache format. A machine
whose Firefox account already has `current` therefore reads as having
Firefox installed: its launcher entry appears and its next launch starts
without a fetch, after offline revalidation if the update changed its
policy. An account with no
`current`, because the application was never launched or the machine is
freshly installed, shows the application as not installed in td-software,
and it has no launcher entry. Existing jail state is untouched either
way.

**Claude Code.** td does not ship it in any form: its declaration,
account, policy, `/bin/claude` and release-stream support are removed in
a separate change, and td-software offers it neither as built-in nor as
installable. A terminal application td-software installs later uses the
terminal presentation §W.7 plans.

**`/bin/<app>` for window applications.** Removing those links is a
separate change, but td-software's launches rely on it. The
application-exec wrapper builds `/bin/APP` for direct presentation
(td-authd/DESIGN.md, "Fixed application launch prerequisite"), and §A.0
resolves the application from td-jail's `argv[0]`. Dropping the link
needs a fixed td-jail entry outside `PATH`, and the open farm in
`bin_farms()` has to move with it.

## 7. Decisions

The repository user settled the open questions:

1. **Fresh installs.** Nothing is installed by default and nothing
   fetches during installation or boot, as §W.9 says. td-software lists
   Firefox first with Install, so the first download is one the owner
   asked for and can watch.
2. **Removal and state.** Remove keeps the jail state by default; a
   separate "Delete data" action deletes it (§4.2).
3. **Confirmation.** No td-software operation takes the secure path.
   td-ui's confirmation dialog guards Remove and Delete data (§3).
4. **Updates.** Only from td-software. The unwaited daily check beside a
   launch goes (§4.1, §6); no background timer replaces it.
5. **Stale after a system update.** Offline revalidation at launch, as
   §4.1 proposes; a failure sends the owner to td-software.
6. **Status transport.** Root relays `td-app status` (§3); no status
   file is provisioned.
7. **Applications in the image.** td-software shows mail and news as
   built in and offers no hiding. §B.5's hide list can follow later.
