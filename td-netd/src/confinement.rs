//! The source-level contract UNSAFE.md §2 states and the compiler cannot:
//! one raw entry point in `sys.rs`, these nine call sites with their pinned
//! numbers and arguments, and the interface-config ioctl requests.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use std::path::Path;
use td_source_scan::{self as scan, squeeze, unsafe_allows, unsafe_blocks, unsafe_items};

/// The one module reached by `#[path]`: td-fido's AES, shared source.
const FIDO_PATH: &str = concat!("#[pa", "th=\"../../td-fido/src/fido_aes.rs\"]");

fn walk() -> scan::Tree {
    let base = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    scan::read_tree(base).unwrap()
}

fn sources() -> Vec<(String, String)> {
    walk().rs
}

fn source(name: &str) -> String {
    scan::source(&sources(), name)
}

fn squeezed() -> String {
    scan::squeezed(&sources())
}

/// The shared AES file, read as the crate compiles it.
fn fido_aes() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../td-fido/src/fido_aes.rs");
    scan::strip_comments(&std::fs::read_to_string(path).unwrap())
}

#[test]
fn src_holds_exactly_the_scanned_modules() {
    let scan::Tree { rs, other } = walk();
    let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "confinement.rs",
            "genl.rs",
            "main.rs",
            "sha1.rs",
            "sys.rs",
            "wifi.rs",
            "wpa.rs",
            "wpa/tests.rs",
        ],
        "the crate's file set changed"
    );
    assert!(other.is_empty(), "src/ holds a non-.rs file: {other:?}");
    let declared = scan::declared_modules(&rs);
    for (path, _) in &rs {
        assert!(
            path == "main.rs" || declared.iter().any(|d| d == path),
            "'{path}' was scanned but no `mod` line declares it"
        );
    }
    // fido_aes is declared at the root but lives in td-fido; it is read below.
    for target in &declared {
        assert!(
            target == "fido_aes.rs" || rs.iter().any(|(f, _)| f == target),
            "module '{target}' is declared but was not scanned"
        );
    }
}

#[test]
fn exactly_one_scoped_allow_and_one_unsafe_block() {
    let files = sources();
    let allows: usize = files.iter().map(|(_, t)| unsafe_allows(t)).sum();
    let blocks: usize = files.iter().map(|(_, t)| unsafe_blocks(t)).sum();
    assert_eq!(allows, 1, "exactly one scoped unsafe allow (sys.rs)");
    assert_eq!(blocks, 1, "exactly one unsafe block (sys.rs)");
    assert_eq!(unsafe_blocks(&source("sys.rs")), 1);
    for form in ["fn", "impl", "trait"] {
        let items: usize = files.iter().map(|(_, t)| unsafe_items(t, form)).sum();
        assert_eq!(items, 0, "an item of this form exists: {form}");
    }
    // The shared file is compiled into this crate, so it is held to the
    // same budget: none, under its own forbid.
    let fido = fido_aes();
    assert_eq!(unsafe_blocks(&fido) + unsafe_allows(&fido), 0);
    for form in ["fn", "impl", "trait"] {
        assert_eq!(unsafe_items(&fido, form), 0);
    }
    // One `#[path]`, to td-fido's AES, and no other decoupling construct.
    let squeezed = squeezed();
    assert_eq!(squeezed.matches(FIDO_PATH).count(), 1);
    let rest = squeezed.replacen(FIDO_PATH, "", 1);
    for construct in scan::DECOUPLING {
        assert_eq!(
            rest.matches(construct).count(),
            0,
            "`{construct}` would decouple the compiled crate from the scanned source"
        );
    }
    let fido = squeeze(&fido);
    for construct in scan::DECOUPLING {
        assert_eq!(
            fido.matches(construct).count(),
            0,
            "fido_aes.rs: {construct}"
        );
    }
}

#[test]
fn the_unsafe_lint_is_named_exactly_twice() {
    let squeezed = squeezed();
    let lint = concat!("unsafe_", "code");
    assert_eq!(squeezed.matches(lint).count(), 2);
    assert_eq!(
        squeezed.matches(&format!("{}![deny({lint})]", "#")).count(),
        1,
        "the crate root denies the unsafe lint"
    );
}

const CALL: &str = concat!("sys", "call6");

/// The x86_64 numbers UNSAFE.md §2 records. The number is pinned, not just
/// the name: a renumbered constant changes the kernel call while every
/// name-based assertion still passes.
const AMENDED: &[(&str, &str)] = &[
    ("SYS_CLOSE", "3"),
    ("SYS_POLL", "7"),
    ("SYS_IOCTL", "16"),
    ("SYS_SOCKET", "41"),
    ("SYS_SENDTO", "44"),
    ("SYS_RECVFROM", "45"),
    ("SYS_SETSOCKOPT", "54"),
];

