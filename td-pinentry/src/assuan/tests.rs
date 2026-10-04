use super::*;
use crate::request::Secret;

/// The conversation `input` makes with `serve`, each prompt answered by
/// the next of `answers` and recorded; the output as text.
fn converse(input: &str, answers: Vec<Answer>) -> (String, Vec<Request>) {
    let receiver = spawn_reader(std::io::Cursor::new(input.as_bytes().to_vec())).unwrap();
    let mut inbox = Inbox::new(receiver);
    let mut out = Vec::new();
    let mut answers = answers.into_iter();
    let mut asked = Vec::new();
    serve(&mut inbox, &mut out, &mut |request, _| {
        asked.push(request);
        answers.next().expect("an answer for every prompt")
    })
    .unwrap();
    (String::from_utf8(out).unwrap(), asked)
}

fn secret(text: &str) -> Answer {
    Answer::Text(Secret::copy(text))
}

#[test]
fn a_passphrase_is_returned_escaped_after_the_greeting() {
    let (out, asked) = converse(
        "SETDESC Please enter%0A%22Some One <a@b>%22\n\
         SETPROMPT Passphrase:\n\
         SETTITLE Unlock\n\
         GETPIN\n\
         BYE\n",
        vec![secret("50%\rlike")],
    );
    assert_eq!(
        out,
        "OK Pleased to meet you\nOK\nOK\nOK\nD 50%25%0Dlike\nOK\nOK closing connection\n"
    );
    let request = &asked[0];
    assert_eq!(request.description, "Please enter\n\"Some One <a@b>\"");
    assert_eq!(request.prompt, "Passphrase:");
    assert_eq!(request.title, "Unlock");
    assert!(matches!(
        request.kind,
        Kind::Text {
            masked: true,
            repeat: None,
            ..
        }
    ));
}

#[test]
fn each_way_a_prompt_ends_has_its_answer() {
    let cases = [
        (Answer::Cancelled, "ERR 83886179 Operation cancelled\n"),
        (Answer::Declined, "ERR 83886194 Not confirmed\n"),
        (Answer::TimedOut, "ERR 83886142 Timeout\n"),
        (
            Answer::Failed("no display".to_owned()),
            "ERR 83886165 No pinentry\n",
        ),
        (secret(""), "OK\n"),
    ];
    for (answer, expected) in cases {
        let (out, _) = converse("GETPIN\n", vec![answer]);
        assert_eq!(out, format!("OK Pleased to meet you\n{expected}"));
    }
}

#[test]
fn a_caller_hanging_up_ends_the_conversation_unanswered() {
    let (out, _) = converse("GETPIN\nNOP\n", vec![Answer::Hangup]);
    assert_eq!(out, "OK Pleased to meet you\n");
}

/// The conversation `input` makes when each prompt waits, as the window
/// does, until the inbox says the caller has gone.
fn converse_until_hangup(input: &[u8]) -> (String, usize) {
    let receiver = spawn_reader(std::io::Cursor::new(input.to_vec())).unwrap();
    let mut inbox = Inbox::new(receiver);
    let mut out = Vec::new();
    let mut prompts = 0;
    serve(&mut inbox, &mut out, &mut |_, inbox| {
        prompts += 1;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !inbox.hung_up() {
            assert!(std::time::Instant::now() < deadline, "the end is seen");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Answer::Hangup
    })
    .unwrap();
    (String::from_utf8(out).unwrap(), prompts)
}

#[test]
fn the_inbox_sees_the_caller_end_behind_a_waiting_prompt() {
    let (out, prompts) = converse_until_hangup(b"GETPIN\n");
    assert_eq!((out.as_str(), prompts), ("OK Pleased to meet you\n", 1));
    // Lines sent while the prompt waits keep their place until the end,
    // and the prompt still ends unanswered.
    let (out, prompts) = converse_until_hangup(b"GETPIN\nNOP\nNOP\n");
    assert_eq!((out.as_str(), prompts), ("OK Pleased to meet you\n", 1));
    // More than the queue holds, with the end behind them, is gone too.
    let flood = format!("GETPIN\n{}", "NOP\n".repeat(QUEUE * 3));
    let (out, prompts) = converse_until_hangup(flood.as_bytes());
    assert_eq!((out.as_str(), prompts), ("OK Pleased to meet you\n", 1));
}

#[test]
fn queued_lines_keep_their_order() {
    let receiver = spawn_reader(std::io::Cursor::new(b"A\nB\r\nC\n".to_vec())).unwrap();
    let mut inbox = Inbox::new(receiver);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !inbox.hung_up() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // A CR before the LF is the line's end too.
    for line in [&b"A"[..], b"B", b"C"] {
        assert_eq!(inbox.next(), Event::Line(line.to_vec()));
    }
    assert_eq!(inbox.next(), Event::End);
}

#[test]
fn an_interrupted_read_is_read_again() {
    struct Interrupting(bool, std::io::Cursor<Vec<u8>>);
    impl std::io::Read for Interrupting {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.0) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.1.read(buffer)
        }
    }
    let mut input =
        std::io::BufReader::new(Interrupting(true, std::io::Cursor::new(b"NOP\n".to_vec())));
    assert_eq!(
        read_line(&mut input).unwrap(),
        Some(Event::Line(b"NOP".to_vec()))
    );
    assert_eq!(read_line(&mut input).unwrap(), None);
}

