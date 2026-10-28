# Offline graphical installation

This is the contract for td's offline graphical installer. It extends
[DESIGN.md](DESIGN.md), which owns the disk layout and the single
deployment publisher. Version 1's profile is active, which claims this:
`./build-iso` composes its installation image ([README](../README.md),
[MEDIA.md](MEDIA.md)), and that image installs td onto a disk under QEMU
with UEFI, as increment 7's end-to-end evidence shows. `qemu-boot-live` is
that evidence, run on demand rather than by `check` or `ready`; it boots
the medium as USB storage, `./test-iso` boots it as optical media by hand,
and `qemu-boot-media` and `qemu-install-system` boot their own media both
ways. Version 1 fixes the keyboard layout at `us`: a choice of layout waits
for a keyboard catalog, a later increment. From ENCRYPTION.md increment
7, the selector's disk-recovery prompt reads the kernel's US keymap
whatever layout is chosen ("Keyboard console"). Every home-path consumer
now follows the installed account ("Media, boot and persistence").

## Version 1

Produce one hybrid ISO that boots through x86-64 UEFI both as optical media
and when flashed byte-for-byte to a USB drive. Boot into td-compositor and
an automatically opened native Rust installer. Installation uses the
deployment carried on that media, with no downloads, package selection,
network setup, or source compilation on the destination machine.

The first supported test platform is QEMU with UEFI firmware. ThinkPad
T430s hardware validation is the next milestone, not a v1 hardware claim.
Its drivers are built into the kernel ahead of it, each checked after
config resolution by the linux-x86-64 recipe: i915 display, the 82579LM's
e1000e, the Synaptics touchpad and TrackPoint, thinkpad_acpi with battery,
AC, buttons, thermal, fan and backlight, CPU frequency and idle drivers,
suspend, EHCI for its USB 2.0 port, the Ricoh SD card reader and the
Conexant and Intel HDMI audio codecs. QEMU exercises their shared ACPI,
input and power-management code but none of the laptop's devices; only the
T430s itself can show those work. A card in the built-in reader is readable
but never an installation target. The compositor recognizes the touchpad
from its sysfs properties and moves the pointer by one finger's travel, as
a mouse does; its button presses click, and tapping and scrolling are not
yet read. The TrackPoint moves it relatively. Its Intel Wi-Fi needs
firmware no reviewed pin carries yet (AGENTS.md, "Device firmware"), so
the wired port is its network.

The ThinkPad T14s Gen 4 AMD is a second hardware target on the same terms:
its drivers are built in and checked after config resolution ahead of
validation on the machine. Beside the T430s's thinkpad_acpi and ACPI
platform set, PS/2 keyboard and TrackPoint, and the NVMe and xHCI every
installation medium needs, it has amd-pstate, the AMD ACPI platform devices
with their GPIO controller and DesignWare I2C, the HID-over-I2C touchpad
through hid-multitouch, the Realtek codec, x2APIC, which firmware that hands
over in x2APIC mode needs, and the AMD IOMMU, for interrupt remapping and
DMA isolation.
Its Phoenix Radeon is driven by AMDGPU and DC with twelve reviewed
linux-firmware files embedded in the kernel, including DMCUB for display
power management. AMD PMC, the CMOS RTC and PCIe ASPM following firmware
policy supply the s2idle platform support. Physical secure attention offers
`Z: SLEEP`, then Enter, with a presented lock before sleep for enrolled
sessions. The compositor retains its single-output KMS profile; this is not
external-display or hotplug support. Its internal microphones are on the ACP,
whose audio drivers are not built. Suspend/resume is a hardware-validation
target, not yet an acceptance claim: test repeated short and long sleeps on
AC and battery, panel recovery, keyboard/TrackPoint/touchpad, Wi-Fi, token
unlock and the kernel's hardware sleep statistics on the machine. It has
no wired port. Its Qualcomm WCN6855 Wi-Fi is its network: ath11k is
built in with linux-firmware's pinned files embedded in the kernel
(AGENTS.md, "Device firmware"), and `td-netd wifi join` names the network
("Installed Ethernet and remote access"). Only WCN6855 silicon is carried
(QCA2066 and QCA6698AQ share its PCI ID), and its board data is chosen
by the card's PCI subsystem ID from `board-2.bin`, which holds Lenovo's
17aa:9309 and has no generic fallback; a card it lacks logs "failed to
fetch board data" and brings up no interface. This has not yet been
tried on the hardware.

Legacy BIOS, resizing, dual boot and RAID are outside v1. Preserving an
existing td installation is limited to the explicit reinstall mode below.
For a new installation, one selected whole disk is reformatted, not
wiped (ENCRYPTION.md "Device-bound formatting"), and receives GPT, a FAT32
ESP and the td Btrfs volume.

The wizard collects one human username, hostname and timezone; the keyboard
layout is `us`, the only one version 1 admits. Storage is device-bound
where the live system has a usable TPM 2.0 and a keyboard console and
unencrypted otherwise (ENCRYPTION.md "Activation"), which the review
states with its reason, and the installed account automatically enters
the desktop either way. The welcome and final review screens disclose
those facts ("Storage choice"). There is no PIN field, password
substitute or inert enrollment screen. [ENCRYPTION.md](ENCRYPTION.md) owns
the later complete encrypted boot, hardware-backed PIN, recovery and
session-authentication cutover. No account secret is needed to install.
After installation the account may enroll FIDO2 login keys from its
desktop, which turns automatic login off
([td-login/TOKEN-LOGIN.md](../td-login/TOKEN-LOGIN.md), implemented,
and not usable until the acceptance evidence its opening names);
the installer collects no key and its disclosures describe the installed
default.
Networking can be configured after booting the installed system, and
the Wi-Fi networks joined in the live session before installing are
normally already there: the installation carries td-netd's saved
networks onto the installed `/var` when their file is as td-netd keeps
it (DESIGN.md "Carried Wi-Fi networks").

## Reinstall while keeping personal files

On welcome, the Reinstall button (or `r`) chooses reinstall; Fresh install
(or Return) chooses whole-disk installation. Both are clickable footer
buttons. Reinstall lists eligible disks through the same media exclusion
and exclusive-claim checks. Return on the chosen disk asks root to inspect
its existing installation. It skips editable account settings. The review
names the saved account, hostname, timezone and unchanged filesystem UUID,
and says personal files and settings will be kept. The installer review
explicitly says reinstall, never erase. Enter confirms that exact
mode and review; Escape cancels. A request for one mode cannot execute a
review for the other.

This increment supports existing unencrypted, single-device td Btrfs volumes.
A LUKS header is refused before mounting or writing, with an explicit service
log; encrypted recovery is not implemented. Neither mode falls back to the
other. Reinstall requires readable saved username, hostname and timezone,
trusted directory ancestors and a real UID-1000 home. Missing or malformed
saved account state is refused; it is not guessed or reset.

Inspection mounts the claimed volume read-only with `rescue=nologreplay`, `nodev`,
`nosuid`, `noexec` and `subvolid=5`. The kernel-mounted filesystem must name
the observed UUID, the td label and only the loop device backed by the
exclusive disk claim. Execution re-inspects and compares the saved settings
and UUID with the consented plan before a writable mount. It checks the
existing ESP's capacity before changing persistent state.

Reinstall never runs a formatter, writes GPT, deletes `@var`, recreates a
home or applies new account settings. It preserves all of `@var` except its
temporary replacement payloads. A fixed root-private staging directory,
`@var/lib/td-install/reinstall` (0700), is reclaimed on retry before
copying, and a stage an older reinstall left elsewhere is left alone;
unknown entries, links and subdirectories refuse cleanup before any staged file is removed. Source
payloads are copied beneath it and fully verified before the boot publisher
is invoked.
The publisher replaces both system selections, including damaged payloads
with the same deployment ID, using its existing verified bundle writer.
Old system rollback is not retained by a reset. The filesystem is
unmounted, and the existing ESP receives the kernel and a selector with the
unchanged UUID through td-fat.
No partition, encryption header, user file or saved login credential is reset.

Interruption can leave the system unbootable until reinstall is retried from
the USB. Personal files remain in place, and the same mode can replace
incomplete boot state. This is not an atomic boot-chain rotation or a
backup. Scratch cleanup never recursively walks a mountpoint. Failed
unmounts are reported and leave their mounts held.

Protocol request `0x0b` carries a destination observation only. Root supplies
the saved settings and UUID. Preserve plans have the unencrypted storage and
zero basis; these are existing-storage facts, not a new TPM probe decision.
The private consent review appends a boolean preserve byte after storage,
and its greeting is `TDINA03\n`. The ordinary service greeting is
`TDINS09\n`. The mode is part of the reviewed plan and exact Execute bytes.

The private staged manifest must hash to the deployment ID bound by the
reviewed plan, and full staged payload verification follows, so changing
removable media cannot substitute another manifest or payload bundle
between initial validation and copying.

The live QEMU oracle installs, writes a personal file through the installed
account, boots the same disk from USB into reinstall, and boots it again.
Its post-reinstall checks require the same filesystem UUID, host identity,
home inode, personal-file contents and saved account settings. Host tests
also cover a damaged same-ID payload, repeated reset, and a changed
manifest's refusal before replacement.

## User flow and authority

The sequence is welcome, destination disk, account and regional settings,
review, installation progress, and completion. Back preserves valid input.
Errors remain visible and explain the failed operation. The review includes
the exact disk identity and capacity, the selected settings, and an explicit
confirmation that all data on that disk will be lost for a new installation;
reinstall instead skips editable settings and confirms personal-file preservation. Success offers an
orderly reboot or power-off with an instruction to remove the installation
media.

The UI is a td-owned, dependency-free Rust Wayland client. Follow td-editor's
software rendering, font and input conventions where useful; a general UI
toolkit and GPU renderer are not prerequisites. Shared source reuse must
retain its owners' contracts, and any syscall boundary needs its own
UNSAFE.md authorization. No browser, webview or HTTP service is required.

The UI runs without disk-writing privileges. A root-owned installation
service admits only typed installation operations over a private local
channel. The UI cannot select executables, shell commands, arbitrary paths,
mount options or a different deployment source. Only the live wizard
reaches this service: it runs as its own service identity, `tdi1000`
(UID/GID 990), and td-authd's setup intake admits that kernel peer UID
alone (td-authd/DESIGN.md "Whole-disk installation intake"). This authority
does not depend on `su`, empty passwords, or a reusable elevation grant.
On trusted live media the wizard's final review is the human confirmation.
Return or the confirmation button starts the exact reviewed plan; no separate
secure attention screen is used. The review must have been painted for at
least 500 ms, with the same plan nonce, before keyboard or pointer confirmation.
Return does not repeat. This prevents accidental early input, but does not
claim a secure-input boundary: the trusted wizard and its ordinary input are
part of the installer authority. Installed-system elevation is unchanged.

