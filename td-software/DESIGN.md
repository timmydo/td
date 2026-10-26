# td-software

**Status: proposal — nothing here is implemented or normative yet.** It is
written for the repository user's review. `AGENTS.md`, `APPLICATIONS.md`
and `td-authd/DESIGN.md` remain the authority; where this text and they
differ, they describe the code and this describes a possible change.
`AGENTS.md` gains a routing line only when the component exists.

td-software would be a td-ui program and the only place a session owner
installs, updates and removes applications. The repository user decided
that install-on-demand goes away: starting an application that is not
installed never fetches it. The compositor's launcher would show what
td-software installed, gaining an entry after an install and losing it
after a removal.

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
td-dua. A new fixed private request beside `07` to `0c` starts it
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
start and again after every install, update or remove it performs. A
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
column, or a second immutable table built by the same compiler. Each
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
3. **A table-driven card.** The compositor reads the immutable table,
   asks for the installed set and uses the scrolling window from §5.
   Starts carry a validated name. The image stops passing
   `--launcher-application`.
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
