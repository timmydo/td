# Workloads: accounts, runtimes and grants

**Status: decided by the repository user; not landed.** Each increment in
"Landing order" amends the documents that state the current contract
(`AGENTS.md`, `APPLICATIONS.md`, `td-authd/DESIGN.md`, `td-sandbox/DESIGN.md`,
`td-agent/DESIGN.md`, `td-software/DESIGN.md`, `td-web/DESIGN.md`,
`UNSAFE.md`) in the landing that changes it. Until then those documents
describe the system as built, and where this one disagrees with them it
describes the target.

A **workload** is anything td starts to run code on the human's behalf
that td did not review as part of the target graph, or that acts on
untrusted input: Flathub applications (Firefox), td's own applications
(mail, news, td-web), every td-sandbox sandbox and every td-agent
workspace.

## The decision

No workload runs as the human. Each runs as an account of its own, sees a
read-only runtime, owns a private writable home, and reaches anything of
the human's only through a grant the human made and td-authd performed.

One exception is deliberate, and the repository user chose it:
td-agent's unconfined workspace, which the human picks for a
conversation, or which is all the chooser offers where td-jail cannot
run and the human enabled `unconfined_without_jail` (td-agent/DESIGN.md
§8, "Unconfined workspaces"), runs its tools as the human with no jail.
The model drives those tools and its input can be hostile, so this is
the workload the decision otherwise forbids, with everything the human
holds: the whole home, the SSH keys and agent, td-agent's key file, its
store and the repositories it publishes; the compositor's control
socket, whose human-credential check is the one this decision keeps
workloads from passing, so it can grant any application or sandbox any
of the human's directories, or a sandbox the host's network; td-agent's
own control socket, so it can answer its own cards; root's workspace
intake, so it
can create, delete and launch any sandbox and spend workload rows, and
from step 8 empty or enter any confined conversation's slot; and no
bound on the processes it leaves (td-login/THREAT-MODEL.md §1). Every
other td-agent workspace is jailed and, from step 8, runs as an account
of its own.