The `td-setup` front end has a source-built static target recipe and
`td-setup-test` realized-output check. The recipe stages its own tree with
`td-install`, `td-ui` and the compositor sources that the toolkit mounts,
`td-protector`, whose recovery-key codec alone td-setup names, and its
dependencies (td-tpm, td-json and the engine SHA-256 td-tpm includes),
then builds with the target Rust toolchain. The check runs `--help`,
`--font-license` and a headless render of every page type at the reference
800x600 size and the compositor's 752x508 tile, including all destination
and review detail pages of both storages, every progress outcome and the
recovery key's pages, without emitting image bytes. The connected and
pure views share a 752x480 minimum extent, while welcome keeps its own
content-dependent layout. The render data is synthetic
and grants no authority. This establishes an image-eligible executable whose
window presents welcome and, on Return, connects to td-authd's setup intake
(td-authd/DESIGN.md "Whole-disk installation intake") and asks the service
it starts for destinations, showing a waiting notice until the answer, which
the window takes at the end of every turn, idle or not. The service's list
follows, or its refusal in the installer's own words, or, when the intake is
absent or the connection ends, an explicit service-unavailable page; an
ended connection discards the listed observations. An absent intake fails at
once; otherwise a worker thread connects, requires the service's greeting
within ten seconds and does the blocking exchange, one request at a time, so
the window keeps drawing, and between requests it notices the service
closing. A reply has no time limit. The disks show in a listbox: a paper
ground inside a border, as the form's text fields have, each row the
disk's kernel name, its model (`model:` and the escaped value, `model
reported empty` or `model not reported`; an escaped value holds no
space, so it cannot read as either) and its capacity, and the selected
disk's row in the selection colour with paper ink. Until one is chosen
the detail under it says to click a disk or press Up or Down. Up and
Down move the list's navigation index, a left click on a listed disk's
row selects that disk as they would, PageUp and PageDown page a disk's
identity, and Escape returns to welcome; the key list names the click
beside Up and Down. From the list Escape keeps the connection for the
next request; while waiting it abandons the request and its connection,
so a service that never answers cannot hold the window, and td-authd
serves the next installer once it reaps that service. Disks the window
stopped waiting for are dropped. With a disk selected, Return continues
to the settings step described below and Escape there goes back to the
list, keeping the drafts.
The live window paints in td-ui's first theme, `SAND`, and F12, td-ui's
theme chord, moves it to the next for the wizard's run (td-ui/DESIGN.md,
"Themes"): it runs without a home, so it reads and keeps no theme file.
The window takes F12 before any page does, so it asks the service nothing,
and the headless renders paint in `SAND`.
A held key repeats at the delay and rate the compositor's keyboard
announces, through td-ui's repeat (td-ui/DESIGN.md), only where moving
or typing is what it repeats: Up, Down, Left, Right, PageUp, PageDown,
Backspace, Delete and a typed character, space included, on any page,
and the key list's reading keys while they scroll it. On a page Return,
Escape, Tab, Home, End, F1 and F12 act once a press, so a held key never
advances or leaves a step or asks the service anything twice. A page's
key repeats while held whether or not the page moves, as at a list's
end. A key that closes the key list, or one held through the click that
closes it, stops repeating, so it never reaches the page under the
list. A repeat fires only at the end of a turn whose queue drained,
after any release or focus change queued before it, and only for a
chord the list or the page still repeats.
F1, td-ui's key list chord, is the window's too (td-ui/DESIGN.md, "Key
list"): it opens the list of the pages' keys over the page, the shown
page's first (with the time zone row focused, its keys and then the
rest of the form's) and the window's F1 and F12 last, each page's rows
a table beside the code that binds them (the form's beside its drafts).
While the list is open it takes every key but F12, its reading keys
scrolling it and F1, `q` or Escape closing it, as a left click does, and
no page hears one or asks the service anything; a resize lays it out
again. A click closes the key list: a left press while it is open
closes it and repaints, as its title bar's td-ui hint says, stops a key
held through it from repeating, and reaches no page. With the list
closed a left press is the page's click at the pointer's last position:
on the destination step a listed disk's row selects that disk, hit
against the first row the last frame showed rather than one a pending
move would reveal; on settings a click focuses a field or works a
chooser, as that step says; and a press anywhere else on those pages or
on another page does nothing but press a step button below. A click on
a disk only selects, as Up and Down do; continuing is Return or the
Next button below. A click that changes the page stops a key held
through it from repeating. Motion only moves the position, a left
release acts only on a step button as below, the wheel scrolls only an
open chooser list on settings, and the other buttons do nothing, so
only F1 opens the key list (no page has a Help button, td-ui's
`keys::BUTTON`), it scrolls by its keys only, not the wheel, and F1,
`q`, Escape or that click close it. A window test sends it motion, the
wheel, a right press and releases, which leave it open and paint
nothing, then a left press, which closes it with the page unchanged;
another clicks each disk's row, the selected row again, an empty row,
the heading and the detail, and the same place off the destination
step.
The list's rows are spelled as td-ui's keymap spells chords
(`Return`, `PageUp`, `S-Tab`), and `keys::check` holds every page's
sections to that spelling; `--help` and this document name the keys
the same way. The review's Return confirms installation; Escape goes back.
A native
compositor case presses F1 on the seat and finds welcome around the
panel and its title bar in the selection's
colour, Return reaching no page while it is open, and Escape painting
welcome whole again.

Every step page shows step buttons at the right of its footer row,
laid out by td-setup's `nav` over td-ui's `Button`: Back where Escape
steps back and Next where Return steps on, each named for what it does
there: Reinstall and Fresh install on welcome, Next on the disk list and
the shown recovery key, Review on settings, Install on the review,
Done once the recovery key is kept, and Withdraw alone on the consent
view; the waiting, unavailable and refused
notices have Back alone. The two are fixed slots fifteen cells wide, Next
always rightmost and Back always left of it; on welcome these slots choose
Fresh install and Reinstall respectively. A lone Back or Withdraw is
kept in its slot, so a button never sits where the next page puts the other
kind. Next is painted as the default step, in the selection's colour,
and disabled while Return would do nothing: with no disk selected, while
a proposal or an execute is outstanding, or while the recovery key's
acknowledgement is with the service, when Back is not offered either.
A button acts on a left release over the one pressed, on
the page it was pressed on, so a press dragged off it, a key between
press and release, or the page changing under it does nothing, and
leaving the window forgets the press. A button is hit only as the last
presented frame showed it, unchanged since, so a frame too small for its
page offers none, and only once its page has been shown for half a
second, so a double click's second press does not act on the page the
first opened. On settings Review asks for the review from whichever
field has the focus, as Return on the time zone does. The completion
page's Restart and Power off act the same way while an ending may be
asked for. Progress, a recovery key still being asked for and an
outcome have no step buttons. A button is a view of its key: it asks
the service nothing the key would not, and consent is still given only
on the wizard's final review. `--preview`, and the native
compositor case that compares welcome and the unavailable notice pixel
for pixel, paint the same buttons, and `--render-check` refuses a page
whose footer text would run under the slots.

Its progress and completion views are pure views of the service's
reports, described below. A live boot starts the window in the graphical
session ("Live startup"); the end-to-end installation evidence is in place
with the medium booted as USB mass storage, and version 1's profile is
active (increment 7), with the parts named at the top remaining.

### Live startup

The system image carries td-setup, and a live boot (`td.live=1`, which only
the live selector's handoff sets; td-install/MEDIA.md "Live boot") opens it
and nothing else: the live session is for installing td. Every stock session
unit that opens a window (the shell's terminal, mail and news), places those
windows, or proves them in the autotest evidence names the live medium's
mount, `/run/td-media`, in td-svc's `unless-exists=` (td-svc/DESIGN.md §3),
so each settles ready without running and what requires it, such as boot
health after the terminal, still runs. Firefox names
`/run/td-applications-on-demand` instead, which a live boot also has
(APPLICATIONS.md §W.9). The session's
services, the secure attention path and td-authd's authority run as on an
installed boot. The `[setup]` td-svc unit starts the wizard once the
compositor serves (under the autotest token, also once TLS setup's console
output has ended); the compositor prepares its session with td-authd, which
binds the setup intake, before its display listener exists. It runs as
`tdi1000` through td-login's checked service path, in its own service
cgroup, unjailed and without disk authority, on the compositor's socket by
`WAYLAND_DISPLAY`. The compositor admits that identity as a display client
only, never as the human. The wizard maps alone on the first workspace,
where the session begins, with the whole output and the keyboard. A person
can still open a terminal or the launcher's programs; such a window maps
beside the wizard and takes the keyboard as any newly mapped window does.
The unit is ready when the compositor's layout report names a window with
td-setup's app id; it is never restarted, so a wizard that exits is not
relaunched behind the person, and an installed boot's unit exits at once.
Under the autotest token a live boot prints `TD-SETUP-LIVE-READY` once
td-setup's is the session's one window, the first workspace active and the
only one occupied, visible, focused and at least 752x480, and td-authd's
setup intake exists.

When the command line also holds `td.setup-input=1`, td-setup says on
standard error, which the unit leaves on the console, one
`TD-SETUP-SHOWN n=SEQUENCE page=...` line for each page state the
compositor showed while the window held the keyboard with its keymap
loaded: which page, and on the destination, settings and review pages the
disk count and selected kernel name, the focused field, the typed username,
hostname and zone search, the zone, whether a review left is still being
released, or the review's disk and settings. A state is said only after the
frame drawing it is done, never once a later frame or the too-small ground
has replaced it, and only when it differs from the last said; every value
byte outside a closed set is written `\xHH`. The lines are a test
diagnostic, not authenticated: any console writer could print one, and a
compositor overlay that keeps the window's keyboard focus while taking keys
is not visible to it. The evidence changes nothing the wizard does; a
command line td-setup cannot read leaves it off.

`td-recipe-eval qemu-boot-live` boots the medium `build-iso` composes, from
the same verified deployment and with a key made for the run, through UEFI
firmware as USB mass storage behind an xHCI controller, read-only, with an
empty sparse virtio disk after it. This oracle boots the medium only as USB;
optical media stay supported, and `qemu-install-system` and
`qemu-boot-media` boot media both ways. Firmware passes no command line on
the removable-media path, so the run's private copy of the firmware's
variables holds one boot entry, `Boot0000` first in `BootOrder`, naming only
`\EFI\BOOT\BOOTX64.EFI`, which firmware loads from the first filesystem
holding it, removable media first, with `audit=0`, the autotest tokens and
the setup-input token as its load options; the kernel appends them to its
built-in command line and the live selector hands them on. The entry is
written in edk2's variable store layout into a copy made for that boot. A
template OVMF would reformat (a firmware volume header of the wrong length,
revision, kind or checksum), a store holding a variable edk2 has not
finished writing, any boot entry, `BootOrder` or `BootNext`, or no erased
room is refused. Once `TD-SETUP-LIVE-READY` is seen it drives the wizard
through QEMU's emulated keyboard, acting only after td-setup has said the
state the act is for: welcome, the one listed destination, a username,
hostname and time zone typed a key at a time, the service's review of
exactly those, naming unencrypted storage for want of a TPM with the
keyboard console found, back from the review until td-setup says the
service released it, and the same review again. Return on the settled review confirms the installation directly.
QEMU's count of changes to the target, writes, discards and zone appends
alike, must be zero both after withdrawing and before the final confirmation.
Once td-setup says the
installation completed, with Restart selected, the drive presses Return;
the live phase ends only when the guest's orderly reboot has printed
`TD-SHUTDOWN-OK` and then the kernel's `reboot: Restarting system`, which a
power-off does not print, without a kernel panic, and QEMU, under
`-no-reboot`, has exited by itself and cleanly. The target must then hold
the installer's GPT layout, primary and backup whole. A lost or garbled
evidence line fails the run as soon as a later line shows the gap, and a
state that does not follow its act within five minutes, or an installed
notice that does not follow consent within half an hour, fails it then; so
does td-setup saying the installation failed, saying its ending was refused
or can no longer be asked for, or a guest still up ten minutes after the
restart was asked for. The power-off button's path is the same request with
the other verb, pinned by td-install's and td-setup's tests rather than
booted. Under that token the greeter parks rather than reboot the live boot
when its health transaction settles; no autotest window evidence runs on a
live boot, so nothing else maps to take the drive's keys. With the medium
detached, the installed disk then cold-boots through firmware twice, alone
and then renamed behind a decoy disk: each boot must bind the volume UUID
read from the image's own superblock and the medium's deployment, activate
the wizard's account and host (the primary profile and hostname reports),
acknowledge a healthy deployment and flip the compositor's pages, with a
machine-id, host key and the account's home under `/var/home` that the first
boot created and the second found, and with every compositor session's clock
naming Asia/Tokyo. Once a boot reports success, its display is captured
through QMP, at most once a second for two minutes and before the boot is
stopped, until the status bar's clock ends in the configured zone's offset,
UTC+09:00 for Asia/Tokyo, drawn as the compositor draws it from the image's
outline face; a boot whose display never shows it fails. The name and the
offset come from the one clock the compositor loaded from `/etc/timezone`.
Each boot's serial console is also a socket the oracle types into, QEMU
waiting for it before the guest starts and logging what the guest writes to
the console file it reads: once a boot reports success and the greeter has
logged the account in on ttyS0, its login shell must, within two minutes of
that success, report `id -un`, the home a bare `cd` from `/` reaches through
`HOME` with `pwd -P`, and `/etc/timezone` as `dana /var/home/dana
Asia/Tokyo`; on the first boot it writes a per-run token into a file in that
home and syncs, and each boot reports the file's contents and the home's
inode from `ls -di .`, the second boot the same token and inode as the
first. `./test-iso` boots a medium through firmware by hand, with no tokens.

The intake admits only the wizard's identity. The session's own programs,
its shell and the VM guest helper among them, run as UID 1000: the socket's
permissions (mode 0600, owned by `tdi1000`) refuse their connect, and
td-authd's peer check closes any other peer that reaches it unanswered, so
none can take the one installation service a session generation serves,
drive a review to the consent prompt or ask for the restart or power-off a
complete installation offers; that ending, which the wizard's identity may
ask for, only ends the live session through the supervisor's orderly reboot
or power-off. Nothing unprivileged becomes `tdi1000`: it is a service-only
account, and td-login, never installed setuid, switches credentials only for
root. Root is outside this boundary and could write a disk without the
installer (the stock image leaves its password field empty, but nothing in
the live session enters it); through the installation service, erasure still
needs the person's physical consent. Programs of the session can still
disturb the wizard without driving it, among them: the compositor's control
socket moves, regroups and resizes its window and changes its workspace, a
window mapped later, such as one a person opens, takes its focus (above), a
window with td-setup's app id satisfies the unit's diagnostic readiness
probe, the session's clients can fill the compositor's public client slots
or `/tmp` first, and files squatting td-ui's predictable buffer names in
`/tmp` can leave it unable to draw. Synthetic input is refused while trusted
attention is enabled, so none can type into it.

The destination page is a pure view over `Destination` values supplied by
the service. It shows capacity, model, kernel name, device number, sequence,
serial and WWID where present, and escapes untrusted labels for display,
including spaces. Long identifiers span numbered detail pages, so the full
escaped identity remains inspectable; excess disk counts show an explicit
refusal. The caller retains list position separately from choice. Supplied
label values carry a `value:` prefix, and wrapped continuation lines carry
`| ` and the field name, so they cannot impersonate the page's own field
headings or missing and present-empty markers. A selected row is only a
navigation index; the service must still authenticate the source, establish
eligibility and retain the disk claim. The page uses the pure `td-install`
library's plan module. Its target recipe stages that sibling source tree and
its confinement test pins the library's two public modules; outside test
modules only td-setup's `service` module names the protocol module, the
setup intake, a socket connection or threads, by any std path, grouped
import or alias, nor re-exports them, and the window connects only through
it and the toolkit. The live window lists the service's destinations and
proposes only what the settings step below completes. All wizard views fit
the compositor's 752-pixel tile within an 800-pixel headless output.

The account and regional settings page is a pure view over the window's
drafts. Username and hostname are text entries taking printable, non-space
ASCII up to their token limits, edited with Left, Right, Home, End,
Backspace and Delete; keyboard layout is fixed at `us`; time zone is a
chooser over the catalog the service's time zones reply carries, asked for
over the connection that listed the disks the first time the step is shown.
Up, Down, PageUp, PageDown, Home and End move through the catalog, and
typed characters seek the first zone beginning with them, ignoring case; a
character that would match none is not taken, and a movement key starts the
seek over. `Etc/UTC` is chosen when the catalog has it and nothing else was;
a later catalog keeps a choice it still has. Until the catalog arrives the
row says it is being read, and a refusal is shown there in the installer's
words and asked again when the step is next entered. A catalog that never
comes holds no page: Escape leaves settings, and a disk list then asked
waits behind it and is abandoned like any other. Tab and S-Tab move
between fields; on the others Up, Down and Return do. The drafts, and a
catalog already received, outlive a lost connection. The page displays
bounded tokens and focus without treating wire admission as policy or
catalog approval, and empty values may be shown before completion; the
validation contract below applies before review.

Keyboard layout and time zone are drop-down choosers: each is a bordered
box with a `▾` at its right, and opens a list of its entries, eight
rows tall with a scrollbar when there are more, under the box, or over it
when the footer leaves no room below. Space or `M-Down` on a chooser, or a
left click on it, opens the list with the current choice selected and in
view. While it is open it takes every key: on the time zone Up, Down,
PageUp, PageDown, Home, End and typed characters choose as above and the
rows follow the choice; Return, or a click on the open chooser, closes it
keeping the choice; a click on a row chooses that entry and closes it;
a click on the list's scrollbar or border does nothing; Escape, or a
click anywhere else, closes it and puts back the zone it opened on,
a click on another field also focusing it, and on the other chooser
opening its list; Tab and S-Tab keep the choice and move on. Rows are hit
as the list now stands, which the next frame shows. The wheel scrolls
the open list without choosing, and does nothing while it is closed. A
click on a text field focuses it, and with no list open a click off the
fields changes nothing, so a seek in progress and a step button's first
press survive it. The layout's list holds its one layout, `us`; a time
zone list opens only once the catalog has arrived. While the time zone
list is open the key list leads with its keys. So a Return that closes
the list never proposes, and a press on a step button while a list is
open only closes it, unchanged; an answer that leaves settings closes
it keeping its choice. Return on the time zone row proposes the
selected disk with the drafts, or says on the page's notice row
which field is still empty; while the service reviews, the notice says so
and a second Return proposes nothing. The service's review opens the review
page, and its refusal is shown on the notice row in the installer's words
until a key changes the form. The fields stay editable while the service
reviews; the review shows what was proposed. A review answers only the
proposal last sent while it is still wanted: one given up is withdrawn when
it arrives, even if settings were shown again and another proposal made,
which is sent after that withdraw. Leaving settings before a review arrives
drops a proposal not yet sent and releases one that arrives later.

The pure review page renders one immutable `Plan` proposal. It shows the
complete escaped disk identity, all four selected settings and the
plan's storage and its basis ("Storage choice") across bounded detail
pages. The destructive-loss notice, the storage notice and the
automatic-login notice remain visible on every page: an unencrypted
review's says storage is not encrypted and why, and a device-bound
review's that storage is encrypted to this computer's TPM, which does
not protect a lost computer, that the account signs in automatically,
and that a recovery key follows. A device-bound review's detail pages
carry the tier's full disclosures ("Storage choice"). It cannot
authenticate the
source, establish a disk claim or authorize execution; the service and
the wizard's final confirmation remain mandatory. In the live window it
shows the service's review, never the drafts: PageUp and PageDown move
between its detail pages, Return sends execute with the review exactly as
the service returned it, once, and Escape returns to settings and sends
withdraw with the review's nonce, releasing the review and its disk
claim before anything else is asked; an
execute already sent is answered first, and one that already ended the
review cancels the release. An execute refused as consent unavailable is
shown under the warnings and leaves the review held, so it may be executed
again or left; one refused as no review returns to settings with nothing to
release; one refused as a stale review returns to settings and withdraws the
review shown, since the service still holds a review and its claim, and a
withdraw it refuses ends the connection, which ends that review; one refused
as busy ends the connection, since consent may be sought or an installation
running. An execute that finds the disk changed returns to settings with
that reason. A lost connection discards the review, which with its claim
ends with the installer's channel until an installation starts.

The pure progress view renders service-supplied phases and a bounded failure
reason. It shows an unknown outcome when service status is unavailable,
never infers success from a queued request, and warns that a failed disk may
be incomplete and requires a new review before retry. The completion view
offers two buttons, Restart (selected first) and Power off: Left or Up
chooses Restart, Right or Down Power off, Tab and S-Tab the other, and
Return asks the service, over the same connection, to restart or power
off. The view says to keep the installation media in until the computer has
restarted and shows its startup screen, or has powered off, then remove it.
The live system runs from the medium until its teardown ends, so a medium
pulled when the screen goes dark can leave that teardown unable to run its
last programs and the machine to be switched off by hand (the installation
is already durable); and an installed disk boots by the removable-media path
with no firmware boot entry, so a medium left in may start again. Powering
off lets the person take the medium out with the machine at rest. Return
asks once, showing that it asks; a refusal is shown and either button may be
asked for again; an accepted ending shows the computer restarting or
powering off and takes no more keys. Once the connection is gone, nothing
can ask again (only welcome connects, and td-authd serves no second
installation), so the view says to restart or power off the computer by
other means; gone while an ending is asked for, since the teardown may end
it before the acceptance reaches the window, the view says the computer may
be restarting or powering off and, if it does not, to do so by other means.
The caller may show the view only after the service reports durable
filesystem and deployment publication, verified boot artifacts, and settings
publication. In the live window, execute answered awaiting consent shows a starting
view while root acknowledges execution; no secure attention prompt opens.
A device-bound installation shows its recovery key in the same wizard.
While starting or installing, the window asks for status over the same
connection at most every half second
from each ask, and follows only the review it executed: awaiting consent, a
running phase, failure and completion are shown as reported; a review
abandoned before any write (declined, expired, unavailable or the disk
changed) returns to settings with the reason; idle, another review's state,
the review still merely reviewed, or a lost connection after execute shows
the outcome as unknown, never success or a mere unavailable service. Escape
at the consent view withdraws the review; until the release is confirmed a
lost connection, or leaving the wait for disks on a new attempt, is still an
unknown outcome, and a state already asked for that reports the review ended
settles it: abandoned needs no release, and completion or failure is shown.
No key leaves progress, failure, an unknown outcome or completion; Return on
completion asks only for the chosen ending. A reported completion or failure
stands when the connection ends.

A device-bound installation reaches completion through its recovery-key
phase ("Device-bound records"). When status first reports the phase for
the review it executed, the window asks for the key, once per execution,
and shows its eight groups in td-protector's display form beside a QR
code of its 48 digits (td-setup's `qr`: ISO/IEC 18004 version 2, level Q,
numeric mode, the one symbol that holds exactly 48 digits), saying that it
is shown only now, that no copy is kept, and that it is the only way to
open the disk if the TPM, firmware measurements or boot chain change. A
phone that scans the code reads the 48 digits alone, what cryptsetup's
passphrase prompt takes; the page advises keeping a scan in a password
manager, not a camera app, which may send what it scans to its maker
(ENCRYPTION.md "Device-bound default"). The code is drawn dark on light
in literal colours no theme maps, with its four-module quiet zone, at the
largest whole module of at least four pixels per scale unit that fits
beside the text, and the page is not drawn where that does not fit; the
rows beside it are cut to 56 columns. Nobody types any
part of the key here or in any other routine flow (ENCRYPTION.md
"Device-bound default"). The phase has no deadline and changes only with
what the window sends, so its state is not asked for while the key is
shown. Return goes on to a step of its own that says the key is not
shown again and nothing on the computer keeps it; Escape there shows the
key again, and Return says it was kept and sends the acknowledgement,
which carries the review's nonce and nothing of the key. Return acts
once a press, never repeating while held, so one press never passes the
key unseen, and Return there acts only once that step has been painted
for the same 500 ms a review must be, so a second press with the first,
a double tap or a bouncing key cannot acknowledge a key whose step the
person never saw. Once the acknowledgement is sent, the key is no longer
offered from that step. The key's pages say that closing the window now
withdraws the installation and the disk will not start. The window holds
the key, in memory only and zeroed on drop, until the service takes the
acknowledgement, and may show it again until then; no copy or paste
reaches it, and no diagnostic names a digit. Its display form and its QR
code's modules, and every buffer the encoder fills on the way, are
zeroed on drop; the frame's pixels are outside that and not zeroed. Once
the acknowledgement is taken the window shows the installation
finishing, polling as for progress, while the service writes the table
and the phase still reports; the completion page and its restart follow
only the service's complete. The evidence line says the step
(`page=recovery step=asking`, `shown`, `kept`, then `page=finishing`),
never a digit. Only the service's own report of failure 6 is shown as
withdrawn. A connection lost during the phase before the acknowledgement
is sent, the key's own request answered as refusal 16 or otherwise than
with this review's key included, shows the installation not finished
(`page=unconfirmed`): a service still running withdraws it, but one that
stopped, a crash included, withdrew nothing, and the window cannot tell
which. Either way the disk carries no partition table, so it does not
start, and it may still hold the new ESP files and the volume's header,
which the unconfirmed recovery key opens (ENCRYPTION.md "Device-bound
formatting"). A connection lost once the acknowledgement was sent, or
after it was taken, shows the outcome as unknown, as any lost connection
does, and so does an acknowledgement answered otherwise than with this
review's phase, which ends the connection. A connection lost on a
device-bound execution before the phase is reported is unknown too: an
earlier write or verification failure may still be its outcome.

Disk enumeration is read-only and bounded. Show model, serial when supplied
by the device, capacity and a distinguishing device identifier. These are
descriptions, not proof of device authenticity. Exclude the installation
medium and every disk backing its mounted files, mounted/in-use targets,
read-only devices, partitions and unsupported device topologies. Failure
to resolve backing devices refuses installation rather than guessing.

Review produces one immutable installation plan. Before any destructive
write, the service validates its opened destination against that plan,
rechecks eligibility and size, and retains the destination identity for the
entire operation. Hot removal, replacement, a changed plan or lost consent
requires a new review. A stale device pathname never identifies permission
to erase whatever later appears there. Descriptor pinning alone does not
prove that an unrelated process cannot mount the device; the implementation
must specify exclusive admission before activating disk writes.

No write occurs while navigating the wizard. Once destructive execution
starts, cancellation or UI loss cannot promise restoration of old contents.
The service retains bounded progress and an explicit outcome and never
automatically retries a destructive operation after reconnect or restart.
Completion requires durable filesystem and deployment publication, verified
boot artifacts, and settings publication. A queued request is not success.

The raw `td-install format` command coordinates preparation and both
filesystem writes through one held destination. Its exact interface and
limits are in DESIGN.md's coordinated raw formatting contract. The small
QEMU oracle uses it after read-only source validation; the full-system
oracle installs through the service. Scratch preparation must succeed before
the first GPT write. The optical/USB scratch-exhaustion oracle constrains
the live formatter's private staging tmpfs to 64 KiB, verifies that mount
size, requires a failed mkfs image-zeroing operation and an independent
scratch ENOSPC probe, and checks every destination byte remains unchanged.
This is a refusal test, not a scratch-size estimator. This command does not
activate the service or provide the review/consent sequence.

## Immutable review data

The `td-install` Rust library exports `installation_plan::{Plan,
Destination, DestinationObservation, Settings, Storage, Basis, Candidates,
Zones}`
and the `installation_protocol` messages described below. Both are pure data
prerequisites for the service and UI, with no CLI, device access, filesystem
access, entropy generation, transport or installation execution. The Cargo
library is also a target path dependency of td-setup; it exports exactly
those two modules, as pinned by td-setup's confinement test, which lets only
td-setup's `service` module name `installation_protocol`. The formatter
binary stages the plan and protocol modules separately through `#[path]`,
for `observe-plan` and `serve`, with its recipe and compiled-file guard
declaring the sources; its recipe also compiles td-tpm, td-json and
td-protector as rlibs for `serve`'s TPM probe and device-bound
formatting. Each further target
consumer must declare its source and public API reach in its own recipe
and confinement roster. A decoded plan conveys no authority.

