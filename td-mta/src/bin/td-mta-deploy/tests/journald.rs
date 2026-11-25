#![allow(clippy::unwrap_used)]
use super::*;

const MAIN: &str = "# /etc/systemd/journald.conf
#  This file is part of systemd.
[Journal]
#Storage=auto
#SystemMaxUse=
#SystemKeepFree=
";

fn ours() -> String {
    format!("{MAIN}\n# {DROPIN}\n{TEXT}")
}

#[test]
fn the_drop_in_is_the_journal_bound_at_mode_0644() {
    let setting = setting().unwrap();
    assert_eq!(setting.path, PathBuf::from(DROPIN));
    assert_eq!(setting.mode, 0o644);
    assert!(DROPIN.starts_with("/etc/systemd/journald.conf.d/"));
    assert!(setting.text.contains("\n[Journal]\n"));
    for (key, value) in KEYS {
        assert!(setting.text.contains(&format!("\n{key}={value}\n")));
    }
    // The text is what journald itself would take for its last word.
    effective(&ours()).unwrap();
    assert!(FOREIGN.starts_with(&format!("{DROPIN} is not")));
}

#[test]
fn only_the_drop_in_as_last_word_satisfies_journald() {
    // Not read at all: the distribution's commented defaults set nothing.
    let error = effective(MAIN).unwrap_err().to_string();
    assert!(error.contains("does not read"), "{error}");
    // A file read later undoes it, and is named.
    let later = format!(
        "{}\n# /etc/systemd/journald.conf.d/zz-local.conf\n[Journal]\nSystemMaxUse = 2G\n",
        ours()
    );
    let error = effective(&later).unwrap_err().to_string();
    assert!(
        error.starts_with(
            "/etc/systemd/journald.conf.d/zz-local.conf sets journald's SystemMaxUse="
        ),
        "{error}"
    );
    // An empty assignment resets it to the default.
    let reset = format!(
        "{}\n# /run/systemd/journald.conf.d/x.conf\n[Journal]\nSystemKeepFree=\n",
        ours()
    );
    assert!(effective(&reset).is_err());
    // A setting read earlier, one in another section, a comment and a
    // later file with no section do not.
    let earlier = format!(
        "# /etc/systemd/journald.conf\n[Journal]\nSystemMaxUse=4G\n\n# {DROPIN}\n{TEXT}\n# /etc/systemd/journald.conf.d/zz.conf\nSystemMaxUse=1G\n[Other]\nSystemKeepFree=1G\n[Journal]\n#SystemMaxUse=1G\n;SystemKeepFree=1G\n"
    );
    effective(&earlier).unwrap();
    // A later file that sets the same values does not undo them.
    let same = format!("{}\n# /etc/systemd/journald.conf.d/zz.conf\n{TEXT}", ours());
    effective(&same).unwrap();
    // A comment that starts like a header is not one: the assignment
    // after it stays in its file's section and is seen.
    let comment = format!(
        "{}\n# /etc/systemd/journald.conf.d/zz.conf\n[Journal]\n# /etc/ if the original file is shipped in /usr/), or\nSystemMaxUse=2G\n",
        ours()
    );
    let error = effective(&comment).unwrap_err().to_string();
    assert!(
        error.starts_with("/etc/systemd/journald.conf.d/zz.conf sets"),
        "{error}"
    );
}

#[test]
fn journald_start_time_is_read_in_unix_seconds() {
    assert_eq!(unix_time("@1732400000\n").unwrap(), 1_732_400_000);
    for text in [
        "",
        "@",
        "1732400000",
        "@17324x",
        "Mon 2026-11-23 10:00:00 UTC",
    ] {
        assert!(unix_time(text).is_err(), "{text}");
    }
    assert_eq!(Cap::Installed.name(), "installed");
    assert_eq!(Cap::Restarted.name(), "restarted");
    assert_eq!(Cap::Unchanged.name(), "unchanged");
}