The alternative, every workload as the human with sharing by plain bind
mounts, is simpler and was rejected. Its jail is then the only wall: an
escape from Firefox, a sandbox or an agent's tools holds the human's whole
home (SSH keys, the td-agent key, every application's state) and can
signal the human's unconfined processes. It also passes the compositor
control socket's check, which admits "exact human kernel credentials"
(td-compositor/DESIGN.md §15), so the compositor could not tell a request
from the human apart from one from an escaped workload.
`APPLICATIONS.md` §L already makes distinct application identities a
prerequisite for elevation ("`td-compositor` must NOT share a uid with
anything an application can become").

The decision reverses one placement: mail and news, which now run as the
login user (`APPLICATIONS.md` §L, "Firefox runs under external service UID
65536"), return to their reserved accounts.

## Where things stand

| Workload | Account today | Started by |
| --- | --- | --- |
| Firefox | `tda65536` | td-authd `application-start` |
| mail, news | the human (1000); 65537 and 65538 stay reserved | `application-start` or the human's shell |
| td-web | `tda65540`, with a fetch service of its own; on a host, the human | `application-start` from its launcher card; on a host, `./install-apps`, unjailed |
| td-sandbox | `tdw<uid>`, the sandbox's own, from 98304 up | `td-sandbox` → root's workspace intake → `td-jail --workspace` as the account |
| td-agent conversation tool jails | a per-conversation `agent-N` ledger account on td; the caller on a development host | td-agent → root intake → td-login → td-jail |
| td-agent reviews | the human until the review cutover | td-agent → `td-jail --workspace` |
| td-agent unconfined workspace | the human, no jail | td-agent `--unconfined` |

Already built and reused by this design:

- the workload account ledger `/var/lib/td/accounts.tsv`, the one source
  of an application's uid, which firstboot seeds with the carried rows
  and the retained reservations of `/var/lib/td/principals.tsv`, in
  which td-software's install of a Flathub application allocates its
  row through td-authd, and which never reuses a uid; td-login resolves
  application accounts from it and every application identity consumer
  reads it (td-authd/DESIGN.md, "Workload account ledger");
- td-jail's mapping of an external uid to 1000 inside the jail;
- the workload grant table `/var/lib/td/grants.tsv`, root's alone,
  whose directory rows keyed by account td-authd publishes as idmapped
  views at each application launch and lists in the account's manifest,
  seeded once with Firefox's writable Downloads (td-authd/DESIGN.md,
  "Workload grant table" and "Application filesystem grants";
  `UNSAFE.md` §16); td-jail binds only the views a manifest lists, each
  compared with the identity td-authd published it by, one derivation
  (`engine/src/mount_identity.rs`) for both: the mount covering the
  human's home, the home's path below it and the row's path;
- the request path that lists, adds and revokes an application's or a
  sandbox's directory rows and sets a sandbox's network row: the human's
  `grants`, `grant`, `revoke` and `network` on the compositor control
  socket (td-ctl, and for applications td-software's folders panel),
  forwarded as td-authd's private request `2c` to td-firstboot's
  `grant-table`, the table's one writer, which admits a sandbox only
  once the ledger holds its account (td-authd/DESIGN.md, "Grant
  requests"); td-sandbox's `share`, `unshare` and `network` reach the
  same writer through root's workspace intake;
- the portal's fixed read-only Downloads view (1000→991), present while
  Firefox holds the Downloads grant;
- the signed Flathub runtime fetch and its generations, in the runtime
  store below, and the leases a launch holds on them (`APPLICATIONS.md`
  §W.9);
- td-jail's one runtime mount at `/usr`, shared by the application kind
  and the workspace kind, whose spec may name a runtime (`runtime=`) and
  which td-sandbox sandboxes set with `td-sandbox runtime` and td-agent
  templates with their `runtime` key;
- td-portal's caller identity from the bus broker's uid and jail lineage
  (`APPLICATIONS.md` §E).

Step 7 landed: each sandbox runs as an account of its own, `tdw<uid>`
from 98304 up, which root's workspace intake allocates through
`td-firstboot allocate-workload 1000 sandbox NAME` with an empty home
under `/var/lib/td/workloads` and a `td-workload-UID` delegation, and
which td-login resolves from the ledger alone (td-authd/DESIGN.md,
"Workload account ledger"). td-sandbox asks the intake to create and
delete sandboxes, change their grants and network, and start a shell or
command in one; root starts it as the account in td-jail's workspace
kind with the sandbox's directory rows published as executable views and
its network row applied (td-authd/DESIGN.md, "Workspace launch intake";
td-sandbox/DESIGN.md). A deletion clears the name's rows, empties the
home as the account and marks it deleted, keeping the ledger row; root
never creates over a sandbox holding files or grants, which td-sandbox
adopts instead. The profiles td-sandbox kept before are imported once,
their old homes left in place (td-sandbox/DESIGN.md, "Import").
td-sandbox calls no td-jail, and td-jail refuses td's account the
sandbox's offline profile.

Confined td-agent conversations use root's workspace intake and reusable
ledger accounts, with private scratch and per-launch directory plans.
Reviews still use the human's caller class until their account cutover.

Not built: subordinate accounts, the `map-subuid` operation
`APPLICATIONS.md` names, and socket grants, whose rows the table already
parses and no request writes. Workspace directories travel in each launch
plan rather than through persistent grant-table requests.

## A launch

Every workload launch combines the same four parts:

| Part | What it is | Who may write it |
| --- | --- | --- |
| Runtime | read-only trees at `/usr` (and `/app` for a Flathub application) | the runtime account, through td-app |
| Account | the external uid the workload runs as; 1000 inside its jail | nobody; root allocates it |
| Home | the account's private home | the workload |
| Grants | the human's directories and sockets the human granted | root, through td-authd |

What differs between Firefox, mail, a sandbox and an agent workspace is
only what fills each part and which td-jail launch kind applies its
policy.

## Runtimes

A runtime is a read-only tree set, versioned, stored once, and shared by
every workload that names it:

- **native**: no `/usr`; td's `/bin` and `/td/store`, as today's
  source-built applications and workspaces see;
- **a Flathub runtime**, such as `org.freedesktop.Platform//25.08`, at
  `/usr`, with td-jail's `/bin /sbin /lib /lib64` aliases into it for
  application launches and `/lib /lib64` only for workspace launches,
  where td's `/bin` stays first on `PATH`;
- **a layered application**: a Flathub runtime at `/usr` plus the
  application's tree at `/app`.

Runtime declarations are a table of their own, `FLATHUB_RUNTIMES`
(`engine/src/flathub_app.rs`): each runtime's ref and its td name, which
its environment policy and, with the application, its dynamic-library
policy are keyed by. Every Flathub declaration uses the one repository
constant. Application declarations name the runtime they accept.
Workspaces name a runtime by its td name (td-sandbox/DESIGN.md,
"Runtime"; td-agent/DESIGN.md §8, Runtime; `APPLICATIONS.md` §C) and
install none: a runtime is installed and kept only by an application
that names it, and a workspace launch finding no `current` is refused,
naming those applications. The table
also declares `org.freedesktop.Sdk//25.08` (`freedesktop-sdk-25-08`),
which no application names, so a workspace naming it is refused as not
installed until a runtime install path exists; it has no environment or
dynamic-library policy, which are keyed by application.

Runtimes live in one store, `/var/lib/td/runtimes/`, one store per
runtime named by its td name, owned by the runtime account `td-runtimes`
(UID 998, the desktop's alone) and written only by td-app running as it,
with §W.9's layout and admission rules 1–3 and 5. Deploy trees are
readable by every account. A workload can therefore no longer write its
own runtime. td-firstboot makes the store root and the account's cgroup
delegation; root's `runtime-cache` operation, and `application-cache`'s
install, update and remove, are the only ways anything runs as the
account (td-authd/DESIGN.md).

Each runtime keeps its `current` and `previous` generations and any
generation a running launch or an application's validation holds (§W.9's
shared `flock`). Applications and workspaces follow `current`; installing
or updating an application checks it against the runtime's `current`
under rule 4 and rule 5's dynamic validator, and an application whose
runtime's `current` moved revalidates offline at its next launch or is
refused; no application pins an older runtime commit. Installing an
application installs or updates its runtime first; removing the last
installed application that names a runtime empties that runtime's store.

Runtimes are never recipe inputs and are outside every closure, as §W.9
already requires of Flathub bytes. No Flathub byte enters the image.

## Accounts

Every workload account comes from one root-owned ledger, keyed by kind
and name (`application firefox`, `sandbox default`, `workspace <id>`).
td-authd allocates the uid when the workload is first installed or
created, on the human's request; no caller supplies a uid. A uid is never
reused, after removal included, which the reservation ledger already
guarantees. An application is allocated when td-software installs it, not
when the image declares it.

The uids the image reserves today carry over as rows already present:
Firefox 65536 and td-web 65540, and mail 65537 and news 65538, which
return to use. A
workload's kind and name join the ledger to its policy: an application's
reviewed declaration in the image, or the grants the human made for a
sandbox or workspace. So the bus broker and td-portal take an
application's uid from the ledger, not from a build-time table.

A workload account's home is root-created at mode 0700:
`/var/lib/td/applications/<uid>` for applications, as today, and
`/var/lib/td/workloads/<uid>` for sandboxes and workspaces. A sandbox's
home is there; the per-profile homes td-sandbox kept before stay where
they were, untouched (td-sandbox/DESIGN.md, "Import"). A workspace's
account also has a scratch tree, `/var/lib/td/workload-scratch/<uid>`,
root's with four directories of the account's, the persistent `/tmp`,
`/var/tmp`, `/dev/shm` and `/run` of its jails, outside the home since a
jail's directories may not nest (td-authd/DESIGN.md, "Workload account
ledger").

The ledger is `/var/lib/td/accounts.tsv`, root:root mode 0644, with one
`KIND<TAB>NAME<TAB>UID` row per account (td-authd/DESIGN.md, "Workload
account ledger"). Uids are split by kind: applications take 65536 through
98303, so the reserved rows lie inside it, and sandboxes and workspaces
98304 through 131071. The application range's first 1024 uids, 65536
through 66559, are the carried block: the carried rows, the reservations
that predate the ledger, and the uid of any application a later image
ships, which takes its carried row from the block. An install allocates
from 66560 up and never enters it, so a carried row or retained
reservation can never meet an installed application's uid. Only an
account of the block can hold the human's legacy state, so boot
migrates only into one of those. A removed application keeps its row,
and its reinstall the same uid. A lost ledger forgets its allocations,
but an install never re-issues a uid whose home, cgroup or runtime
directory remains. Besides the four carried rows, the ledger imports
every application row the reservation ledger retains, so a former
install's Claude Code keeps 65539. Rows are keyed by kind and name,
not by human owner, sandboxes' and workspaces' as applications': a
second human would share them, and serving one is a later decision.
Sandbox and workspace rows stop at 2048 together and at 1024 each, so a
session that cycles sandbox names can spend neither the rows
applications need nor td-agent's workspace rows; a workspace's name is
one of td-agent's own slots, so spending those only makes slots
td-agent adopts (td-authd/DESIGN.md, "Workload account ledger").

## Grants

Everything a workload reaches outside its runtime and home is a grant:

- **a directory** of the human's: a detached idmapped bind mount mapping
  filesystem uid/gid 1000 to the workload's account, read-only or
  writable, and always nosuid and nodev. No row asks for execution: an
  application's views are always noexec, and a sandbox's or workspace's
  are executable, by its account's kind alone (`UNSAFE.md` §16).
  Rows are validated (account, directory, mode) triples. td's own trees
  in the home (`.td` and every top-level `.td-*`, `.cache`, `.config`
  whole, `.local/share`, `.local/state`), the sensitive trees td-agent
  refuses a workspace (credentials, keyrings, profiles such as `.ssh`,
  `.gnupg`, `.password-store`, `.mozilla`, `.var/app`) and where the
  human's programs live (`bin`, `.local/bin`, `.cargo/bin`) are never
  grantable, nor any ancestor of one; that is one list, the grant
  format's, which td-agent reads. One account's directories do not nest,
  and a path holds no control or bidirectional format character. A row a
  later deployment's rules refuse is kept inert, listed and revocable but
  never published, rather than failing the whole table;
- **a socket**: Wayland, the fetch service, and others named per grant;
- **network**: `off` or `host`: one
  row per sandbox or workspace account, which the human sets as a
  directory is granted, and no row for none. An application's network
  is its reviewed declaration's, never a grant.

The human requests a grant through the compositor control socket, as
td-software's install requests arrive. A grant is not elevation: it
delegates the human's own authority over the human's own files, so it
needs the human's credentials, not secure attention. td-authd keeps the
grant table root-owned and readable by root alone, performs the mounts at
launch below a root-owned tree outside every workload-writable home, and
writes each account a manifest of its views that only it and root can
read. The table's rows are root's alone, but a mounted view is visible
in the host's mountinfo to any unjailed process. A revoked grant's view
is released at the account's next launch, or at shutdown, whichever
comes first; until then the root mount stays, and a running jail keeps
what it was launched with. A workload
cannot grant itself anything; when it needs a file it asks td-portal
(FileChooser and its successors), whose dialog the human answers.

There is no FUSE document portal: `CONFIG_FUSE_FS` stays off, and grants
are mounts.

## Launch path

All workload launches go through root:

- applications: `application-start`, unchanged except that mail and news
  use `exec-service-as` with their reserved accounts;
- workspaces: a new operation that checks the workspace's ledger row,
  applies its grants, and runs `td-jail --workspace` as that account.
  td-jail's workspace kind then requires its caller to be a workspace
  account in the ledger rather than uid 1000. For a sandbox this is
  root's workspace launch intake (td-authd/DESIGN.md, "Workspace launch
  intake"): the human's connection, once admitted, becomes the jail's
  channel, and td-jail admits the account's caller class beside the
  human's (`APPLICATIONS.md` §C, "The `workspace` kind"). The account's
  uid is the jail's inside as well as out; uid 1000 inside comes with
  the jail's subordinate mapping, which is not built.

td-sandbox launches no jail itself; it asks root's intake for a launch,
as do td-agent's confined conversations. Reviews retain their old path
until the review cutover. td-agent's unconfined workspace, which runs tools as the human
with no jail, stays as the decision's one deliberate exception, with
what it holds (above); every other agent workspace is jailed under its
own account.

## Landing order

1. This document.
2. Split `FLATHUB_SOURCES` into runtime and application declarations. No
   behaviour change.
3. The runtime account, store and root `runtime-cache` operation; Firefox's
   runtime moves out of its account's cache in the same landing. Landed;
   until step 10 the runtime half of an install shows no progress in
   td-software.
4. One td-jail runtime mount shared by the application and workspace
   kinds; workspace `runtime=`. Landed: the shared mount, the workspace
   kind's `runtime=`, td-sandbox's `runtime` and td-agent templates'
   `runtime` key. A runtime a sandbox or confined conversation names
   runs as that workload's ledger account, with its grants,
   under its seccomp filter and outside the application kind's bus,
   portal and Wayland boundaries, and td's own glibc programs there can
   load its code (APPLICATIONS.md §C), so such a workspace is no host
   for td's control plane (AGENTS.md, "Trust zones"). The target above
   is unchanged.
5. The ledger allocator in td-authd, with applications moved onto it:
   the bus broker and td-portal read application uids from the ledger,
   and td-software's installs allocate. In three landings: the ledger's
   format and its seeding by td-firstboot (landed); every consumer moving
   to it at once, with the application rows leaving passwd, group and
   `/etc/td-principals.tsv` and the uid leaving the bus declarations
   (landed: td-login resolves `tda<uid>` from the ledger and refuses a
   passwd or group entry that speaks for one, and the broker, td-portal,
   td-jail, td-compositor, td-audio, td-authd, td-secret and td-firstboot
   join the image's declarations with the ledger, td-authd/DESIGN.md,
   "Workload account ledger"); then allocation at install (landed:
   td-authd's install has td-firstboot allocate a Flathub declaration's
   row from 66560 up and prepare its home, cgroup and runtime
   directory; an application the image ships keeps needing a carried
   row from the carried block, since nothing allocates at boot).
6. The grant table and the generalized idmapped grant (`UNSAFE.md` §16).
   Landed, in three landings. First, td-jail derives the identity a
   view of a human directory must show from the mount covering the
   human's home, the home's path below that mount and the directory's
   path below the home, so nothing is mounted
   at a granted directory to mark it, and init no longer binds
   `Downloads` onto itself (`APPLICATIONS.md` §C, "Filesystem grants").
   Second, the root-only grant table keyed by account, seeded once by
   td-firstboot with Firefox's writable Downloads, and per-launch
   publication: `application-start` publishes an application account's
   views and manifest, td-jail binds what the manifest lists in place of
   Firefox's fixed projection and its `xdg-download` permission, the boot
   unit that prepared Firefox's view is gone, and shutdown releases every
   view (td-authd/DESIGN.md, "Workload grant table"). td-authd publishes
   and td-jail admits by one shared mountinfo derivation; one row whose
   source is refused costs that row its view, not the launch; and td-agent
   reads its refused trees from the grant format. With Firefox's row
   revoked, FileChooser has no Downloads view. A rollback to the first
   landing re-grants Firefox's Downloads by its own boot unit, whatever
   the table says, and leaves the table and view tree unread. Third, the
   request path through the compositor control socket that lists, adds
   and revokes an application account's directory rows, from td-ctl and
   td-software's folders panel, through td-authd's request `2c` to
   td-firstboot's `grant-table` writer under the principal state lock,
   with a VM oracle that grants a directory to Firefox before its launch,
   checks the read-only idmapped view the launch publishes, and revokes
   it (td-authd/DESIGN.md, "Grant requests"). A revoke takes effect at
   the account's next launch.
7. Workspace launches through root; td-sandbox profiles move to ledger
   accounts and their `share` becomes a grant request. Landed in four
   landings:
   sandbox accounts, with nothing launched as one yet (landed: one name
   form, `tdw<uid>`, for the sandbox and workspace kinds, which share a
   range; td-login resolves it from a sandbox or workspace row alone and
   places it in `td-workload-UID/session`; every reserved-name list
   holds it; td-firstboot's `allocate-workload` allocates a sandbox's
   row from 98304 up, avoiding every kind's leftovers, and prepares its
   empty `/var/lib/td/workloads/UID` home and its delegation, but no
   `/run/user/UID`, td-authd/DESIGN.md, "Workload account ledger");
   grant requests that admit the sandbox kind (landed: `grants`, `grant`
   and `revoke` name a sandbox, and `network sandbox NAME off|host` sets
   its one network row, `host` written and `off` no row, which `grants`
   lists; a sandbox's grant or `host` needs its ledger row and the
   primary human's session, and never allocates a row; sandbox and
   workspace rows together keep to half the grant table, 2048 rows and
   512 KiB; a workspace and an application's network are refused,
   td-authd/DESIGN.md, "Grant requests"); the root workspace launch
   (landed: `td-authd workspace-serve`, root's daemon on
   `/run/td-authd/1000/workspace`, admits only the primary human's
   connections and one `start NAME RUNTIME TERM` request, requires the
   sandbox's existing ledger row and adds none, prepares the account's
   home and delegation again through `prepare-workload`, which refuses a
   row a deletion took meanwhile, refuses a named runtime that is not
   installed, publishes its directory rows as executable views, maps its
   network row to the development or development-offline profile in a
   root-owned spec, and runs `td-jail --workspace` as `tdw<uid>` through
   `td-login exec-service-as` with the connection as the jail's channel;
   td-jail's workspace kind admits a sandbox's or workspace's ledger
   account as a second caller class, with root's spec, the account's
   home and its manifest's views at the human's own paths,
   td-authd/DESIGN.md, "Workspace launch intake"); then td-sandbox's
   cutover (landed: the intake also creates a sandbox through
   `allocate-workload`, never over one holding files or grants, which
   td-sandbox adopts instead, deletes one by clearing its grant rows
   through the writer, having the account empty its home and marking it
   deleted, keeping the ledger row, lists and changes its grants and
   network, and runs a command as well as a shell; td-sandbox keeps only
   a record per sandbox and asks the intake for everything else, starting
   no jail; a one-time import creates each old profile whose name is a
   ledger name and no sandbox's yet, leaving its old home untouched and
   named by `inspect`, and reports and drops shares outside the home and
   names beginning with a digit or a hyphen; the client treats an end of the
   intake's connection before the relay's first frame as a failed start
   and names `/var/log/svc/workspace-intake.log`; td-jail refuses td's
   account the sandbox's offline profile; and the `sandbox-evidence`
   oracle runs a sandbox through the intake on a QEMU image,
   td-sandbox/DESIGN.md).
8. td-agent workspaces on ledger accounts. The repository user decided:
   only confined workspaces move, to `workspace` rows; the unconfined
   workspace stays as the human, the exception named above. A workspace
   account is a reusable per-conversation slot, `agent-N`: deleting a
   conversation empties its slot (its home and scratch tree, as the
   account) and clears its rows, freeing it, and the next conversation
   creates it again fresh, as a deleted sandbox is created again. The
   human's directories reach the account as per-launch views from
   td-agent's plan. A built-in "diagnose" template follows, and reviews
   move through root onto a `review-N` slot pool, retiring td-jail's
   uid-1000 workspace class. In six landings:
   workspace accounts in the ledger, with nothing launched as one (landed:
   `allocate-workload` and `prepare-workload` admit the `workspace` kind,
   only td-agent's slots, `agent-1` through `agent-960` and `review-1`
   through `review-64`, exactly the kind's rows, so a program spending
   them can only make slots td-agent adopts; the two kinds keep to 2048
   workload rows together and each to 1024; a
   workspace's account gets a scratch tree
   `/var/lib/td/workload-scratch/UID`, root's of the account's group at
   0710 with `tmp`, `var-tmp`, `shm` and `run` the account's at 1777,
   which the allocator's taken set counts as it counts homes, a new row
   refusing a leftover; `workspace-empty` empties the tree's
   directories as the account and leaves them at 1777, and the grant
   table's writer clears a
   workspace's rows; td-login already resolves `tdw<uid>` from a
   workspace row, td-authd/DESIGN.md, "Workload account ledger"); root
   launching a workspace instance (landed: the intake's `workspace
   create`, `delete`, `status` and `launch` verbs on td-agent's slots; a
   launch's plan of the human's directories by td-jail's roles,
   td-agent's own trees admitted in their exact shapes and roles alone;
   the slot account's views published from the union of its running
   instances' plans, held in root's memory, so none is released under a
   running instance, with no grant table row read or written and nothing
   moved; td-jail examining and binding, of a workspace account's views,
   only those its spec's role lines name, its repositories' git chains
   from the views, and admitting its own scratch tree's lines; the
   account prepared once a boot and again after a creation, a deletion
   or a failed instance; separate pools of 128 instances, 32 a slot; and
   a deletion, of a sandbox too, refused while a launch or instance of it
   runs, that first ends what they left in the account's delegation
   through `cgroup.kill`, td-authd/DESIGN.md, "Workspace instances"); td-agent's intake client (landed, unused: typed slot operations and
   role-validated launch plans, with td-sandbox's socket transport shared
   in engine); td-agent's conversation cutover and VM oracle (landed:
   durable slot reservations, tools and maintenance through root, deletion
   and scratch reuse, and a disconnect monitor independent of the entry's
   reader); reviews on the `review-N` pool through root; and the
   diagnose template.
9. mail and news on their reserved rows, with fetch services of their
   own as td-web has (td-web/DESIGN.md §3).
10. td-software's runtime view (its folders panel landed with step 6).

## Open questions

- How the human reads a sandbox's or workspace's home: a reverse idmapped
  view, as the portal's handoff maps Firefox to the portal, or only
  through grants the workload already writes into.
- Which human sockets an agent workspace may be granted (the SSH agent,
  git credential helpers), and whether any may be.
- Whether a workload's request for a directory grant is answered by
  td-portal's dialog alone or also recorded as a standing grant.