`Candidates` carries at most 64 distinct destination observations, as the
protocol's destinations reply. Its canonical `TDCAND01` record has an
eight-byte magic, one-byte count, then each destination in the same field
order and bounds as the plan's destination. It admits at most 55,817 bytes
before allocation and rejects trailing bytes, duplicate kernel names or
device numbers, repeated disk sequence numbers, invalid fields and
truncation. An empty list is valid. Encoding and decoding preserve the
observations only: neither operation proves that a disk is eligible, that
the installation medium is excluded, or that a privileged service supplied
the bytes. `Zones` carries the service catalog's time zone identifiers, 1 to
1,024 plan timezone tokens in strictly ascending order. Its canonical
`TDZONE01` record has an eight-byte magic, a big-endian u16 count, then each
identifier as a u16 length and its bytes; it admits at most 67,594 bytes
before allocation and refuses trailing bytes, disorder, repeats, invalid
tokens and truncation. Like `Candidates` it proves nothing: the service
checks a chosen zone against its catalog, not these bytes. The live window
lists a service's destinations as navigation only.

A plan owns a nonzero 32-byte proposal nonce, the complete destination
observations, a 32-byte deployment manifest digest, a version-4 volume
UUID, and username, hostname, keyboard and timezone choices. Its
destination contains the kernel name, major/minor, disk sequence,
capacity, logical sector size, removable flag and optional model, serial
and WWID. All retained fields are private and accessible only through
shared references or copied scalar values. Equality compares every
field, including optional labels and nonce. A clone is the same
proposal, never a fresh consent or retry. The record describes whole-
disk erasure and automatic login, which are not caller-selectable flags,
a basis byte recording what the service's own probes found and a
storage byte naming the storage they allow, or unencrypted where the
person chose it ("Storage choice", "Device-bound records"); a request
carries neither, and a proposal only the person's choice.

The public `DestinationObservation` names each unvalidated input field;
`Destination::new` validates and copies it. Construction and decoding admit
the wire representation only. Destination names have 1..=64 ASCII letters,
digits, underscores or hyphens; major and disk sequence are nonzero.
Capacity is nonzero and aligned to the admitted 512- or 4096-byte logical
sector. Labels retain up to 256 UTF-8 bytes exactly, matching discovery's
representation. Missing, present-empty, and nonempty labels are distinct;
no trimming or normalization occurs here. They are untrusted display data:
a renderer must escape control characters and handle directionality without
letting a label impersonate trusted prompt text. Choices are nonempty
printable ASCII without spaces, bounded to 32, 63, 64 and 64 bytes
respectively. These bounds do not establish account name grammar, hostname
policy, keyboard support or timezone membership. Account, hostname and
timezone checks remain mandatory before review; keyboard catalog admission
must be implemented before that choice is used. Wire admission deliberately
does not claim a disk fits a deployment or is an eligible whole disk.
Device labels are observations, not authentication.

`Plan::encode` produces one canonical binary record. `Plan::decode`
rejects inputs over 2048 bytes before parsing, unknown versions,
truncation, invalid lengths, invalid flags, invalid field
representations and trailing bytes. The complete framing is checked
before allocating field strings. No native-endian numbers, JSON numeric
rounding or path resolution enters the codec. The field order is:

- Eight bytes `TDPLAN04`, nonce (32), deployment digest (32), UUID (16),
  mode (one byte: 0 erase, 1 preserve).
- Big-endian major/minor (u32 each), sequence/capacity (u64 each), sector
  size (u32), removable (one byte, exactly zero or one), storage (one
  byte, 0 unencrypted or 1 device-bound) and basis (one byte, bit 0 the
  TPM probe and bit 1 the keyboard-console probe, any other bit
  refused).
- Kernel name, then model, serial and WWID. Each optional label starts with
  a zero/one presence byte; only a present label has a string payload.
- Username, hostname, keyboard and timezone, in that order.

Every string has a big-endian u16 byte count followed by exactly its
bytes (UTF-8 for labels, ASCII for names and choices). The current
maximal admitted record is 1194 bytes. The storage and basis bytes sit
inside the destination's fixed fields, so a candidates record and a
propose request, which carry the destination alone, have neither. UUID bytes are in
textual/network order, with version 4 and the RFC variant bits checked;
any 32-byte deployment digest is representable, including all zeroes. A
digest is a full-width hash value, not a reserved nonce sentinel; only
source validation can establish that it names the intended manifest.
The codec does not prove a nonce is fresh or a UUID globally unique. The
authority generates them.

Before a service presents this value to an operator for consent, it must
validate and retain its source, validate all choices against that
source, establish destination eligibility and layout and payload fit, and
retain the exact proposed value. Scratch shortage is refused before the
first write instead (DESIGN.md "Payload fit"). The service core below does
each of these: it retains the source and excludes the disk it is stored on,
validates it under its claim, requires it to be the deployment the
running root was verified as, so that the choices it checks against
that root and its time zone catalog are checked against the source, refuses
a disk that cannot hold its payloads and retains the value. td-authd starts
it on a live boot and answers its consent channel. Execution requires the wizard's final
confirmation bound to that whole value and revalidates the selected disk
under a retained exclusive claim. Only the authenticated live wizard connection can send that Execute. No public request, reconnect or service restart
may silently retry erasure. A live boot starts the wizard ("Live startup"),
and td-authd admits only the wizard's identity as its installer; erasure
still needs confirmation on the wizard review.