#[test]
fn confirm_message_and_the_third_button() {
    let (out, asked) = converse(
        "SETNOTOK _No\nSETOK _Yes\nCONFIRM\nCONFIRM --one-button Sure?\nMESSAGE\nGETPIN\n",
        vec![
            Answer::Declined,
            Answer::Confirmed,
            Answer::Confirmed,
            Answer::Cancelled,
        ],
    );
    assert_eq!(
        out,
        "OK Pleased to meet you\nOK\nOK\nERR 83886194 Not confirmed\nOK\nOK\n\
         ERR 83886179 Operation cancelled\n"
    );
    assert_eq!(asked[0].kind, Kind::Confirm { one_button: false });
    assert_eq!(asked[0].not_ok.as_deref(), Some("No"));
    assert_eq!(asked[0].ok, "Yes");
    // The not-OK button belongs to a question with Cancel beside it.
    for one in [&asked[1], &asked[2]] {
        assert_eq!(one.kind, Kind::Confirm { one_button: true });
        assert_eq!(one.not_ok, None);
    }
    assert_eq!(asked[3].not_ok, None);
}

#[test]
fn a_repeated_passphrase_is_reported_and_asked_once() {
    let (out, asked) = converse(
        "SETREPEAT Re_peat:\nSETREPEATERROR differ\nGETPIN\nGETPIN\nSETREPEAT\nGETPIN\n",
        vec![secret("p"), secret("q"), secret("r")],
    );
    assert_eq!(
        out,
        "OK Pleased to meet you\nOK\nOK\nS PIN_REPEATED\nD p\nOK\nD q\nOK\n\
         OK\nS PIN_REPEATED\nD r\nOK\n"
    );
    assert!(matches!(
        &asked[2].kind,
        Kind::Text { repeat: Some(label), .. } if label == "Repeat:"
    ));
    assert_eq!(
        asked[0].kind,
        Kind::Text {
            masked: true,
            repeat: Some("Repeat:".to_owned()),
            repeat_error: "differ".to_owned(),
        }
    );
    assert!(matches!(asked[1].kind, Kind::Text { repeat: None, .. }));
}

#[test]
fn an_error_shows_on_the_next_prompt_alone() {
    let (_, asked) = converse(
        "SETERROR Bad Passphrase (try 2 of 3)\nGETPIN\nGETPIN\n",
        vec![Answer::Cancelled, Answer::Cancelled],
    );
    assert_eq!(
        asked[0].error.as_deref(),
        Some("Bad Passphrase (try 2 of 3)")
    );
    assert_eq!(asked[1].error, None);
}

