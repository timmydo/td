# td-remote

td-remote carries bounded messages between a td program on this machine and
its counterpart on another, over the standard streams of an `ssh` child, for a
td program's remote window. What the messages mean, and what the far side keeps,
are each program's own.

It is `std` plus td's `td-fs`, forbids `unsafe`, and opens no socket: ssh is
the only network code, the only login and the only thing listening. Nothing
here makes a td program a server anyone must run (`AGENTS.md`, principle 5):
the far side is a child of the user's own ssh session, or a process of their
own account that such a child reaches.

## The ssh child

`Link::ssh` runs `ssh OPTIONS -- HOST PROGRAM WORDS…` and nothing else.

- `Host` is `[user@]name` of letters, digits and `. - _ @ :`, neither part
  empty or starting with `-`, so no ssh option can pass as a host, or as the
  name a configuration hands to its `ProxyCommand`, whatever a caller is handed.
- sshd gives the remote command to the login shell as one line. Only fixed
  `&'static` words and a `RemoteProgram` cross that way. A `RemoteProgram`
  admits letters, digits and `/ . - _`, so the shell reads it literally; a path
  relative to the account's home reaches a program that a non-interactive
  `PATH` does not. A file's name or bytes travel in frames, never as an
  argument.
- `-T` keeps a terminal out of a binary stream, whatever the configuration's
  `RequestTTY` says. `-x`, `-a`, `ClearAllForwardings` and `RemoteCommand=none`
  keep configured forwardings and commands out of a link that wants none. The
  agent is not forwarded to the far side. `ControlPath=none` gives the link a
  connection of its own, at the price of authenticating each, since a shared
  master would hold it to that master's keepalives. `ConnectTimeout`, and the
  same `ServerAlive` values as td-agent's git transport, notice a dead peer in
  about a minute.
- A window has no terminal for ssh to ask on. Unless the environment already
  holds a non-empty `SSH_ASKPASS` or `SSH_ASKPASS_REQUIRE`, a choice the person
  made, and when there is a `WAYLAND_DISPLAY`, ssh asks through `td-pinentry`:
  beside this executable, or where `./install-apps` puts it, or on `PATH`.
  Without one ssh asks on the controlling terminal if the program has one, a
  later reconnect included, or else fails and says why, and the error carries it.

What ssh writes to its standard error is kept, its last KiB, for the error a
closed link reports: a person sees `td-x: command not found` or `Permission
denied`, not a broken pipe. Another machine's words reach a window through
`printable`, one line with its controls and its invisible format characters,
bidirectional overrides among them, replaced.

## Handshake

Each side writes before it reads: the magic `td-remote 1\n`, then one frame
holding a version and a program name. Both must match exactly, and a mismatch is
refused on both sides naming both programs or both versions. There is no negotiation: a program
whose protocol changes bumps its version, and the person installs the same td
on both machines.

A login shell's startup file can write to standard output ahead of the program,
which would otherwise read as a length. The magic is why that is told apart:
the first read that departs from it ends the link with `Error::Garbage`, quoting
what had arrived and naming the cause, with no further read a silent far side
could hold up. What arrived past the magic goes on to the framing that follows.
A connection cut during the magic or the hello is a closed link, which ssh's
words explain.

## Messages

Frames are a big-endian four-byte length and at most 1 MiB, refused before
anything is allocated. A frame holds a stream, flags, and a chunk of at most
256 KiB of one message; the last chunk says so, and only the last may be empty.
Reassembly holds each stream's message to the link's `max_message`, which ends
the link when passed. A stream the program did not name, unknown flags or a
chunk of another size end it too.

- **Streams overlap.** Senders take fair, first-come turns at the writer, per
  chunk. An ordinary lock would let a large message's sender take it again ahead
  of a waiting small one, and a 16 MiB save would stall a directory completion.
  Two messages on one stream go one after the other, since only their order
  tells a stream's chunks apart. A failed write may have torn a frame, so
  nothing is written after one.
- **No waiting on a consumer.** One reader serves every stream, so it never
  waits: a stream's inbox holds `Options::unread` whole messages, one for request
  and reply, and one more than that ends the link as a protocol error. Why a link
  ended is kept beside the inboxes rather than queued in them, so a full one
  cannot keep it from another. What a far side can make a near side hold is that
  bound, a partial message per stream, and `max_message` each.
- **Codec.** `codec` is fixed-width big-endian integers and length-prefixed
  bytes. Its reader takes a bound for each length from its caller, refuses a
  byte string past its bound or past the message, a list's count past its bound,
  and trailing bytes.

The far side's messages are untrusted input to the near side. A machine you can
ssh to can make a link fail, and can show you any file it likes; a program must
not let it do more. That is each consumer's decoder's duty as well as the
bounds' here.

## Reconnecting

`Client` owns a `Connector` and makes a new link when asked after the last one
was lost. It never reconnects on its own, so an idle window holds no retry loop
and asks for no passphrase unbidden.

Each connection has a generation. A far side that keeps per-connection state
lost it, so its client must notice before asking again: `request_on` takes the
generation the caller prepared for and answers `Stale`, sending nothing, for
any other. One thread's reconnect therefore cannot put another thread's next
request in front of a far side that does not know what it refers to.

`RequestError` keeps apart what a person must be told differently. `Unsent` and
`Stale` mean the far side heard nothing, so the operation did not happen.
`Lost` means the request was on its way, so it may have happened. A link whose
reader has seen it end, or whose last write failed, is noticed before anything
is sent, so it costs no request an unknown outcome; until ssh's keepalives notice
a dead peer, about a minute, a request can still go into one and be `Lost`. A
failed write says why the link ended rather than `Broken pipe`, waiting a moment
for the reader to hear from ssh. A request queued behind another that lost the
link is `Stale`. A message nobody asked for is a
protocol error, found before the next request can take it for its reply, and
reported once rather than replaced by a new connection in silence. A program that
can no longer tell what a far side holds gives the connection up (`abandon`).

A connection that takes a long time, as a person answering a passphrase does,
holds up the threads that need that connection, and only those; if it fails, they
all hear so, and the person is asked nothing twice. A program calls `Client` from a
worker, never from the thread that draws. Neither a handshake nor a request has a
deadline, since either may be a person answering ssh, or a slow disk: ssh's
keepalives end a dead connection, and a far program that hangs with its connection
alive holds its caller as a hung local disk would. Only the person ending the `ssh`
child frees it; `Client` offers no way to, since no program has yet asked for one.

## Not here

- No listener, no TLS, no authentication: ssh's.
- No daemon or multi-client policy: a program that wants its far side to outlive
  the connection owns that.
- No compression or delta: whole messages, which ssh may compress if configured.
- td-agent's git transport still spells the ssh keepalive values itself; its
  remote window will share them with `Link::ssh`. Its own framing is this crate's.