## Installation service protocol

`td_install::installation_protocol` defines the messages between the
unprivileged installer and the root installation service. It is data and
codec only, and decoding a message grants no authority. The service core
below implements it; td-authd's setup intake starts it for td-setup, which
asks for destinations, time zones and status, proposes, executes, withdraws,
asks for a device-bound installation's recovery key and acknowledges it,
and asks for an ending, and checks that a review carries exactly what it
proposed and that a withdraw answers abandoned (withdrawn), the key
request the key, an acknowledgement running in the recovery-key phase,
and an ending complete, for its nonce; any other answer ends its
connection, and with it, until an installation starts, the review and
claim.

Both ends first send and require the eight bytes `TDINS09\n`. Any change to
a message or its bytes changes this greeting; there is no negotiation. Each
message then travels in one frame: a big-endian u32 length and that many
payload bytes. `payload_len` admits a nonzero length within the direction's
bound before the reader allocates. A request is at most 2049 bytes, so root
admits only a small bound from the unprivileged side; a reply is at most
67,595 bytes, the widest time zone catalog. A payload is one message: a tag
byte and its body, with trailing bytes refused. A decode failure closes the
channel without a reply.

Requests carry no path, executable, mount option, source or consent:

- `0x01` destinations: no body. The service observes eligible whole disks.
- `0x02` propose: a destination, then username, hostname, keyboard and
  timezone, in the plan's field order and bounds, then one byte: 1 when
  the person chose unencrypted storage, 0 otherwise, any other value
  refused ("Storage choice"). The destination only
  selects: the service admits it only if it equals, field for field and
  labels included, the service's own fresh observation of an eligible
  candidate, and otherwise refuses with destination changed. The reviewed
  plan carries the service's observation, never UI-supplied values. The
  service chooses the nonce and volume UUID, validates the source,
  claims the disk and checks that the disk holds the deployment.
- `0x03` execute: the complete `TDPLAN04` record the installer reviewed. It
  asks the service to seek consent for the review it retains, and is
  refused unless the record equals that review. Equality is a
  precondition, never consent: consent reaches root only from the
  compositor, under the elevation contract, never through this channel.
- `0x04` status: no body.
- `0x05` withdraw: a 32-byte review nonce. It releases a review, and its
  claim, before any destructive write.
- `0x06` time zones: no body. The service reads the running system's
  catalog once and keeps it, failure included; settings are checked
  against that same record, so the installer offers exactly the zones the
  service admits, and an unreadable catalog refuses a proposal as time
  zones unavailable, not as an unsupported zone.
- `0x07` restart and `0x08` power off: a 32-byte review nonce. Each asks
  the service to end the live session through the supervisor's orderly
  reboot or power-off, only while the installation the nonce names is
  complete.

Replies set the high bit, so no request decodes as a reply or the reverse:

- `0x81` destinations: a `TDCAND01` record.
- `0x82` reviewed: the service's `TDPLAN04` record, made while it holds
  the destination claim. Its nonce names the review.
- `0x83` status: a state byte, then for every state but idle the nonzero
  review nonce, then for running, failed and abandoned one detail code.
  States are 0 idle, 1 reviewed, 2 awaiting consent, 3 running, 4 complete,
  5 failed and 6 abandoned.
- `0x84` refused: one code. Nothing was written.
- `0x85` time zones: a `TDZONE01` record.

Running phases are 1 preparing the disk, 2 writing filesystems, 3 publishing
the deployment, 4 applying settings, 5 verifying boot and 6 recovery key.
Failures, after which the disk may be incomplete and a retry needs a new
review, are 1 destination changed, 2 insufficient space, 3 write failed, 4
verification failed, 5 settings failed and 6 recovery unconfirmed.
Abandonment, which means no destructive write started, is 1 withdrawn, 2
consent declined, 3 consent expired, 4 consent unavailable (the trusted
path could not show the review, or lost it while displayed) or 5
destination changed (the held disk changed or vanished, or a recheck
refused it, before the first write). Refusals are 1 busy, 2 source
unavailable, 3 discovery failed, 4 destination changed, 5 destination busy,
6 insufficient space, 7 invalid username, 8 invalid hostname, 9 unsupported
keyboard, 10 unsupported timezone, 11 stale review, 12 no review, 13 consent
unavailable (no seat, compositor or installer confirmation path can present the
review, or the consent channel cannot carry it), 14 time zones
unavailable (the catalog could not be read), 15 power unavailable (the
supervisor did not accept the reboot or power-off) and 16 recovery key
sent. 17 was a typed-back key that did not match; no key is typed back
since `TDINS08`, and 17 is unassigned. The recovery-key phase, its two
requests (`0x09` and `0x0a`), its reply (`0x86`) and code 16 are
"Device-bound records". Unassigned codes refuse, and a duplicated code
does not compile.

The service holds at most one review. A review is held while reviewed,
awaiting consent or running, and not after complete, failed or abandoned.
Where two refusals apply, busy wins. Each state admits:

- destinations, time zones and status: in every state; all are
  read-only.
- propose: while idle, failed or abandoned. While reviewed, awaiting
  consent or running it is refused as busy; the installer withdraws before
  proposing again, so no request replaces a review that is held or
  displayed. After complete it is refused as busy: one service installs
  once, and td-authd starts no further service that generation.
- execute: only while reviewed, and only with a record equal to that
  review. The service rechecks the held disk, then answers awaiting
  consent, abandoned (destination changed) or refused as consent
  unavailable. A different record is a stale review; with no held review
  it is no review; while awaiting consent or running it is busy, so
  execution is never repeated.
- withdraw: while reviewed or awaiting consent, for that review's nonce,
  answering abandoned (withdrawn) and releasing the claim and any displayed
  consent. Another nonce is a stale review, no held review is no review,
  and running is busy: a started installation cannot be withdrawn.
- restart and power off: only while complete, for that installation's
  nonce. The service runs the running root's `/bin/td-svc reboot` or
  `/bin/td-svc poweroff` with a cleared environment, no input, its output
  captured and its error discarded, for at most ten seconds, and answers
  complete once the supervisor has accepted that verb (`reboot requested`
  or `poweroff requested`, or that one already in progress). The
  supervisor accepts before it stops anything, but its teardown may end
  this service, or the installer's connection, before that answer reaches
  the installer. Any other answer, the other verb's included, failure or
  timeout kills that client and refuses as power unavailable; a client
  that timed out may still have started the ending. Another nonce is a
  stale review, running is busy, and every other state is no review. Neither
  takes consent: they change no disk, end a live session whose writes are
  lost at power-off anyway, and run the same teardown every reboot or
  power-off does (td-svc/DESIGN.md section 8).

Destinations and propose may be refused; execute, withdraw and the ending
requests answer with status or a refusal; status always answers with
status.
`Reply::answers` checks only this shape: the installer still compares the
review nonce and the proposed or echoed fields. Complete means durable
filesystem and deployment publication, verified boot artifacts and settings
publication. An installer that asked for execution and then sees idle, or
loses the channel, shows the outcome as unknown, never success. Nothing
retries a destructive operation after a reconnect or restart.

### Device-bound records

These records carry device-bound storage. They landed at once, in
ENCRYPTION.md increment 5, as the greeting `TDINS05\n`, the consent
channel's `TDINA03\n` and the plan's magic `TDPLAN02`; the earlier
versions are refused, not negotiated. `TDINS05` is `TDINS04` (power off,
request `0x08`, included) with the additions below, so the recovery
requests take the next free tags. Increment 7's probe commit then made
them `TDINS07` and `TDPLAN04` with the plan's basis byte ("Storage
choice"); `TDINA03` is unchanged. `TDINS08` replaced the type-back with
an acknowledgement that carries no digit, and retired refusal 17.
`TDINS09` added propose's storage choice byte; the plan's bytes are
unchanged under `TDPLAN04`, and it now admits unencrypted storage with
any basis.

- The plan gains one storage byte after the removable flag: 0 unencrypted
  or 1 device-bound, any other value refused. It is the storage the
  plan's basis names, or unencrypted where its proposal chose that
  ("Storage choice"); a plan naming device-bound storage its basis does
  not is refused. The review page and the consent channel's review
  report, which gains the same byte after the deployment digest, show
  it.
- Running phase 6, recovery key, follows verifying boot on a device-bound
  installation: everything but the partition table is written and
  verified, and the service holds the recovery key.
- Request `0x09` recovery key, a review nonce, is admitted only in that
  phase for that nonce. Its first admission is answered by reply `0x86`
  recovery key: the nonce and the key's 48 ASCII digits. Every later ask
  is refused as 16 recovery key sent, so the service sends it once.
- Request `0x0a` acknowledge recovery, a review nonce alone, is admitted
  only in that phase after the key was sent: the person has the key,
  written down or scanned from its QR code. The service zeroes the key and
  answers status, still running in the phase, while the execution writes
  the table. Nothing of the key travels back, so the service compares
  nothing; one carrying more than the nonce is refused as a decode
  failure.
- Failure 6, recovery unconfirmed: the installer was lost before the key
  was acknowledged, during that phase or before it, and the installation
  was withdrawn (DESIGN.md "Device-bound formatting").

The phase has no deadline: a person may take as long as writing the key
down needs. Withdraw stays refused as busy while it runs.

The key travels once, from the service to the installer, as exactly 48
ASCII digits: the codec admits nothing else and leaves the check digits
to td-protector's recovery-key codec, which td-setup applies before it
shows the key. Its value is zeroed on drop and never printed, and
equality reads all 48 bytes without an early exit. The encoded message
holding it is its sender's to zero; each receiver (td-setup and the QEMU
oracle) zeroes every received payload with the codec's `scrub` after
decoding it, successfully or not, and the service zeroes every request
it reads the same way.

Only a device-bound execution reaches the phase, once its volume is
written and verified (DESIGN.md "Device-bound formatting"): it hands the
key to the service, which answers status as running in phase 6. Outside
the phase both recovery requests are refused as busy while an
installation runs and as no review otherwise; in it, a request naming
another nonce is a stale review, and an acknowledgement before the key
was sent is busy. The service frames every reply it sends, the recovery
key included, from buffers it zeroes once written or not, and a frame
read short is zeroed too. The acknowledgement releases the waiting
execution, which only then writes the partition table, the write that
makes the disk bootable, syncs it and reads it back. Its result completes
the installation or fails it, as write failed or verification failed
with the installation withdrawn, and only then reaches status and
td-authd's finished report; until then status stays running in phase 6,
and a request the phase admitted is refused as busy. An installer lost
before the acknowledgement, during the phase or before it, is answered on the
execution's behalf: it withdraws the installation, which fails as
recovery unconfirmed. One lost after it changes nothing: the execution
finishes as it would have.

A service's plans are device-bound or unencrypted as its probes found
("Storage choice"). td-setup shows a review of either storage as the
service made it, and follows the phase and failure 6 only for a review
it executed as device-bound: the worker remembers the storage of the
review execute last named, and the phase, failure 6, the key or an
acknowledgement reported of any other review ends the connection.
td-authd shows either storage on its consent summary, so the live wizard
reaches the completion page's display and acknowledgement on a machine
with both probes, as `qemu-boot-encrypted`'s wizard leg proves.

### Storage choice

This section is ENCRYPTION.md increment 7's, and current: `serve`
chooses storage from its own probes, and the person may choose
unencrypted storage where they allow device-bound.

The plan carries one byte after the storage byte, the storage basis: bit
0 set when the service's TPM probe passed and bit 1 when its
keyboard-console probe passed (ENCRYPTION.md "Activation" defines both),
any other bit refused. The plan's magic is `TDPLAN04`, its maximal
record 1194 bytes, and the greeting then `TDINS07` (now `TDINS09`), the
earlier versions refused rather than negotiated; a propose request
carries no basis and no storage, only the person's choice of unencrypted
storage (`TDINS09`). The consent channel's review report keeps its
storage byte alone and its `TDINA03`. `serve` runs both probes at every
start, once each, after admission and before its greeting, writes the
cause of each that fails to standard error, and records them in every
plan. The TPM's read has a 3-second deadline (`TPM_PROBE_DEADLINE`) so
that a slow TPM, whose command the kernel may retry for minutes, cannot
outlast the installer's ten-second greeting wait: a missing device, a
failed read or a read still unanswered at the deadline is recorded as
not passed, with its cause on standard error, and never refused. A late
read is abandoned on its own thread, never waited for, and that service
makes no further TPM use, since only a device-bound execution would and
its plans are unencrypted. The kernel's wait for a TPM command takes no
signal, so with a wedged TPM that service's process cannot exit until
the read returns, up to the kernel's two-minute command duration;
td-authd does not wait on it, but admits no new wizard connection while
the old service is still stopping, so a reconnect can wait that long.

Every plan names device-bound storage exactly when its basis has both
bits and its proposal did not choose unencrypted storage, and
unencrypted storage otherwise (`Basis::storage`, `Plan::unencrypted`);
nothing refuses for a missing TPM or keyboard console. The plan's
storage byte is that storage: a plan naming device-bound storage with a
basis that does not is refused, and one naming unencrypted storage is
admitted with any basis, which it keeps, so the review and the consent
channel show what the probes found beside what was chosen. The encoding
and `TDPLAN04` are unchanged; `TDINS09` admits the newly possible
record. td-authd starts the service with no storage operand, for there
is none, and its consent summary shows the plan's storage ("ENCRYPTED TO
THIS COMPUTER, AUTOMATIC LOGIN" or "UNENCRYPTED STORAGE, AUTOMATIC
LOGIN"; td-authd/DESIGN.md "Immutable consent description
prerequisite"). td-setup follows a device-bound review it executed
through the recovery-key phase.

The review page shows the storage on a detail row (`Storage: encrypted
to this computer`, `Storage: not encrypted, as chosen` or `Storage: not
encrypted`) above the basis, worded as findings (`Probes: usable TPM 2.0
found` or `not found`, `keyboard console found` or `not found`), and its
evidence line names both (`storage=device-bound|unencrypted
tpm=found|missing console=found|missing`) and the disclosure set
(`disclosure=device-bound|chosen|no-tpm|no-console|no-tpm-no-console`).