#[test]
fn defaults_options_reset_and_timeout() {
    let (out, asked) = converse(
        "OPTION ttyname=/dev/pts/3\n\
         OPTION allow-external-password-cache\n\
         OPTION default-ok=_Unlock\n\
         OPTION default-cancel=_Stop\n\
         SETDESC kept until reset\n\
         SETTIMEOUT 30\n\
         GETPIN\n\
         RESET\n\
         SETTIMEOUT x\n\
         GETPIN\n",
        vec![Answer::Cancelled, Answer::Cancelled],
    );
    assert!(out.contains("ERR 83886360 Invalid parameter\n"), "{out}");
    // An option this program does not implement is refused, so the agent
    // never counts on it; the terminal's are taken.
    assert!(
        out.starts_with("OK Pleased to meet you\nOK\nERR 83886254 Unknown option\nOK\n"),
        "{out}"
    );
    assert_eq!(asked[0].ok, "Unlock");
    assert_eq!(asked[0].cancel, "Stop");
    assert_eq!(asked[0].description, "kept until reset");
    assert_eq!(asked[0].timeout, Some(30));
    assert_eq!(asked[1].ok, "Unlock");
    assert_eq!(asked[1].description, "");
    // RESET keeps the timeout gpg-agent sets once, as pinentry does.
    assert_eq!(asked[1].timeout, Some(30));
}

#[test]
fn getinfo_comments_case_and_unknown_commands() {
    let (out, _) = converse(
        "# a comment\n\nnop\nGETINFO flavor\nGETINFO pid\nGETINFO nothing\nFROB\n\
         SETDESC\t  tabbed\nGETPIN\n",
        vec![Answer::Cancelled],
    );
    let pid = std::process::id();
    assert_eq!(
        out,
        format!(
            "OK Pleased to meet you\nOK\nD td\nOK\nD {pid}\nOK\n\
             ERR 83886360 Invalid parameter\nERR 83886355 Unknown IPC command\nOK\n\
             ERR 83886179 Operation cancelled\n"
        )
    );
    // A tab ends the command, and the blanks after it are not the text.
    let (_, asked) = converse("SETDESC\t  tabbed\nGETPIN\n", vec![Answer::Cancelled]);
    assert_eq!(asked[0].description, "tabbed");
}

#[test]
fn a_line_too_long_is_refused_and_the_next_one_read() {
    // One byte past libassuan's ceiling, its LF included.
    let long = "SETDESC ".to_owned() + &"x".repeat(MAX_READ - "SETDESC ".len());
    assert_eq!(long.len() + 1, MAX_READ + 1);
    let (out, _) = converse(&format!("{long}\nNOP\n"), vec![]);
    assert_eq!(
        out,
        "OK Pleased to meet you\nERR 83886343 Line too long\nOK\n"
    );
    // A line of exactly libassuan's ceiling, its CR and LF included, is
    // read: an agent may fill it.
    let fits = "SETDESC ".to_owned() + &"x".repeat(MAX_READ - "SETDESC \r\n".len());
    let (out, asked) = converse(&format!("{fits}\r\nGETPIN\n"), vec![Answer::Cancelled]);
    assert!(out.starts_with("OK Pleased to meet you\nOK\n"), "{out}");
    assert_eq!(asked[0].description.len(), MAX_READ - "SETDESC \r\n".len());
}

#[test]
fn a_long_passphrase_spans_data_lines_that_decode_back() {
    let text: String = "ab%\n".repeat(300).chars().filter(|c| *c != '\n').collect();
    let (out, _) = converse("GETPIN\n", vec![secret(&text)]);
    let mut decoded = Vec::new();
    for line in out.lines().filter(|line| line.starts_with("D ")) {
        assert!(line.len() < MAX_LINE, "{}", line.len());
        decoded.extend_from_slice(unescape(&line.as_bytes()[2..]).as_bytes());
    }
    assert!(out.lines().filter(|line| line.starts_with("D ")).count() > 1);
    assert_eq!(String::from_utf8(decoded).unwrap(), text);
}

#[test]
fn escapes_and_labels() {
    assert_eq!(unescape(b"a%0Ab%25c%zz%4"), "a\nb%c%zz%4");
    assert_eq!(unescape(b"%E2%9C%93"), "\u{2713}");
    assert_eq!(label("_OK"), "OK");
    assert_eq!(label("Save__as"), "Save_as");
    assert_eq!(label("end_"), "end");
}
