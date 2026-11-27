# Native remote desktop

The viewer attaches to the primary account's running graphical session.
A terminal open before the connection is the same terminal during it.
Connecting, using the desktop and disconnecting must preserve an existing
physical lock. An unlocked monitor stays visible for shared use. A physical
lock or unlock during the connection changes only local presentation and
local input admission. The viewer uses application focus while physical
input stays with the local console.

## Connection and authority

Run `td-desktop [user@]host` on a td or foreign Wayland desktop. The
viewer uses td-ui and is an ordinary binary in the host's `./install-apps`
set. Its td-remote SSH child starts a fixed helper on the td machine. SSH
owns host identity verification, account selection and key authentication.
An existing primary-account login key suffices. An SSH restriction that
prevents executing the helper also prevents attaching through that key.

The helper connects to a compositor-owned Unix socket in the installed
session. The compositor checks the kernel peer credentials for the primary
human. Confined workload UIDs and the administrator UID receive no desktop
connection. A local process running with the human's full credentials has
the same ordinary desktop authority as their SSH helper. The socket belongs
to one compositor generation. A viewer needs a new connection after a
restart. The compositor admits one viewer at a time. The live installer has
no remote desktop endpoint. Only an installed boot can publish it.

The remote protocol carries ordinary desktop operations only. Privileged
remote actions continue through the fixed commands of the separately
keyed td-admin SSH account. The viewer cannot enter secure attention,
acknowledge consent, enter a hardware PIN, unlock the console, or supply a
presentation receipt. The existing headless automation channel keeps its
test-only input and capture restrictions.

## Display and input

Render the current workspace from ordinary client surfaces into separate
viewer storage. Physical scanout continues through the lock and trusted
presentation path. A locked monitor retains its lock screen through
viewer attach, input, clipboard changes and disconnect. A physical unlock
continues to require the machine's local authentication. A local secure
attention interaction owns its screen, keyboard and pointer throughout.

Give the remote input source its own held keys, modifiers, pointer buttons
and focus lifetime. Physical input on a locked console can operate only
the lock interface. Remote key and pointer events may reach ordinary
applications on the visible workspace. They must never be dispatched as
physical device reports. Trusted prompts and the trusted notebook's
credential window require exclusion from both remote pixels and input.
Resolve protected surfaces by trusted compositor or launch authority.
Root gives the notebook the protected Wayland socket. The server withholds
the remote seat while a client on that socket can own a surface.

Coordinates refer to the remote output. The viewer scales the image to fit
and maps pointer coordinates back through the same transform. Input aimed
at a superseded output geometry is discarded. Losing viewer focus, ending
the link or entering a protected interaction releases held input and cancels
grabs. A replacement connection starts with all keys and buttons up.

## Clipboard and flow control

The first clipboard format is UTF-8 text. F9 copies the remote selection into the viewer machine’s clipboard.
F10 sends the viewer machine’s clipboard to the remote selection. Paste
in the destination application to insert the transferred text. Ordinary copy
and paste inside a remote application continue to use its own Wayland
selection. Request clipboard data only for the chosen transfer and bind it
to the connection, selection revision and eligible source. Recheck these
before delivery so a focus change or protected prompt during a delayed
transfer cannot disclose its selection.

Bound output dimensions, frame bytes and clipboard bytes before allocation.
Limit frames in flight and coalesce stale pictures. Preserve input ordering
and give releases and disconnect a bounded path even on a slow display
link. The compositor waits for the SSH helper between requests without
a frame-rate deadline. Partial requests and socket writes have deadlines.
Connection loss invalidates transfer state. The viewer reports SSH
and host-key errors through the existing td-remote error path.

## Acceptance criteria

- Boot an enrolled guest to its lock screen. Open the viewer over real SSH
  without local interaction. Type a fresh marker into an ordinary terminal
  behind the lock. Observe the marker in the viewer and inspect an independent
  capture of physical scanout for the lock surface. Render the same guest
  after disconnect and verify the physical lock still holds.
- Inject a key through the guest's physical device while the viewer has
  terminal focus. Verify it never reaches that terminal. Invoke local
  secure attention and exercise a PIN step. Verify the viewer cannot see
  that step, send input to it or satisfy its presentation checks.
- Exercise clicks, drags, modifiers, focus loss during a held key, viewer
  disconnect during a drag, and a reconnect. Verify remote controls have
  no lingering effect on local input.
- Transfer distinct multiline text in both directions. Exercise an oversized
  selection, a replaced source, an interrupted transfer and a protected
  window. Verify the receiving side only publishes the intended text.
- Exercise enrollment changes, a second viewer, a confined workload, a
  refused SSH key and the td-admin login. Verify account separation through
  actual processes and sockets. Exercise a host-installed native viewer
  against the installed guest helper.