The person chooses on the review, where the storage is disclosed, not on
a form of its own. A fresh review naming device-bound storage says on
its notice row, while nothing else is said there, that `u` installs
without encryption instead; `u` withdraws that review as Escape does and
proposes the reviewed disk and settings again with the choice. The
review of that proposal states that storage is not encrypted, as chosen,
so that whoever has the disk can read it, and offers `e` to propose
again encrypted to this computer. The choice is kept for every later
proposal until `e` takes it back. Neither key does anything once Return
has asked to install, on a reinstall, or on a review the probes left
unencrypted, which offers nothing: no key asks for encryption the
service cannot provide. td-setup checks that a review of a proposal that
chose unencrypted storage names unencrypted storage, and that one that
did not, or a reinstall, names its basis's storage, and otherwise ends
the connection. Either key acts only once the review has been painted
for the 500 ms Return waits, and once a press, so a letter typed or held
across the page change never changes what is reviewed; Shift or Caps
Lock sends the capital the page names, which acts the same. The new
proposal is the reviewed plan's disk and settings, not drafts edited
since. Starting again from the welcome page forgets the choice.
td-authd's consent summary shows the storage as it does for any review
(`UNENCRYPTED STORAGE, AUTOMATIC LOGIN`).

The welcome page says that storage will be encrypted to this computer
when it has a usable TPM 2.0 and a screen and keyboard its startup
recovery prompt can use, and unencrypted otherwise, that the review says
which and lets the person choose none where encryption is possible, and
that the account signs in automatically either way. An unencrypted
review states that storage is unencrypted and why:
no usable TPM 2.0, no keyboard console (a framebuffer console and a
keyboard with Enter and the top-row digits), or both; and that the
account signs in automatically. A device-bound review states, beside
the destructive-loss notice and across its detail pages, every one of
ENCRYPTION.md's disclosures for the tier, in wording that may be shorter
but drops none:

- Storage is encrypted to this computer and released at power-on with
  no interaction. That is device binding, not authentication of a
  person: anyone who powers on this computer with its TPM unlocks the
  disk and reaches the desktop, which the account enters automatically.
  It protects the disk read away from this computer (a removed drive or
  a copied image), not a lost or stolen computer ("Device-bound
  default").
- What the disk held before installation is not erased: blocks the new
  system has not overwritten keep it, readable to whoever has the disk
  ("Device-bound formatting").
- A recovery key follows, as digits and a code a phone can scan. It is
  shown once and no copy is kept; the installation completes once you say
  it is kept, and until then the disk does not start. Nothing asks you to
  type it now; it is typed only to recover the disk. It is the only way
  to open the disk if this computer's TPM, firmware measurements or
  startup files change.
- Until the installed system first starts, release is not bound to this
  computer's startup chain: any other system that leaves PCR 12 at zero,
  td's live medium excepted, can release it, and so can the
  administrator of an unencrypted td system running on this computer. A
  one-time firmware boot-menu choice used for that first start can send
  later ordinary starts to recovery.
- When release fails, the recovery prompt appears on the screen and on
  the serial console. The key's digits are never shown, but a console
  server or BMC recorder on the serial line may record what is typed.
- If this computer's TPM is missing at a start, or reaches the system
  only after td's startup wait for it, that start goes to recovery, and
  on a TPM that appears late the running system's administrator can
  release the disk until the next restart.
- The screen and keyboard found now are what recovery will expect. One
  removed or changed later (a USB keyboard unplugged, a display moved to
  a graphics card without UEFI GOP or a td driver) can leave recovery on
  the serial console alone, where the computer has one; the live medium
  and the recovery key still open the disk. A device that only
  advertises a keyboard's keys (a security token, a receiver with no
  keyboard paired, a barcode scanner) counts as a keyboard here.
- The account signs in automatically.

## Installation service core

`td-install serve <td-boot> <deployment-directory> <verified-root>
<td-firstboot>` is the service core for one
installer. Its four operands are absolute control-plane inputs bound by its
caller, never by the installer, and must be present and of their kind
(an execution also reads the live session's saved Wi-Fi networks under
the verified root, DESIGN.md "Carried Wi-Fi networks"):
`td-boot` and `td-firstboot` executable files, the deployment directory and
verified root directories. It requires effective uid 0 (a
sanity gate against a misplaced start, not proof of privilege), the
installer's channel on standard input and td-authd's consent channel on
standard output: each a connected Unix stream socket, found as such in
`/proc/net/unix`, since a datagram or sequenced-packet peer would truncate
frames and need not close, and not one socket on both. That table lists only
serve's own network namespace, so its caller must create the sockets there,
and every row naming a socket must agree, since a bound name can forge one.
It clears an inherited non-blocking flag on each, and refuses before sending
a byte otherwise. Nothing it runs writes to standard output: every child's
is captured or null. It then speaks the installation service protocol on
standard input until the peer closes between frames, and exits; a malformed
frame or message ends it without a reply. Until an installation starts, the
review and its claim end with the installer's channel; a started
installation runs to its finished report first. td-authd starts it for each
installer on a live boot (td-authd/DESIGN.md "Whole-disk installation
intake").

serve takes no storage operand. Every serve opens the TPM once the
admission checks above pass, so a misplaced start never reaches it, and
before sending a byte, and probes whether it is usable: PCR_Read of PCRs
4 and 9 in the SHA-256 bank answers that bank, and neither value is
zero. That read is td-protector's observed policy, made once on a fresh
client and never retried; the probe seals nothing, and the TPM's state
may change before execution, which seals under its own policy. Every
plan records the outcome, under the probe's deadline, beside the
keyboard-console probe's, and names the storage they choose ("Storage
choice"); a device-bound plan's execution formats it encrypted (DESIGN.md
"Device-bound formatting").

It holds at most one review, under the admission rules above. Propose
checks, in order: busy; the settings (the username through `td-firstboot
check-primary-name` against the verified root, the hostname grammar, the
keyboard layout, which admits only `us` until a keyboard catalog exists, and
the running system's timezone catalog, as volume formatting uses); a fresh
candidate discovery, less the disk the source is stored on (resolved from
the source's device number through the kernel's block inventory, a partition
to its disk; a source whose disk is not a whole `sd`, `vd`, `nvme` or `sr`
disk standing on no other device, or one it cannot resolve, refuses
discovery), which must contain the proposed destination exactly, and whose
observation the review then carries; the read-write exclusive claim of
DESIGN.md (a disk held elsewhere is refused as busy; any other failure to
claim a disk discovery just listed, as changed); source validation
through `td-boot validate-source` while that claim is held, whose manifest
digest must be the one `/run/td-deployment` records for the running root;
the nonce and version-4 volume UUID from `/dev/urandom`; payload fit
(DESIGN.md "Payload fit": a volume short of the deployment is insufficient
space, a source the fixed ESP or the kernel bound refuses is source
unavailable); and a recheck of the claimed disk against two inventories. Any
refusal releases the claim. Only then is the review held and returned.
Execute rechecks the held disk; a disk that changed abandons the review.
Without a consent channel, or for a review that channel cannot carry, it
then answers consent unavailable and keeps the review. Otherwise it sends
the review report, answers awaiting consent, and waits for td-authd's answer
while it goes on serving the installer. A decline abandons the review as the
consent channel section maps it. Consent rechecks the held disk once more (a
change abandons the review as destination changed), then writes the started
report before the execution exists (if td-authd cannot be told, the review
is abandoned as consent unavailable and nothing is written), moves the claim
to an execution on its own thread, and answers running, with the phase the
execution last reported, until it reports finished and the service is
complete or failed. Withdraw releases a review that has not started. A
consent greeting that fails or stalls for ten seconds leaves the service
without a channel. A malformed answer, a second answer to a started
installation, a report that cannot be written or a closed consent channel
ends only that channel: a displayed review is abandoned as consent
unavailable, later executes are refused as consent unavailable, and a
running installation continues. A reply whose write stalls for ten seconds,
like any other lost reply, counts as losing the installer. The wire carries
only a refusal's code, so the cause of each discovery, settings, claim,
source, fit or recheck failure is written to standard error; the time zone
catalog's, kept with the catalog, is written once. While a review is held,
its own claim keeps the held disk out of destinations replies; that omission
is not a disk change. Each request is served in turn, and the next is not
read until the reply to the last is written, so an installer that does not
read cannot queue requests; it can still repeat source validation at
will, which the caller that starts the service bounds.

The service has an execution (DESIGN.md "Executing a consented
installation") and opens the consent channel, so an execute td-authd answers
with consent installs. Tests drive the core's two ends with fakes and the
execution on a regular-file disk; `qemu-install-system` drives the service
itself as root in a guest, as both of its peers, onto a disposable disk it
then boots (td-install-qemu-test/DESIGN.md), and `qemu-install-encrypted`
drives it under an emulated TPM with a display and keyboard, typing the
recovery key back, and without either probe's device (ENCRYPTION.md
"Acceptance evidence"). The service holds the source
directory open for its life, so the medium's filesystem, and the kernel's
exclusive claim on the disk under it, outlive even a lazy unmount, and that
disk is never claimable as a destination; the live root, a loop over the
medium's `root.erofs`, holds it too. The source itself is read by path, so a
detached medium refuses: discovery first cannot place its disk, and a medium
whose drive remains is no longer authenticated. Discovery also excludes that
disk by name, as resolved, independently of the claim. Trusted consent
remains required before execution.

## Installation consent channel

`installation_consent.rs` defines the private channel by which td-authd,
which starts the service, admits a wizard-confirmed execution and tracks
its lifecycle. The channel name is internal; it supplies no second human prompt. td-authd compiles the same file. It is data and codec
only. Both ends are root and td-authd creates the channel, so decoding
authenticates nothing; it refuses only bytes outside the grammar. The
service core speaks it, and td-authd opens it for the service it starts on a
live boot (td-authd/DESIGN.md "Whole-disk installation intake") as one end
of a socketpair on the service's standard output, where the service opens
it.

Both ends first send and require `TDINA03\n`, which changes with any message
or its bytes. Each message then travels in one frame: a big-endian u32
length, nonzero and at most 1024, then that many payload bytes. A payload
is a tag byte and its body, with trailing bytes refused. Every nonce is a
review's 32-byte nonzero nonce.

The service reports:

- `0x01` review: the nonce, the disk's kernel name (a one-byte length, then
  at most 31 ASCII letters, digits, `_` or `-`, the kernel's own bound), its
  nonzero capacity in bytes as a big-endian u64, its model then serial
  (each `0` for absent, or `1`, a big-endian u16 length and at most 256
  UTF-8 bytes, possibly none), the hostname (at most 63 bytes) and username
  (at most 32), each nonempty UTF-8 behind a one-byte length, the
  32-byte deployment manifest digest and the plan's storage byte (0
  unencrypted, 1 device-bound). It is at most 722 bytes. These are
  the facts of the held review after its exact Execute request. The
  requester is not on the channel: td-authd admits only the live wizard.
  It acknowledges this nonce immediately without opening secure attention.
- `0x02` ended: a nonce and a code, 1 withdrawn, 2 destination changed
  (including a recheck of the held disk that failed after consent), 3
  installer lost or 4 not consented. No destructive write started.
- `0x03` started: a nonce. Consent arrived and the final recheck passed;
  the first write follows.
- `0x04` finished: a nonce and an outcome, 1 complete or 2 failed, after
  which the disk may be incomplete.

td-authd answers with a high-bit tag, so a reflected frame never decodes:
`0x81` consent, meaning root admitted that wizard-confirmed execution,
or `0x82` declined with a code, 1 declined, 2 expired or 3 unavailable.
The current live broker sends `0x81`; the service still fails closed on
a refusal, a missing acknowledgement, or loss of the private channel. Unassigned codes and tags
refuse, and a duplicated code does not compile.

Order: one review is open at a time, from its review report until the
service closes it with exactly one ended report, or with started and then
finished. The service sends a review only when none is open, and started
only after consent to the open review. td-authd answers an open review at
most once, and never after reading its ended report; an answer never closes
a review. After a decline the service closes the review as not consented,
and records the matching abandonment: consent declined, consent expired or
consent unavailable. Answers can cross ended reports in flight, so the
service ignores an answer that names no open review; a second answer to
the open review ends the channel. td-authd ends the channel on a report
out of this order. Until started, losing the installer ends the review as
installer lost; once started, an installation reports finished whether or
not its installer remains. A held review the channel cannot carry, such as
one whose disk name exceeds 31 bytes, is refused at execute as consent
unavailable and kept.

## Choosing the volume identity

`td-install new-volume-uuid` takes no operands and prints one canonical
lowercase version-4 UUID and a newline. It uses the formatter's existing
kernel random-device reader; each request draws a new value. It opens no
destination, writes no disk bytes, and does not reserve a UUID or grant
destructive authority. Random values are not hardware authentication,
secrets or a proof of global uniqueness. The live environment must provide
its trusted kernel random device.

The caller requires successful exit and complete output, retains that
value once per proposed installation, and supplies it unchanged to
`prepare-selector` and `format --uuid`. Output failures are failures even
if a partial line was written. Plan retention, review and consent remain
service responsibilities; retrying this command chooses a different value.

## Preparing the installed selector

`td-install prepare-selector TEMPLATE VOLUME-UUID OUTPUT` makes
a private selector copy bound to the chosen volume. The UUID must be
canonical, nonzero lowercase text, using the same admission as
`format --uuid`. The installation service chooses it once for its plan
and passes the same value to this operation and formatting. This command
neither generates an identity nor authorizes a disk operation.