/// Every value a call site passes by name, pinned too.
const VALUES: &[(&str, &str)] = &[
    ("TCGETS", "0x5401"),
    ("TCSETS", "0x5402"),
    ("TERMIOS_LEN", "36"),
    ("AF_NETLINK", "16"),
    ("SOCK_RAW_CLOEXEC", "3|0o2000000"),
    ("NETLINK_GENERIC", "16"),
    ("SOL_NETLINK", "270"),
    ("NETLINK_ADD_MEMBERSHIP", "1"),
    ("RECV_FLAGS", "0x40|0x20"),
    ("SOCKADDR_NL_LEN", "12"),
];

#[test]
fn the_syscall_roster_is_pinned_by_number() {
    let sys = squeeze(&source("sys.rs"));
    for (name, number) in AMENDED {
        assert_eq!(
            sys.matches(&format!("const{name}:usize={number};")).count(),
            1,
            "{name} must be {number}"
        );
    }
    assert_eq!(
        squeezed().matches(concat!("const", "SYS_")).count(),
        AMENDED.len(),
        "a syscall outside the roster is an UNSAFE.md amendment"
    );
    for (name, value) in VALUES {
        assert_eq!(
            sys.matches(&format!("const{name}:")).count(),
            1,
            "{name} declared once"
        );
        let declared = sys
            .split(&format!("const{name}:"))
            .nth(1)
            .and_then(|rest| rest.split_once('='))
            .and_then(|(_, rest)| rest.split_once(';'))
            .map(|(value, _)| value.to_string());
        assert_eq!(declared.as_deref(), Some(*value), "{name}");
    }
}

/// The asm body, pinned whole: a register swapped compiles and passes every
/// other assertion.
#[test]
fn the_confined_block_is_pinned_whole() {
    const BLOCK: &str = concat!(
        "un",
        "safe{core",
        "::arch::a",
        "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
        "in(\"rdi\")a1,in(\"rsi\")a2,in(\"rdx\")a3,",
        "in(\"r10\")a4,in(\"r8\")a5,in(\"r9\")a6,",
        "out(\"rcx\")_,out(\"r11\")_,options(nostack),);}"
    );
    let squeezed = squeezed();
    assert_eq!(squeezed.matches(BLOCK).count(), 1, "re-audit the asm body");
    assert_eq!(squeezed.matches(concat!("a", "sm!")).count(), 1);
}

/// Each call site, pinned whole: the roster pins which calls, this pins
/// what is handed to them.
#[test]
fn every_call_site_is_pinned_whole() {
    const ARGUMENTS: &[&str] = &[
        "(SYS_IOCTL,fdasusize,request,argpasusize,0,0,0,)",
        "(SYS_IOCTL,0,TCGETS,termios.as_mut_ptr()asusize,0,0,0,)",
        "(SYS_IOCTL,0,TCSETS,termios.as_ptr()asusize,0,0,0,)",
        "(SYS_SOCKET,AF_NETLINK,SOCK_RAW_CLOEXEC,NETLINK_GENERIC,0,0,0,)",
        "(SYS_SETSOCKOPT,self.fdasusize,SOL_NETLINK,NETLINK_ADD_MEMBERSHIP,value.as_ptr()asusize,value.len(),0,)",
        "(SYS_SENDTO,self.fdasusize,message.as_ptr()asusize,message.len(),0,0,0,)",
        "(SYS_RECVFROM,self.fdasusize,buf.as_mut_ptr()asusize,buf.len(),RECV_FLAGS,address.as_mut_ptr()asusize,address_len.as_mut_ptr()asusize,)",
        "(SYS_CLOSE,self.fdasusize,0,0,0,0,0)",
        "(SYS_POLL,fds.as_mut_ptr()asusize,2,millis,0,0,0,)",
    ];
    let sys = squeeze(&source("sys.rs"));
    for arguments in ARGUMENTS {
        assert_eq!(
            sys.matches(&format!("{CALL}{arguments}")).count(),
            1,
            "the call site is not the pinned one: {arguments}"
        );
    }
    assert_eq!(
        squeezed().matches(&format!("{CALL}(")).count(),
        ARGUMENTS.len() + 1,
        "the raw entry point is defined once and called only at the pinned sites"
    );
    // The buffers those calls hand the kernel are the sizes their length
    // arguments say.
    for buffer in [
        "letmuttermios=[0u8;TERMIOS_LEN];",
        "letmutaddress=[0u8;SOCKADDR_NL_LEN];",
        "letmutfds=[0u8;16];",
        "letvalue=group.to_ne_bytes();",
    ] {
        assert_eq!(sys.matches(buffer).count(), 1, "{buffer}");
    }
}

