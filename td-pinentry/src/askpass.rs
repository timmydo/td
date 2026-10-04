//! The askpass convention ssh and git share: the prompt is the one
//! argument, the answer is written to standard output with a newline and
//! the program exits 0; a refusal writes nothing and exits 1. ssh says in
//! `SSH_ASKPASS_PROMPT` when it wants only a yes (`confirm`) or only to
//! show a message it will end itself (`none`).

use std::io::{self, Write};

use crate::request::{wipe, Answer, Kind, Request, DEFAULT_REPEAT_ERROR};

/// The prompt as a request. A user name or a host key's yes or no is no
/// secret, so its field shows what is typed; anything else is masked.
pub fn request(prompt: &str, hint: Option<&str>) -> Request {
    let kind = match hint {
        Some("confirm") => Kind::Confirm { one_button: false },
        Some("none") => Kind::Confirm { one_button: true },
        _ => Kind::Text {
            masked: !visible(prompt),
            repeat: None,
            repeat_error: DEFAULT_REPEAT_ERROR.to_owned(),
        },
    };
    let mut request = Request::new(kind);
    request.description = prompt.trim_end().to_owned();
    request.prompt = String::new();
    if hint == Some("none") {
        request.ok = "Dismiss".to_owned();
    }
    request
}

/// Whether a prompt asks for something that is not a secret, by the whole
/// form of the two that do not: git's `Username for '<url>': ` and ssh's
/// unknown host key question. A prompt merely containing their words, a
/// key file's name for one, stays masked, as does a translated one.
pub fn visible(prompt: &str) -> bool {
    let prompt = prompt.trim_end();
    (prompt.starts_with("Username for '") && prompt.ends_with("':"))
        || (prompt.starts_with("The authenticity of host ")
            && prompt.ends_with("(yes/no/[fingerprint])?"))
}

/// Writes the answer as the convention asks; true when it was given, so
/// the program exits 0. A window that could not open is the error.
pub fn finish(answer: Answer, out: &mut dyn Write) -> Result<bool, String> {
    match answer {
        Answer::Text(secret) => {
            let text = secret.as_str().as_bytes();
            let mut line = Vec::new();
            line.try_reserve_exact(text.len() + 1)
                .map_err(|_| "cannot reserve the answer".to_owned())?;
            line.extend_from_slice(text);
            line.push(b'\n');
            let written = out.write_all(&line).and_then(|()| out.flush());
            wipe(line);
            written.map_err(|error: io::Error| format!("cannot write the answer: {error}"))?;
            Ok(true)
        }
        Answer::Confirmed => Ok(true),
        Answer::Cancelled | Answer::Declined | Answer::TimedOut | Answer::Hangup => Ok(false),
        Answer::Failed(why) => Err(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Secret;

    #[test]
    fn a_secret_is_masked_and_a_name_or_a_host_key_answer_is_not() {
        let masked = |prompt: &str| match request(prompt, None).kind {
            Kind::Text { masked, .. } => masked,
            Kind::Confirm { .. } => panic!("{prompt} is a text"),
        };
        assert!(masked(
            "Enter passphrase for key '/home/a/.ssh/id_ed25519': "
        ));
        assert!(masked("Password for 'https://a@github.com': "));
        assert!(!masked("Username for 'https://github.com': "));
        assert!(!masked(
            "The authenticity of host 'h (192.0.2.1)' can't be established.\n\
             ED25519 key fingerprint is SHA256:x.\n\
             Are you sure you want to continue connecting (yes/no/[fingerprint])? "
        ));
        // Their words inside another prompt do not unmask it.
        assert!(masked("Enter passphrase for key '/tmp/(yes/no-key': "));
        assert!(masked("Enter passphrase for key '/tmp/Username for 'x':"));
        assert!(masked("Benutzername für 'https://github.com': "));
    }

    #[test]
    fn the_prompt_is_the_description_with_no_label() {
        let request = request("Password for 'https://github.com': ", None);
        assert_eq!(request.description, "Password for 'https://github.com':");
        assert_eq!(request.prompt, "");
        assert_eq!(request.ok, "OK");
    }

    #[test]
    fn ssh_hints_ask_a_question_or_show_a_message() {
        assert_eq!(
            request("Allow use of key?", Some("confirm")).kind,
            Kind::Confirm { one_button: false }
        );
        let message = request("Confirm user presence for key", Some("none"));
        assert_eq!(message.kind, Kind::Confirm { one_button: true });
        assert_eq!(message.ok, "Dismiss");
        assert!(matches!(
            request("Passphrase:", Some("other")).kind,
            Kind::Text { masked: true, .. }
        ));
    }

    #[test]
    fn an_answer_is_one_line_and_a_refusal_is_nothing() {
        let mut out = Vec::new();
        let given = finish(Answer::Text(Secret::copy("pass word")), &mut out);
        assert_eq!(given, Ok(true));
        assert_eq!(out, b"pass word\n");
        for refused in [
            Answer::Cancelled,
            Answer::Declined,
            Answer::TimedOut,
            Answer::Hangup,
        ] {
            let mut out = Vec::new();
            assert_eq!(finish(refused, &mut out), Ok(false));
            assert!(out.is_empty());
        }
        let mut out = Vec::new();
        assert_eq!(finish(Answer::Confirmed, &mut out), Ok(true));
        assert!(out.is_empty());
        assert_eq!(
            finish(Answer::Failed("no display".to_owned()), &mut out),
            Err("no display".to_owned())
        );
    }
}