The caller supplies an already verified selector template and trusted,
stable source/output ancestors. The template is the stock selector, which
carries no identity (td-boot's `SELECTOR_TEMPLATE_PATH`); this operation
preserves all its bytes. It must never be used to modify the
manifest-covered deployment initramfs. No archive parser is added:
ordinary nonempty files pass byte admission, so the caller must establish
that TEMPLATE is the correct boot artifact.

The template is admitted through the same descriptor-pinned regular-file
reader as EFI inputs. Symlinks and non-regular inputs refuse. The complete
prepared file, including padding and appendix, must fit the formatter's
256 MiB EFI input bound. All admission and appendix construction precedes
exclusive output creation; existing files, links and device nodes refuse
without being changed. The held template is streamed through the existing
bounded EFI copier, which rejects length changes. Same-sized concurrent
mutation remains outside the trusted, stable-source contract.

The copy gets zero padding to a four-byte boundary and a deterministic
newc archive containing root-owned mode-0755 parent directories and a
mode-0644 `etc/td/volume-uuid` with the UUID and one newline. The shared
engine writer supplies the header and trailer rules. The pinned kernel
reads concatenated archives and replaces an earlier regular file of the
same name, so the appended UUID is the one the selector reads.

Success has no stdout and requires the complete output, exact mode 0600
and file sync. Output is temporary preparation data, not a published
installation or a directory-entry durability guarantee. I/O or sync
failure can leave a partial private output; callers require successful
exit and use a new output name after failure. The formatter later copies
this prepared file into the ESP and owns destination sync. Preparation
writes no block device and performs no source deployment publication.

The small and full-system installation fixtures carry a trusted selector
template without a volume UUID, as the stock selector is. In the live guest,
after source validation, they prepare the selector with the fixture's chosen
UUID and pass that copy to `format`, so the installed boots depend on the
appended identity. Each live installation calls `new-volume-uuid`; the ISO
carries no destination UUID. The host validates one canonical identity
report against the primary Btrfs fsid in its private disk image and rejects
reuse across successful installs and interrupted reinstalls. Detached boots
must bind that exact UUID across every existing bus/media case. Immutable
plans and installer confirmation remain separate work.

## Media, boot and persistence

[MEDIA.md](MEDIA.md) specifies the hybrid format and its current formatter
boundary.

Use source-built target executables and declared inputs throughout image
composition. Host-seeded control-plane executables never enter the image.
Any marked foreign application payload remains subject to AGENTS.md and
APPLICATIONS.md, including the dedicated read-only payload-input channel.
No new external dependency is approved by this document.

The media's content is built reproducibly and carries no key or signature
(DESIGN.md §6). The installer and installed boot verify every payload
against its manifest; neither authenticates who produced a deployment.
UEFI bootability does not claim Secure Boot authentication.

Optical and USB boots enter the same live profile, with the installation
source read-only and live mutable state volatile. The installed profile
uses persistent Btrfs state. Installed boot must select the destination's
volume without assuming `/dev/vda`, and continue working when device order
changes or installation media is absent. Selector and deployment initramfs
must agree on volume identity across kexec. The fixed ESP stub retains the
deployment-selection boundary in DESIGN.md D5.

Publish the bundled deployment through td-boot, preserving payload
verification and transactional current/previous bookkeeping. Reuse the
td-install GPT/FAT32 implementation and its file-image test path. Check
scratch-space requirements before erasure; the current formatter stages
deployment contents and a filesystem image, so free destination capacity
alone cannot establish that a live session has enough temporary space.

Machine settings are bounded, validated persistent data outside immutable
deployment bytes. The installed account retains the existing single-human
UID/GID allocation; user-selected names must not collide with system or
application accounts. Account databases, home paths, application grants,
service configuration and automatic login must agree before the session
starts. No shipped program assumes the stock account's name or
`/home/tester`: grant preparation, jail admission, authd's task
directory, the jail's Firefox download probe and the
guest helper's workspace share one validated UID-1000 account lookup. The
compositor sends typed launch requests without paths, and its direct
development launcher offers no task terminal; a compositor source check
pins that none of its sources names `/home/tester`. The stock
account is `tester`.
The stock console uses `td-login login-primary`, and human-UID service
commands use `td-login exec-primary`. Both selectors resolve the current
UID-1000 account and retain td-login's existing authorization and credential
checks. The initial terminal, its readiness probe and workspace selection
use literal argv through that launcher. The existing `env` program sets
the terminal control socket after the credential transition. The paired
terminal authority uses `terminal-serve --primary` and
passes the resolved name through its existing ledger and credential checks.
Firstboot's `--application-primary` selects the same account's canonical
home before provisioning writes, retaining application state migration
and ownership checks. The VM helper's private identity, SSH
configuration and development layout also derive their home from that
primary account and retain the persistent mount and ownership checks.
The boot state-ownership and SSH host-key permission checks run as the
validated primary account; home writes and cleanup use its runtime HOME
with dropped credentials. Their root pre/post cleanup clears only the
fixed system-state probe paths.
Boot health runs each unprivileged probe through that launcher
(`exec-primary -- /bin/sh -c`), as does the network self-test, and
resolves the primary name once through it for the literal `exec-as`
credential probe. SSH and Git clients use the credential-selected USER
value, including when a probe substitutes a temporary HOME. The
physical-input download oracle obtains HOME through that launcher and
removes only its two fixture names after dropping to the
primary account. A failed lookup or nonzero removal status prevents the
input test; an absent directory is still diagnosed by the later grant
and download probes. The immutable `/home` alias to `var/home` makes the
account's canonical HOME and the provisioner's persistent home agree.
The SSH server's self-test Match configuration also uses the admitted primary
account, as specified below.
The early account-profile operation below activates the saved username before
these consumers start. Updates retain the saved identity and settings and
revalidate them against the newly selected deployment. An update that introduces
a collision with the saved name refuses at boot. Update-time account preflight
before publication is still required as a future availability improvement;
rollback does not repair malformed shared state.

`td-firstboot check-primary-name ROOT NAME` is a read-only preflight for a
proposed name against an already verified, staged deployment. It shares the
primary-account grammar: 1–32 lowercase ASCII letters, digits, underscores
or hyphens, beginning with a letter. Names formed from `tda`, `tdb`, `tdc`,
`tdi` or `tdp` followed only by digits are reserved even before that
principal is enrolled, so later enrollment cannot collide with the installed
name.
Rechecking the deployment's current primary name is allowed when it passes
these checks. It requires regular, consistently owned
account tables (passwd/group 0644, shadow 0600, principal reservations 0444),
rejects account/group collisions and reserved service/application names,
and requires a matching human primary group and complete shadow roster.
Its exact public account-table modes match the generated image and shared
live account reader; the general `check-principals` command instead checks
reservation integrity and excludes unsafe writers without pinning those
public modes.
Unknown or duplicate supplementary-group members refuse, so choosing a
previously unresolved member name cannot silently acquire group authority.
It prints `TD-PRIMARY-NAME-CHECK-OK` only on success and never writes settings
or account files. The caller supplies the staged root and must separately
verify deployment authenticity; this command does not authorize erasure,
enroll retained reservations, validate service configuration or activate the
selected account. The volume formatter and full-system installation fixture
use this preflight as described below.

`td-firstboot stage-primary-name ROOT NAME OUT` prepares that account
configuration from the same caller-verified deployment. It validates the
complete input and renamed tables before creating a new private output
root. `OUT/etc` contains passwd/group at 0644, shadow at 0600 and the
unchanged principal reservations at 0444. Only the human account name,
canonical `/home/NAME`, matching primary group name and exact
supplementary-group member references change. Numeric identities,
password/lock fields, other account fields and application reservations
remain intact; the renamed result passes the same complete admission.

The output parent must have the input tree's owner and exclude other
writers. The invoking uid/gid must match that owner, and `/proc` must be
mounted for descriptor-relative access. Parent and output directories are
descriptor-pinned, existing
output roots refuse, and files are created exclusively with their exact
modes. The output root is 0700 and its `etc` directory 0755, independent of
umask. Success is reported as `TD-PRIMARY-NAME-STAGED` only after files and
directories are synced. A write failure can leave a private incomplete
output; callers must require success before consuming it and use a new
output for a retry. This command does not modify the input account files or live `/etc`,
create homes, save an installer choice or start a session. Deployment
verification and boot-time publication remain the caller's responsibility.

`td-install volume` accepts `--username NAME VERIFIED-ROOT TD-FIRSTBOOT`
after the regional settings and before its destination operands. The caller
binds the absolute source-built validator and verified, stable deployment
root, just as it binds the formatter and publisher tools. These paths are
control-plane inputs, not wizard fields. Before opening the destination or
clearing scratch, the formatter validates the bounded name syntax locally and
requires successful `check-primary-name` for that root and name. Complete
roster, collision and reservation admission remains the validator's job.
The volume formatter seeds only `@var/lib/td/username`, a mode-0644 file
containing the name and one newline under mode-0755 parents. A caller doing
layout first must perform the same preflight before layout; volume cannot
retroactively protect an earlier write. This primitive supplies no consent.

`td-firstboot prepare-primary-profile ROOT` runs over a verified
deployment with persistent `/var`, fresh volatile `/run`, and procfs mounted.
The calling initramfs retains devtmpfs while this operation runs: spawning
its mount applet with null stdin/stdout requires the real `/dev/null`.
Devtmpfs is unmounted only after early preparation, before switching roots.
Before writing, it admits the complete root-owned account and principal
tables. An absent saved username retains the deployment's primary name.
When present, the file must be a single-link root-owned regular file at
exactly mode 0644, at most 33 bytes, with the same name grammar and one final
newline. The setting read descends through descriptor-pinned real root-owned
mode-0755 ancestors; missing
optional `var/lib` or `var/lib/td` means no choice, while aliases, bad modes,
wrong ownership and malformed content refuse without repair.

A selected name is staged through the complete account-table admission above
in a new private `/run/td-primary`. The provisioner binds its prepared passwd,
group and shadow files over the corresponding deployment paths and remounts
each read-only, nodev, nosuid and noexec. It checks inode identity and requires
write-opens to fail specifically with EROFS, then re-admits the complete live
account set and verifies the selected name. The principal reservation table
stays on the immutable deployment. The saved-setting descriptors close
after reading; later staging and mount paths rely on the caller's serialized
boot environment and the private prepared tree. No user process, SSH
daemon or competing root writer may run until this sequence succeeds. A partial failure stops
boot; the next boot starts with fresh volatile state. This is publication
before consumers start, not a multi-file atomic update for running readers.
The raw EROFS payload is never edited. With no saved name, its original
account tables must also pass the read-only checks.

Only after coherent publication does the operation derive `/var/home/NAME`
from the UID/GID-1000 account and prepare the primary home. ROOT, its `var`,
and `var/home` must be real root-owned mode-0755 directories. The caller
supplies trusted ancestors and serializes the operation; the provisioner does
not verify the deployment or establish the persistent mount itself.

An existing primary home must be a real UID/GID-1000 directory. Its
inode, contents and ownership are retained; after ownership validation,
its open descriptor restores mode 0700 and syncs any permission change.
Aliases, different owners and non-directories refuse without repair.
A new home is prepared in an exclusive hidden sibling, assigned its final
ownership and mode and synced before rename and parent sync. Up to 64
exclusive creation attempts handle collisions; time and PID are only name
hints, and a pre-1970 clock is allowed. Old staging names are not reused
or removed. Successful stdout is exactly the canonical persistent home path and a newline.
A failure may leave a hidden staging directory after an abrupt stop, or
a complete final home if the final sync fails; it cannot publish a
partially owned home. There is no live rename or home-adoption interface.

The image recipe normalizes the deployment root to mode 0755 and checks
its complete primary-account tables before packing. The deployment
initramfs packs the source-built static provisioner and runs this operation before unmounting procfs or entering the root. The
selector does not pack it. Generic early directory creation excludes the
primary UID. Downloads preparation derives its path from the returned
home and retains its existing grant policy; it records the successful
self-bind path in a root-owned mode-0600 file under `/run`. Shutdown uses
that private record to release the same bind before `/var`, after the
application and portal views are released. Invalid home ownership or
type stops boot and requires inspection from a trusted recovery
environment; user changes to home permissions do not prevent boot.
A successful profile emits `td-firstboot: primary home created|present
/var/home/NAME`, saying whether this boot published the home or found it,
and then `TD-PRIMARY-PROFILE-READY NAME` on stderr after account readback
and home preparation; stdout contains only the home path. Installed-boot
oracles require the home line once, `created` on a new machine and
`present` on a kept one.
The full-system QEMU installer selects `alice` from the verified ISO payload
before formatting and requires exactly one matching marker on every installed
boot, with the home line; its application boot, which runs to a clean
shutdown, must also have every compositor session's clock name
Europe/London, the zone it chose. This covers persistence across media
removal and repeated boots.
The stock QEMU boot instead requires exactly one `tester` profile marker,
covering the branch with no saved username. This does not add a live account
rename, migrate another home, enroll a PIN or provide the wizard's
destructive authorization.

`td-firstboot render-primary-sshd ROOT` is an early boot operation using
the same complete root-owned account admission as home preparation, which
writes only under ROOT's volatile `/run`. It publishes the full fixed SSH
server policy with one Match block for that primary account, in the form
the login state under the same ROOT selects
(td-login/TOKEN-LOGIN.md, "SSH"): the ordinary policy only where that
state is unenrolled, and otherwise the enforced form, which admits only
that account. Neither admits root (td-login/TOKEN-LOGIN.md, "SSH"). A
state it cannot read is unavailable, so it renders the
enforced form; an account failure still fails the render. The caller
owns deployment verification and serializes account publication before
rendering. No name or policy fragment comes from argv, the environment
or mutable home content. Both forms keep the per-machine persistent
authorization file, which admits no account on a td image
(td-login/TOKEN-LOGIN.md, "SSH"), and the distinct volatile,
loopback-restricted human self-test key. Only the ordinary form additionally
consults the primary account's `.ssh/authorized_keys`; its contents authorize
remote Ed25519 keys but never configure the server. A deployment older than L7
reads that file as root's, so a rollback to one re-admits any root key
it holds, among the root paths such a rollback restores (APPLICATIONS.md
§L.1, "The rollback window"). It runs after
profile publication, right after `td-firstboot ensure-login-directory
/sysroot`, whose refusal never stops boot (td-login/TOKEN-LOGIN.md,
"The login record").

Stage-1 init runs it before releasing procfs or starting users. It
publishes `/run/td-sshd.conf` (td-login/THREAT-MODEL.md §1 states how),
then writes the cutover record (td-authd/DESIGN.md amendment 7). Failure
stops boot.
The supervised server explicitly requires that path. There is no optional
include and no immutable config with a stale human name. Only root can
modify its file or parent. The complete policy formatter is shared with
the source-built OpenSSH test, which checks effective authorization for
a renamed human, the stock name and root, and that each form's effective
configuration for each of them carries exactly that form's
`PermitRootLogin` and `AllowUsers` lines. It reads `sshd -T`'s report and
attempts no login, so the refusals themselves are `qemu-login-system`'s
(td-login/TOKEN-LOGIN.md, C11) and, for root, the autotest boot health's
`TD-ROOT-SSH-REFUSED` (td-login/THREAT-MODEL.md §8). The
generated policy contains no secrets
and is recreated on every boot, including deployment updates.

Keyboard and timezone choices must actually affect the installed session;
only supported choices with available data may be offered.

`td-install timezones` provides the read-only catalog for settings
selection; the installation service's time zones reply carries the same
catalog's identifiers, so the installer reads no files. It accepts no
operands and reads only the deployment's `/etc/zoneinfo`: `zone1970.tab`,
`iso3166.tab`, and referenced TZif files. Its JSON `version: 1` is a schema
version, not a tzdata release. `source` is `zone1970.tab`; `timezones` is
sorted by IANA `id`. Each entry carries `countries` with upstream `code` and
`name`, and an upstream `comment` (empty when absent). Country order follows
the upstream zone row. `Etc/UTC` is added with no countries and the comment
`Coordinated Universal Time`. Backward aliases and fixed-offset alternatives
are not enumerated.

The catalog permits at most 512 countries and 1,024 zones including UTC.
Each table is limited to 128 KiB, each line to 2,048 bytes, country
names to 256 bytes, zone IDs to 64 bytes, comments to 512 bytes, and
each zone file to 64 KiB. Empty tables, duplicate or unresolved entries,
malformed UTF-8, unsafe zone IDs, and missing or non-regular files
refuse the whole catalog before JSON output. Zone IDs have two or three
slash-separated components, each starting with an ASCII uppercase letter
and otherwise containing ASCII letters, digits, underscores, plus or
minus signs. This matches the application launcher's name bounds. TZif
screening requires a complete 44-byte v2/v3 header; it does not parse
transitions or establish semantic validity. The tzdata recipe's native
checks own that validation and also run this exact catalog reader
against every realized output. File leaves cannot be symlinks, but the
deployment's root symlink into its immutable store is supported. Parent
paths are trusted deployment data, not a defense against a concurrent
privileged writer. Output errors fail the command and may leave partial
JSON. This command neither selects nor persists a timezone. The
compositor reads the selection written by the volume formatter below.

`td-install volume [--uuid UUID] [--timezone IANA-ID] [--hostname NAME]
DESTINATION MKFS SCRATCH --seed` accepts an optional catalog selection. It
validates the entire offline catalog and selected identifier before
opening the destination or creating scratch state. Unknown names and
backward aliases refuse. The formatter seeds one mode-0644,
newline-terminated name at `@var/lib/td/timezone`; its parent
directories are mode 0755. Omitting the option leaves that file absent.
The deployment-owned optional persistent `/etc/timezone` link points to
`/var/lib/td/timezone`, so deployment updates preserve the choice and
first boot does not overwrite it. The QEMU installation oracle selects
`Europe/London` and verifies the file after formatting and on both cold
boots with the installation media removed. The full-system oracle also
requires mail, news and Firefox startup evidence and clean guest
shutdown in one additional direct selector boot of that installed disk.
Only this additional boot supplies the existing autotest command-line
token; the two firmware boots remain unchanged. The full-system
diagnostic ISO seeds the standard VM loopback-restricted SSH self-test
authorization and mode-0600 test key in its disposable volume, which the
autotest health probe requires. Ordinary installer artifacts do not
contain that diagnostic fixture. The full installed-system boots use a
4 GiB test VM: the current roughly 3 GiB deployment verification can
fill a 2 GiB guest's page cache before kexec allocates its pages with
`__GFP_NORETRY`. td-kexec drops the clean page cache and retries once on
ENOMEM; the 4 GiB stays as a deliberate margin so the oracle does not
depend on that fallback. This is an oracle budget, not a minimum-memory
hardware qualification. The small diagnostic matrix retains 2 GiB.
The application-evidence boot uses the shared system-test timeout,
which covers the longer autotest profiler prerequisite; installation
and ordinary firmware boots retain their separate 900-second default.

`--hostname` selects the installed machine name before destination or
scratch access. Options have the displayed order; duplicate, misplaced,
missing and non-UTF-8 values refuse. Names contain 1 to 63 lowercase ASCII
bytes in dot-separated labels. Each label starts with a letter, ends with
a letter or digit, and otherwise permits letters, digits and hyphens.
The formatter seeds `@var/lib/td/hostname` as one newline-terminated name,
mode 0644 beneath mode-0755 directories. Omission leaves the file absent.

The deployment owns `/etc/hostname` as a persistent link to that file and
ships its default in immutable `/etc/hostname-default`. The existing
`hostname` startup unit runs `td-firstboot hostname` before other sysinit
units. It validates an existing saved name without changing it. Only
absence initializes the deployment default, using the provisioner's
synced temporary-file and rename protocol. An invalid, symlinked,
non-regular, oversized, wrongly owned or wrongly permissioned file
refuses; it is never replaced with a fallback. Reads are bounded to 65
bytes, accept at most one trailing newline, and require mode 0644 and
root ownership. Parents are trusted root-owned system directories;
concurrent privileged replacement is outside this boot-time contract.
This operation has no alternate paths or name operand and is not a
post-install rename interface. Provisioning remains a serialized startup
operation, not a concurrent settings writer. Its read bounds and its
write protocol live in `td-firstboot/src/saved.rs`, which td-authd's
`set-hostname` also compiles, so the rename writes the same canonical
file the same way.

The provisioner sets the kernel name through the shipped `/bin/hostname`
applet, then reads it back from procfs before reporting
`TD-HOSTNAME-READY NAME`. Deployment health requires that unit to
succeed; the serial console retains its independent recovery path.
`/etc/hostname`, the kernel name and the existing network/app-jail
consumers therefore share the installed choice from boot;
APPLICATIONS.md §L.1's `set-hostname` says which of them a rename
changes at once and which at the next boot. Later defaults do not
replace saved state. Malformed shared state therefore prevents
acknowledgement across deployments; rolling back does not repair it.
Recovery requires restoring a canonical, root-owned mode-0644
`lib/td/hostname` in the volume's `@var` subvolume from a trusted
recovery environment. The in-system rename is `set-hostname`
(APPLICATIONS.md §L.1, live since its L4): `td-authd request-hostname
NAME` queues the name and, after the session owner's consent, root
rewrites the saved file canonically, which the next boot activates. It
replaces only a valid saved name, so repair keeps this recovery and has
no in-system UI. The saved account and
hostname activate at boot. The installer sets them up; a user-facing
recovery flow remains to be implemented, and no root path exists for
it: root has no login (APPLICATIONS.md §L.1). The QEMU
installer selects `td-qemu-installed`, checks its
saved bytes alongside timezone state, and requires activation on both
full-system cold boots and the additional application-evidence boot.
Unit tests retain the same saved inode and bytes across a changed
deployment default.

The existing application launcher reads that name and binds each runtime's
own zone file at its jailed `/etc/localtime`. Static mail and news carry
the source-built data in `static-runtime`; Firefox uses its reviewed
Freedesktop runtime data. Missing runtime zones refuse launch.
The compositor snapshots the saved name and source-built TZif rules at
startup, rendering local civil time with its actual UTC offset. Invalid
settings or unsupported data show `CLOCK ?`; an absent setting stays UTC.
See `td-compositor/DESIGN.md` for reader bounds and DST proof. Post-install
account, keyboard and timezone setters remain separate increments.

The `tzdata` recipe compiles the approved IANA 2026e data-only source
with td's imported Debian glibc `zic`. Its output contains fat
TZif files at `share/zoneinfo`, including backward-compatible aliases,
geographic tables, and the upstream version and license. It uses
ordinary POSIX time without leap-second corrections. Fat output is
required by the pinned glibc compiler: its older `zic` miscompiles the
2026 Canadian transitions in slim mode, dropping the correct
daylight-saving flags and abbreviations. The native check covers those
transitions in all nine affected zone names. The upstream `Factory`
placeholder for an unspecified timezone is intentionally omitted; it is
not a selectable civil timezone. The upstream default source set omits
`backzone`; merged locations share their canonical zone's pre-1970
history. This data does not claim complete local histories before 1970.
The recipe does not install a second libc or timezone compiler. Its
native recipe check reads the realized geographic tables and requires
their named zones to exist, then uses td's `zdump` to verify exact 2026
Canadian and 2027 UTC, daylight-saving and fixed-offset behavior.

The complete data output ships at its canonical store path in the system
deployment, including live and installed profiles. The immutable
`/etc/zoneinfo` link exposes compiled zones and geographic tables for
offline selection. Source-built td glibc's compiled `TZDIR` is
`/td/store/glibc-2.41-x86_64/share/zoneinfo` and its `TZDEFAULT` is
`/td/store/glibc-2.41-x86_64/etc/localtime`. Later settings consumers
must explicitly connect the selected data to native readers; libc does
not discover `/etc/zoneinfo` automatically.

## Independently landable increments and evidence

1. Record the agreed v1 scope and its activation boundaries here.
2. Add EFI-capable kernel artifacts and verify their realized format. Finish
   fixed-stub/initramfs packaging and a firmware boot oracle; keep the
   existing direct-kernel oracle as a separate test.
3. Implement deterministic hybrid media composition and the read-only live
   boot profile. Boot the same artifact as optical media and USB mass
   storage, without QEMU `-kernel` or a supplied initrd.
4. Add validated persistent machine configuration and its installed-profile
   consumers, including volume discovery across selector and deployment
   boots. Test settings retention across a deployment update.
5. Add bounded device discovery, immutable plans, trusted destructive
   consent and the service's installation execution. Prove refusal of
   installation media, in-use targets, stale identities, unsupported
   destinations and invalid plans without modifying their bytes. The
   service opens its consent channel within this increment, proven by the
   full-system QEMU oracle driving it as both peers; td-authd's intake and
   the compositor's presentation are proven end to end in increment 6.
6. Add the native wizard, target recipe and live startup integration. Use
   native compositor tests for navigation, rendering, input, errors and
   progress; fixtures cannot grant ordinary clients installer confirmation. A live
   boot starts the wizard ("Live startup"), and `qemu-boot-live` drives it
   with physical keys through the service's review and the compositor's
   consent to an installed disposable disk, which then cold-boots through
   firmware as the configured account and host.
7. Activate version 1's profile only after the end-to-end QEMU evidence:
   boot the ISO, complete the UI flow onto a disposable disk, detach the
   media, boot that disk through firmware, and observe the configured
   account in the compositor with its settings and persistent home.
   Increment 6 proves the flow and the firmware boots with the account,
   host and home reported, the zone named by the compositor's clock and
   shown on it, and the account's login shell keeping a file in the same
   home across both boots, and `qemu-boot-live` boots the ISO itself
   through firmware as USB mass storage. The profile is active; the parts
   of version 1 named at the top of this document remain.

Use per-run disposable disks and firmware variables. No test discovers or
opens an operator's real disk for writing. Exercise both supported media
attachments (`qemu-boot-live` alone boots only USB), corrupt deployment
payloads, insufficient capacity and scratch, interrupted installation,
changed disk ordering and a second installed boot. Require actual
rendered/input and installed-session evidence, not only serial markers
printed before the relevant operation completes.

## Read-only block inventory diagnostic

`td-install inventory` takes no operands and prints one version-1 JSON
object with `scope: "inventory-only"` and a `devices` array. It observes the
fixed `/sys/class/block` tree; it never opens `/dev`, invokes a child,
mounts, writes sysfs, or calls the layout/volume writers. There is no
alternate sysfs root in the CLI. The internal root parameter supports
owned filesystem fixtures in tests.

This diagnostic deliberately includes partitions, mounted disks, media,
read-only disks and stacked devices. It is not the eligible destination
list specified above and is not connected to the wizard or write path.
No field grants permission to install. Source/mount/swap backing-device
resolution, unsupported-topology filtering, exclusive admission, immutable
plans and installer confirmation remain required before destination selection can
activate writes. In particular, `holders` and `slaves` alone do not account
for loop backing files, mounted filesystems or every Btrfs member.

Each record contains the kernel name, decimal major:minor device number,
capacity in bytes, read-only flag, whole-disk parent name for partitions,
the partition number, and sorted holder/slave names. A whole disk reports its disk
sequence number, logical sector bytes, removable flag, model, serial and
WWID. Logical sector bytes are reported as observed, including zero for
some no-media devices; consumers must validate geometry before using it
in arithmetic or admitting a destination.
Descriptions absent from the driver are JSON null; existing empty fields
remain empty strings. Serial prefers the disk's own `serial`, then
`device/serial`; WWID likewise prefers `wwid` then `device/wwid`, and model
reads `device/model`. Serial and WWID remain distinct even when a driver
provides only one. For identification reads only, ENXIO means absent VPD
data under the pinned Linux 7.2.13 SCSI interface and produces null (or the
fallback attribute). Other read errors still refuse the observation. These
descriptions and the
kernel identifiers do not authenticate hardware or survive every reboot.
Partitions have no separate disk record: their parent's geometry applies.
Linux's `size` is multiplied by 512 even for a 4096-byte logical sector.

Collection admits at most 4096 block nodes, 16384 directed relationship
entries in total, and 256 bytes per attribute. Exceeding a bound, invalid
UTF-8/numeric metadata, missing required metadata, an unresolved parent,
duplicate identity or inconsistent reciprocal relationship is an error.
Canonical relationship destinations must match the referenced node.
Partitions have a `holders` directory and no required `slaves` directory.
Optional descriptions and the partition discriminator may be absent;
other I/O failures propagate with their path. Text is JSON escaped, and
node and relationship order is deterministic. A successful empty inventory
is an empty array, not evidence that installation is possible.

Two complete observations must agree before any JSON is printed. This
catches changes between reads but is not an atomic snapshot or protection
against removal/replacement after collection, or a change and reversal
between observations. Bounds limit bytes and entries, not elapsed kernel
I/O time. A failed collection prints no partial inventory; an output I/O
failure can leave partial JSON and a nonzero exit status. Consumers must
require successful exit and a complete document.

The QEMU installation diagnostics exercise this inventory before source
verification and after partition refresh. Their host compares it with
oracle-owned optical/USB images and writable, read-only, undersized and
4Kn targets, plus SATA/AHCI targets at 512-byte geometry and direct NVMe
namespaces at both geometries. Partition parents and disk identity
continuity are checked on successful/recovery installations; refusals must
have no post-format report and retain the existing whole-disk byte preservation proof. These
are observations under a disposable topology, not destination eligibility.

## Advisory destination discovery

`td-install destinations` prints the inventory's version-1 device schema
with `scope: "candidate-only"` and only currently available candidates.
It takes no operands, writes no disk bytes and uses no child processes.
Direct virtio, SCSI/SATA and NVMe whole-disk names are supported; partitions,
optical, loop, device-mapper, RAID and path-specific NVMe namespace names
are excluded. Native NVMe multipath is disabled in the pinned kernel.
Its aggregate namespace names can match this lexical filter on other
kernels; those topologies are outside the supported profile. Each
candidate must be writable according to sysfs, have 512-byte or 4096-byte
logical sectors, and fit the formatter's layout. Any holder or slave edge
on the disk or one of its partitions excludes the whole disk.

For each remaining disk, discovery opens a temporary read-only Linux
O_EXCL claim. A busy claim excludes that disk. Other open or metadata errors
refuse the entire observation, with no partial candidate document. The
opened descriptor must be a block device with the inventory's device
number and capacity. A second complete inventory must equal the first
after all probes finish. Collection bounds, JSON escaping and output-error
handling remain the same as inventory. Empty candidates are a successful
observation, not permission to bypass admission.

`td-install candidate-record` applies the same filter, temporary claims and
second full inventory, then emits one canonical `TDCAND01` binary record
to a pipe or file. It refuses a terminal so device-supplied control bytes
cannot be interpreted there. The record contains no trailing newline;
model, serial and WWID remain untrusted UTF-8 and must be escaped by any
displaying client. All fields and the count are admitted before writing.
More than 64 candidates or repeated names, device numbers or disk
sequences refuse the binary command without output; the JSON diagnostic
keeps its existing behavior. A write error can leave a truncated record,
which the bounded decoder rejects. The probe claims close after each disk
and before the second inventory; the command retains no chosen disk.

This requires trusted devtmpfs/sysfs and their ancestors, and the live
profile must retain mounts of the installation medium and every filesystem
backing its files. Those kernel claims, or excluded stacked relationships,
keep that storage out of the candidate set. This command does not discover
an unmounted source by content or track arbitrary file-backed media.
While held, each probe can temporarily refuse incompatible exclusive
operations such as mounts. Concurrent discovery processes can omit each
other's claimed disks even when both inventories agree. Discovery does not
retry busy claims or missing devtmpfs nodes: busy means omitted, and a
missing node or other probe error refuses the complete observation.
The temporary claims close before the report is consumed. Model, serial,
WWID, device number and disk sequence are observations, not hardware
authentication or retained write authority. Matching two inventories is
not an atomic snapshot, a time bound on kernel I/O, or protection against
removal/replacement, changes that reverse between observations, unclaimed
raw I/O or a privileged topology writer.

The service must bind the reviewed identity and settings in its immutable
plan, independently resolve source backing storage, revalidate the
selected device and retain its claim through destructive execution;
"Installation service core" does each.
This advisory CLI does not activate the service or provide that admission,
consent, source verification, scratch or payload-fit checks.

The QEMU host requires exactly its available target in the initial report,
with the same complete identity as inventory, and no candidates while a
whole-disk claim or mounted system partition holds the target busy.
For writable fixture targets at least 6 GiB in size, the guest also runs
the shipped `candidate-record` command and compares its entire bounded
binary reply with an independently framed sysfs observation. The host
requires exactly one guest verification marker. This proves target
execution, not a service channel or UI selection.
Read-only and undersized targets must be absent even though the fixture
still invokes the formatter to test its refusal. Source-corruption cases
also collect this read-only report before source validation; candidates do
not imply a validated source. The installation medium is always
absent. These are private optical/USB, virtio/AHCI/NVMe observations; no
physical-device compatibility claim is added.

The small QEMU matrix also boots one private writable USB source image,
extended to fit ordinary destination geometry. Its read-only ISO mounts
must exclude the medium from candidates, and all three raw formatting
commands must refuse the source open with EBUSY. After releasing every
source mount, an exclusive read-write open must succeed without writing.
Whole-image length and SHA-256 comparisons preserve both source and target
after QEMU is reaped. This tests the mounted-media claim and its release;
the service must keep its source mounts and plan identity alive.
Other media cases remain write-protected, and no operator device is used.

## Plan observation

`td-install observe-plan` takes one canonical binary plan from standard input
and no operands. It refuses malformed or oversized records before accessing
sysfs, then requires the reviewed disk to remain a whole-disk candidate with
the same name, major/minor, disk sequence, capacity, sector size, removable
flag, model, serial and WWID. It opens `/dev/<kernel-name>` read-write with a
Linux exclusive claim, compares the opened block device number and size, and
requires the pre-claim and under-claim sysfs inventories to agree.
No write call is made. Success prints one JSON object with version 1, scope
`plan-observation-only` and the destination name; failure prints no success
document. The claim closes after output, so this diagnostic grants no later
write authority, does not authenticate the deployment or settings, and is
not a replacement for the service's retained claim and installer confirmation.
An unmounted installation medium can pass the candidate filter; the
service retains and excludes source backing storage itself.
The disposable optical/USB QEMU installer constructs a canonical plan from
guest sysfs for its available target and runs the shipped command before
formatting. It requires an exact success report, refusal of a changed disk
sequence without a success report, refusal of a mounted target, and
unchanged first, middle and final device canaries after each check.
Read-only and undersized fixture targets do not run the positive
observation.

`td-install observe-source-plan <td-boot> <deployment-directory>` takes
the same bounded plan from standard input. It runs the shipped
absolute `td-boot validate-source` to read the manifest and verify
all three payload hashes, then compares the canonical
manifest SHA-256 ID byte-for-byte with the plan's deployment digest. It
holds the reviewed disk's exclusive claim during that source validation.
Only after source validation, digest comparison and the post-validation
disk recheck succeed does it print a version-1 JSON report scoped
`held-source-plan-observation-only`, naming the disk and deployment. A
malformed plan or relative path refuses before the claim or child; a busy
disk, failed child, digest mismatch or failed disk recheck prints no success
report. The child
closes its source descriptors and the disk claim closes before return, so
this observation does not retain authority or authorize a later write. It
rechecks the held descriptor's capacity and bracketed sysfs inventory after
hashing, refusing removal or changed disk sequence visible then. The
opened descriptor's device number is reasserted but cannot change for that
inode. Physical hotplug is not prevented, and a change restored before
recheck is undetectable.
The command does not replace the service's source mount and claim lifetime.
The QEMU fixture requires the exact report, a changed digest
refusal, and a mounted disk refusal with unchanged target canaries. A
test-only validator also attempts an exclusive open during source
validation and must receive EBUSY; this pins the overlap, rather than only
the order of two sequential checks.

## Read-only layout preview

`td-install layout-preview <logical-sector-bytes> <capacity-bytes>`
takes two unsigned decimal byte counts, each at most twenty ASCII digits
and within u64. Leading zero padding is allowed within that length
bound; output numbers have no padding. It computes the same GPT
partition layout that `layout` writes, for v1's supported 512-byte or
4096-byte logical sectors. That is the unencrypted layout: a
device-bound installation ends the system volume on whole 4 KiB
encryption sectors, up to 3.5 KiB earlier on a 512-byte disk (DESIGN.md
"Disk layout"). Invalid geometry, unaligned capacity or
insufficient space for the fixed ESP and minimum system volume refuses
before output. It opens no files or devices, reads no sysfs attributes,
generates no identities and starts no child process.

Success prints one version-1 JSON object with `scope: "layout-preview"`,
`logical_sector_bytes`, `capacity_bytes` and exactly two `partitions` in
disk order. Each partition has `number`, `purpose` (`efi-system` or
`system-volume`), inclusive `start_lba` and `end_lba`, `offset_bytes` and
`capacity_bytes`. Integer fields require exact unsigned integer handling;
consumers must not round large capacities through floating-point numbers.
The output is deterministic and newline-terminated. Invalid inputs produce
no partial report; output/flush errors return failure and may leave partial
bytes, so consumers require both complete JSON and successful exit.

This is a geometry preview, not an immutable installation plan or a
destination eligibility decision. It describes the numbers supplied by its
caller and does not bind a device identity. It does not establish source
authenticity, boot-file fit, payload capacity, scratch availability,
settings validity, exclusive admission or installer confirmation.
Those checks remain required before installation.

The QEMU installation diagnostics also invoke this command on actual
virtio/NVMe 512-byte/4Kn and AHCI 512-byte disk geometry. Their host compares
its complete partition report against the attached private disk and GPT
boundaries, alongside post-format inventory and detached firmware boots.
The diagnostic queries after successful layout so negative cases still
exercise formatter refusal; this is not the wizard's sequencing.

## Session validation

For one-boot session validation, run `td-recipe-eval qemu-boot-session`
(optionally followed by `system-x86-64`). It builds a fresh disposable
system volume and boots it under headless QEMU with networking disabled, on
KVM alone; without it the boot is a host gap, and `TD_QEMU_ACCEL=tcg`
emulates on purpose.
The full system validator requires firstboot identity, immutable deployment
configuration, owned writable state, component health, the compositor and
terminal, application workspace placement and browser support. The check
also requires clean shutdown and an offline Btrfs check. It does not
request the physical-input or audio-capture oracles.
Use `qemu-install-system` for the optical/USB installation matrix and
`qemu-boot-system` for the longer install, update and recovery sequence.

## Installer-oracle failure diagnostics

The small and full-system QEMU oracles keep their exact requirement that
selector volume binding precede deployment selection. On a binding failure,
they report the retained line/byte counts, the first exact binding and
selection line offsets, and up to eight lines containing binding or
selection prefixes, including malformed selection delimiters. Each excerpt retains at most 256 UTF-8 bytes before escaping
control characters; escaping can expand the excerpts beyond those input
byte counts. Clipping and omitted matches are explicit. A labeled console
tail follows. These excerpts can expose earlier malformed records
that the tail omits. They are diagnostic context, never substitute evidence
for a missing, reordered or interleaved exact record. Output is limited to
the console retained by the runner, not a complete boot transcript guarantee.

## Installed Ethernet and remote access

The boot-time `netup` attempt is followed by the supervised `network` service
running `td-netd watch`. It polls physical Ethernet interfaces four times a second,
raises their links, and configures the first connected one in lexical order
with IPv4 DHCP. Static addressing is outside this service; Wi-Fi is below.
Plugging in a cable after boot needs no administrative action.
Unplugging it clears the address, default route and DNS settings; reconnecting
or attaching a USB Ethernet adapter obtains a fresh lease. Failed DHCP attempts
retry after 15 seconds. Boot and desktop health do not require a DHCP server.

`td-netd status` shows the latest connection state, address, gateway and DNS
servers, or the most recent failure. The root-only rotated service log is
`/var/log/svc/network.log`; status is readable by the session owner.
For slow transfers, run `td-netd diagnose` before and after the transfer:
it reports negotiated link speed/duplex, MTU, device errors/drops, TCP
configuration and cumulative TCP retransmission counters without elevation.
`up` is
the boot-time one-shot; it refuses while the watcher owns networking. The
watcher inherits a successful boot lease through a root-only volatile record,
so takeover does not interrupt downloads already in progress. Its lifetime
uses kernel uptime including suspend; expired leases are cleared on resume.
It broadcasts
DHCPREQUEST at half the lease lifetime, retries failures, and clears an
expired or explicitly rejected lease before acquiring another. Servers omitting a lifetime receive
a five-minute revalidation window. A DHCP exchange has a bounded deadline
even when unrelated packets arrive. The daemon owns one connection at a
time and changes neither loopback nor the other link on disconnect.

The same watcher is td's Wi-Fi station wherever the kernel drives a
wireless interface. It joins the WPA2-Personal and open networks the
session owner names, and remembers each:

```text
td-netd wifi scan
td-netd wifi list
td-netd wifi join NAME
td-netd wifi join --open NAME
td-netd wifi forget NAME
td-netd wifi forget --all
```

A `--` before NAME admits a name that begins with `--`. `join` asks for
the passphrase without echo (8 to 63 characters, or a 64-digit hex key),
derives the PMK, hands only that to the watcher over `/run/td-netd/wifi`,
and follows the wireless interface's status until an address arrives or
the join fails. While Ethernet holds the connection it saves the network
and returns; Wi-Fi joins when the cable is unplugged. That socket is the
owner's alone (mode 0600, in a root directory that is root's alone until
the socket is the owner's), the owner being the account the `network`
unit names with `--wifi-owner`. The watcher keeps up to 16 networks
root-only in `/var/lib/td-netd/wifi`, one line each, in the order `join`
last named them, so a passphrase is typed once per network; joining a
seventeenth drops the last. An installation from the live medium copies the
live session's file onto the installed `@var` (DESIGN.md "Carried Wi-Fi
networks"), so a network joined before installing is joined again at the
installed system's first boot. `list` names them, never their keys,
and `forget` removes one, or all with `--all`. A `join` puts its network
first in the list before the outcome is known and, when the radio is
free, attempts it alone, so the outcome it reports is that network's. Every later
association, at boot or after a loss, takes the first saved network, in
the order `join` last named them, with an access point in range that fits
it. A network whose attempt failed sits out the next scan and then its
own 2 to 60 second backoff while another saved network in range is
tried, and is taken anyway when nothing else fits. Each scan also probes
the first three saved names, fewer if the driver takes fewer in one scan,
so a hidden network among them answers; anyone in radio range hears those
names. A deployment from before the list reads a file of two or more
networks as none, so rolling back to one leaves Wi-Fi to be joined again.
It is its own
supplicant over nl80211: the four-way and group-key handshakes run in
td-netd, and EAPOL travels over nl80211's control port, so there is no
wpa_supplicant and no raw socket. Ethernet keeps precedence: the radio
starts an association only while no wired lease is held, and a cable
gaining carrier takes the connection from a Wi-Fi lease. A cable whose
DHCP fails gives way to Wi-Fi until it is unplugged. An authorized
association is a link with carrier for the same DHCP, renewal and expiry;
a failed or rejected lease is retried without leaving the network. A lost
association clears the address and rejoins after 2 to 60 seconds. An
access point that drops the station after the handshake's second message
is reported as a likely wrong passphrase.

WPA3-only, enterprise (802.1X), WPA1, non-CCMP (including mixed WPA/WPA2
networks whose group cipher is TKIP) and management-frame-
protection-required networks are refused with the reason in
`td-netd status`. It roams only by reassociating after a loss, to the
strongest access point of the network it then chooses. The passphrase is
never stored; a stored PMK joins its network for whoever reads it, which
only root can.

For SSH, while no hardware login key is enrolled, put the other computer's
Ed25519 **public** key in `~/.ssh/authorized_keys` on td:

```text
mkdir -p ~/.ssh
chmod 700 ~/.ssh
# Append the public key as one line to ~/.ssh/authorized_keys.
chmod 600 ~/.ssh/authorized_keys
td-netd status
whoami
```

Then run `ssh ACCOUNT@ADDRESS` from the other computer using the displayed
account and address. The SSH server already starts at boot. Login-key
enrollment disables this remote path and revokes existing SSH sessions;
`td-login/TOKEN-LOGIN.md` owns that policy. Root and password login are disabled.

An installed machine can receive this through its source checkout without
reinstalling: pull the published changes in `~/src/td`, run `./update`, approve
the deployment on the physical secure-attention screen, and reboot.
`td-update/DESIGN.md` specifies the build and activation flow. For an older
image whose networking runs only at boot, connect Ethernet before rebooting
to fetch the update. Updates build locally and need network access for missing
pinned sources and toolchain inputs.


## Choosing a system at startup

The installed selector shows current and previous versions for 15 seconds.
Press any ordinary key to stop the countdown, use Up/Down or 1/2 to choose,
and press Enter to boot. Entries show commit date, short hash and build
date in UTC. Older versions without this metadata say unknown and show a
short deployment ID. Selecting a version makes it the default current
system; the other verified version remains available as previous.
Without input, the existing automatic rollback after failed boots remains.

Adding this menu to an older installation requires refreshing its firmware
selector. The USB installer's Reinstall option does this while preserving
home and settings. A normal `./update` updates the deployment, not that
older selector. The live USB installer itself has no version menu: it boots
the single system carried by the medium.