/// The receive checks UNSAFE.md states, and the one owner of the socket:
/// a derived `Clone` or `Copy` would close the descriptor twice.
#[test]
fn the_netlink_socket_is_owned_once_and_hears_only_the_kernel() {
    let sys = squeeze(&source("sys.rs"));
    for check in [
        "ifgot>buf.len(){",
        "ifu32::from_ne_bytes(address_len)asusize!=SOCKADDR_NL_LEN||family!=Some(AF_NETLINKasu16)||port!=Some(0){returnOk(None);}",
        "implDropforGenlSocket{fndrop(&mutself){let_=",
    ] {
        assert_eq!(sys.matches(check).count(), 1, "{check}");
    }
    assert_eq!(sys.matches("derive").count(), 0, "sys.rs derives nothing");
    assert_eq!(sys.matches("pubstructGenlSocket{fd:RawFd,}").count(), 1);
    // The borrowed descriptor goes only to poll2, directly or through the
    // one `nl_fd` binding the watcher's wait polls.
    let lent = [
        "sys::poll2(Some(self.socket.fd()),None,left)",
        "sys::poll2(Some(nl.socket.fd()),None,left)",
        "letnl_fd=self.nl.as_ref().map(|nl|nl.socket.fd());",
    ];
    for (name, text) in sources() {
        if name == "sys.rs" || name == "confinement.rs" {
            continue;
        }
        let text = squeeze(&text);
        assert!(
            !text.contains("GenlSocket::fd"),
            "{name} names GenlSocket::fd"
        );
        let allowed: usize = lent.iter().map(|form| text.matches(form).count()).sum();
        assert_eq!(
            text.matches(".fd()").count(),
            allowed,
            "{name}: fd() outside poll2"
        );
        assert_eq!(
            text.matches("nl_fd").count(),
            2 * text.matches(lent[2]).count(),
            "{name}: nl_fd goes beyond poll2"
        );
        if text.contains(lent[2]) {
            assert!(text.contains("sys::poll2(nl_fd,control_fd,timeout)"));
        }
    }
}

#[test]
fn the_raw_entry_point_is_private_to_its_module() {
    let sys = squeeze(&source("sys.rs"));
    let annotated = format!("{}{CALL}(", concat!("#[allow(un", "safe_code)]fn"));
    assert_eq!(sys.matches(&annotated).count(), 1);
    assert_eq!(sys.matches(&format!("fn{CALL}(")).count(), 1);
    assert_eq!(sys.matches(&format!("pubfn{CALL}(")).count(), 0);
    for (name, text) in sources() {
        if name != "sys.rs" && name != "confinement.rs" {
            assert!(!squeeze(&text).contains(CALL), "{name} names {CALL}");
        }
    }
}

/// `sys::ioctl` takes its request from the caller, so its callers are the
/// pin: the interface-config requests on an AF_INET socket and nothing else.
#[test]
fn the_ioctl_requests_are_the_interface_config_set() {
    const REQUESTS: &[(&str, &str)] = &[
        ("SIOCGIFFLAGS", "0x8913"),
        ("SIOCSIFFLAGS", "0x8914"),
        ("SIOCSIFADDR", "0x8916"),
        ("SIOCSIFNETMASK", "0x891c"),
        ("SIOCGIFHWADDR", "0x8927"),
        ("SIOCADDRT", "0x890b"),
        ("SIOCDELRT", "0x890c"),
    ];
    let main = squeeze(&source("main.rs"));
    for (name, value) in REQUESTS {
        assert_eq!(
            main.matches(&format!("const{name}:usize={value};")).count(),
            1,
            "{name}"
        );
    }
    let mut calls = 0;
    for (name, text) in sources() {
        if name == "sys.rs" || name == "confinement.rs" {
            continue;
        }
        let text = squeeze(&text);
        for (offset, _) in text.match_indices("ioctl(") {
            // Another name ending in `ioctl` is not a call to it; a keyword
            // squeezed against the call (`matchioctl(`) still is.
            let before = &text[..offset];
            let word: String = before
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let keyword = ["match", "return", "if", "while", "in", "else", "break"]
                .iter()
                .any(|k| word.chars().eq(k.chars().rev()));
            if (!word.is_empty() && !keyword) || before.ends_with('.') {
                continue;
            }
            let request = text[offset..]
                .split(',')
                .nth(1)
                .unwrap_or_default()
                .to_string();
            calls += 1;
            if request == "request" {
                // set_addr forwards its own argument; its callers are below.
                continue;
            }
            assert!(
                REQUESTS.iter().any(|(r, _)| *r == request),
                "{name}: ioctl request {request} is not in the roster"
            );
        }
        for (offset, _) in text.match_indices("set_addr(") {
            let args = &text[offset..];
            if args.starts_with("set_addr(fd:RawFd") {
                continue;
            }
            let request = args.split(',').nth(2).unwrap_or_default();
            let request = request.trim_start_matches("crate::");
            assert!(
                request == "SIOCSIFADDR" || request == "SIOCSIFNETMASK",
                "{name}: set_addr passes {request}"
            );
        }
    }
    assert_eq!(calls, 7, "the interface-config ioctl call sites changed");
    // SIOCDELRT deletes whatever route it describes; it describes only the
    // limited-broadcast host route, on no device in particular.
    assert_eq!(
        main.matches("SIOCDELRT").count(),
        2,
        "the const and one site"
    );
    assert_eq!(
        main.matches(
            "letmutrt=build_rtentry([255,255,255,255],[255,255,255,255],[0,0,0,0],RTF_UP|RTF_HOST,None,);matchioctl(fd,SIOCDELRT,rt.as_mut_ptr())"
        )
        .count(),
        1,
        "SIOCDELRT takes only the limited-broadcast host route"
    );
}
