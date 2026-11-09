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
(mail, news, td-web), every td-sandbox profile and every td-agent
workspace.

## The decision

No workload runs as the human. Each runs as an account of its own, sees a
read-only runtime, owns a private writable home, and reaches anything of
the human's only through a grant the human made and td-authd performed.

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
| td-sandbox | the human: "It creates no account and gains no privilege" | `td-sandbox` → `td-jail --workspace` |
| td-agent tool jails | the human (workspace kind requires uid 1000) | td-agent → `td-jail --workspace` |
| td-agent unconfined workspace | the human, no jail | td-agent `--unconfined` |

Already built and reused by this design:

- reserved application uids in `/etc/td-principals.tsv` and the firstboot
  reservation ledger `/var/lib/td/principals.tsv`, which never reuses a
  uid (td-authd/DESIGN.md);
- td-jail's mapping of an external uid to 1000 inside the jail;
- root-made, value-pinned idmapped views of the human's files:
  `prepare-portal-files` (Downloads, 1000→991, read-only) and
  `prepare-application-files firefox` (Downloads, 1000→65536)
  (td-authd/DESIGN.md; `UNSAFE.md` §16);
- the signed Flathub runtime fetch and its generations, in the runtime
  store below, and the leases a launch holds on them (`APPLICATIONS.md`
  §W.9);
- td-portal's caller identity from the bus broker's uid and jail lineage
  (`APPLICATIONS.md` §E).

Not built: subordinate or dynamically allocated accounts (td-software/DESIGN.md:
"no allocation of accounts at run time"), the `map-subuid` operation
`APPLICATIONS.md` names, and any grant other than the fixed Downloads views.

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
Workspaces name a runtime by its td name.

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
`/var/lib/td/workloads/<uid>` for sandboxes and workspaces. td-sandbox's
current per-profile home moves there. The allocation range and the
ledger's row format land with the allocator.

## Grants

Everything a workload reaches outside its runtime and home is a grant:

- **a directory** of the human's: a detached idmapped bind mount mapping
  filesystem uid/gid 1000 to the workload's account, read-only or
  writable, nosuid, nodev, and noexec unless the grant says otherwise.
  This generalizes `prepare-application-files firefox` from one fixed
  application and directory to validated (account, directory, mode) rows;
- **a socket**: Wayland, the fetch service, and others named per grant;
- **network**: `off` or `host`, as td-sandbox's policy has today.

The human requests a grant through the compositor control socket, as
td-software's install requests arrive. A grant is not elevation: it
delegates the human's own authority over the human's own files, so it
needs the human's credentials, not secure attention. td-authd keeps the
grant table root-owned and performs the mounts at launch. A workload
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
  account in the ledger rather than uid 1000.

td-sandbox and td-agent stop launching td-jail themselves; they ask for a
launch. td-agent's unconfined workspace, which runs tools as the human
with no jail, is retired: every agent workspace is jailed under its own
account.

## Landing order

1. This document.
2. Split `FLATHUB_SOURCES` into runtime and application declarations. No
   behaviour change.
3. The runtime account, store and root `runtime-cache` operation; Firefox's
   runtime moves out of its account's cache in the same landing. Landed;
   until step 10 the runtime half of an install shows no progress in
   td-software.
4. One td-jail runtime mount shared by the application and workspace
   kinds; workspace `runtime=`.
5. The ledger allocator in td-authd, with applications moved onto it:
   the bus broker and td-portal read application uids from the ledger,
   and td-software's installs allocate.
6. The grant table and the generalized idmapped grant (`UNSAFE.md` §16).
7. Workspace launches through root; td-sandbox profiles move to ledger
   accounts and their `share` becomes a grant request.
8. td-agent workspaces on ledger accounts; the unconfined workspace is
   retired.
9. mail and news on their reserved rows, with fetch services of their
   own as td-web has (td-web/DESIGN.md §3).
10. td-software's runtime and grant views.

## Open questions

- How the human reads a sandbox's or workspace's home: a reverse idmapped
  view, as the portal's handoff maps Firefox to the portal, or only
  through grants the workload already writes into.
- Which human sockets an agent workspace may be granted (the SSH agent,
  git credential helpers), and whether any may be.
- Whether a workload's request for a directory grant is answered by
  td-portal's dialog alone or also recorded as a standing grant.
