#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, mailbox, run_setup};
use super::*;
use crate::{
    format::{
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, Row},
        Sequence,
    },
    ids::{AccountId, BlobId, ThreadId},
    ports::Crypto,
    store_fs::{BlobSource, CommitRequest, IndexStore},
    wire,
};
use td_crypto::Digest;
const EMAIL: EmailId = EmailId::from_bytes([3; 16]);
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 2048];
    let n = row.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn id<T: wire::LocalId>(id: T) -> String {
    wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn seed(store: &IndexStore<'_>, source: &[u8]) {
    seed_at(store, source, 0);
}
fn seed_at(store: &IndexStore<'_>, source: &[u8], index: u8) {
    let email_id = EmailId::from_bytes([3 + index; 16]);
    let blob_id = BlobId::from_bytes([4 + index; 16]);
    let thread = ThreadId::from_bytes([5 + index; 16]);
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(source).unwrap();
    let blob = encoded(Row::Blob(BlobRow {
        length: source.len() as u64,
        digest: hash.finish().unwrap(),
        created_at: 0,
    }));
    let email = encoded(Row::Email(EmailRow {
        blob: blob_id,
        thread,
        received_at: 0,
        origin: EmailOrigin::Jmap,
    }));
    let mut key = [0; 32];
    Key::Membership(email_id, mailbox(1))
        .encode(&mut key)
        .unwrap();
    let operations = [
        Operation::put(Table::Blobs, blob_id.as_bytes(), &blob).unwrap(),
        Operation::put(Table::Threads, thread.as_bytes(), &[]).unwrap(),
        Operation::put(Table::Emails, email_id.as_bytes(), &email).unwrap(),
        Operation::put(Table::Memberships, &key, &[]).unwrap(),
    ];
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: AccountId::from_bytes([1; 16]),
                epoch: store.epoch(),
                expected: Sequence::from_u64(1 + u64::from(index)),
                utc_ms: 0,
                deadline: deadline(),
            },
            &operations,
            &mut [BlobSource {
                id: blob_id,
                source: &mut source.to_owned().as_slice(),
            }],
        )
        .unwrap();
}
fn execute(source: &[u8], extra: &str) -> td_json::Json {
    run_setup(
        &batch(&format!(
            r#"[["Email/get",{{"accountId":"{}","ids":["{}"],"fetchTextBodyValues":true,"fetchHTMLBodyValues":true{extra}}},"g"],["Core/echo",{{"after":true}},"e"]]"#,
            account(),
            id(EMAIL)
        )),
        1,
        256,
        90,
        false,
        None,
        |store| seed(store, source),
    )
}
#[test]
fn td_mail_plain_message_get_returns_headers_body_and_stored_fields() {
    let response = execute(b"From: Alice <alice@example.test>\r\nTo: Bob <bob@example.test>\r\nSubject: hello\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\nhello world", "");
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Email/get"),
        "{response:?}"
    );
    let email = &response["methodResponses"][0][1]["list"][0];
    assert_eq!(email["subject"].as_str(), Some("hello"));
    assert_eq!(
        email["from"][0]["email"].as_str(),
        Some("alice@example.test")
    );
    assert_eq!(email["preview"].as_str(), Some("hello world"));
    assert_eq!(email["textBody"][0]["partId"].as_str(), Some("1"));
    assert_eq!(
        email["bodyValues"]["1"]["value"].as_str(),
        Some("hello world")
    );
    assert_eq!(email["receivedAt"].as_str(), Some("1970-01-01T00:00:00Z"));
    assert_eq!(
        response["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
}
#[test]
fn td_mail_html_and_attachment_projection_preserves_body_and_locator() {
    let response = execute(b"Subject: html\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<b>hello</b> &amp; world\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=test.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--x--\r\n", "");
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Email/get"),
        "{response:?}"
    );
    let email = &response["methodResponses"][0][1]["list"][0];
    assert_eq!(email["preview"].as_str(), Some("hello & world"));
    assert_eq!(
        email["bodyValues"]["2"]["value"].as_str(),
        Some("<b>hello</b> &amp; world")
    );
    let attachment = &email["attachments"][0];
    assert_eq!(attachment["name"].as_str(), Some("test.bin"));
    assert_eq!(attachment["size"].as_u64(), Some(3));
    assert!(wire::PartLocator::decode(attachment["blobId"].as_str().unwrap()).is_ok());
}

#[test]
fn stored_only_and_capped_body_values_use_requested_projection() {
    let response = execute(
        b"not a valid header\r\n\r\nopaque",
        r#", "properties":["id","blobId"]"#,
    );
    let email = &response["methodResponses"][0][1]["list"][0];
    assert_eq!(email.as_object().unwrap().len(), 2);
    assert_eq!(email["id"].as_str(), Some(id(EMAIL).as_str()));
    assert_eq!(
        email["blobId"].as_str(),
        Some(id(BlobId::from_bytes([4; 16])).as_str())
    );
    let response = execute(
        b"Content-Type: text/plain\r\n\r\nhello world",
        r#", "properties":["bodyValues"],"maxBodyValueBytes":4"#,
    );
    let value = &response["methodResponses"][0][1]["list"][0]["bodyValues"]["1"];
    assert_eq!(value["value"].as_str(), Some("hell"));
    assert_eq!(value["isTruncated"].as_bool(), Some(true));
}
#[test]
fn header_property_keys_are_json_escaped() {
    let response = execute(
        b"X-\"Y: hello\r\n\r\nbody",
        r#", "properties":["header:X-\"Y:asText"]"#,
    );
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Email/get"),
        "{response:?}"
    );
    assert_eq!(
        response["methodResponses"][0][1]["list"][0]["header:X-\"Y:asText"].as_str(),
        Some("hello")
    );
}
#[test]
fn references_not_found_and_invalid_properties_preserve_method_order() {
    let response = run_setup(
        &batch(&format!(
            r##"[["Core/echo",{{"ids":["{}","foreign","foreign"]}},"a"],["Email/get",{{"accountId":"{}","#ids":{{"resultOf":"a","name":"Core/echo","path":"/ids"}},"properties":["id"]}},"g"],["Core/echo",{{"#ids":{{"resultOf":"g","name":"Email/get","path":"/list/*/id"}}}},"r"]]"##,
            id(EMAIL),
            account()
        )),
        1,
        256,
        90,
        false,
        None,
        |store| seed(store, b"\r\nbody"),
    );
    let get = &response["methodResponses"][1][1];
    assert_eq!(get["list"].as_array().unwrap().len(), 1);
    assert_eq!(get["notFound"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["methodResponses"][2][1]["ids"][0].as_str(),
        Some(id(EMAIL).as_str())
    );
    let response = execute(b"\r\nbody", r#", "properties":["bodyStructure"]"#);
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("invalidArguments")
    );
    assert_eq!(
        response["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
}
#[test]
fn projection_failure_rolls_back_the_list_and_leaves_later_methods_usable() {
    let source = format!(
        "Content-Type: text/plain\r\nContent-Disposition: attachment; filename=\"{}\"\r\n\r\nbody",
        "a".repeat(20 * 1024)
    );
    let response = execute(source.as_bytes(), "");
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("error"),
        "{response:?}"
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("serverFail")
    );
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("messageInterpretation")
    );
    assert!(response["methodResponses"][0][1].get("list").is_none());
    assert_eq!(
        response["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
}

#[test]
fn body_output_crosses_staging_boundaries_without_loss() {
    let text = "a".repeat(7000);
    let source = format!("Content-Type: text/plain; charset=UTF-8\r\n\r\n{text}");
    let response = execute(source.as_bytes(), "");
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Email/get"),
        "{response:?}"
    );
    let email = &response["methodResponses"][0][1]["list"][0];
    assert_eq!(
        email["bodyValues"]["1"]["value"].as_str(),
        Some(text.as_str())
    );
    assert_eq!(email["preview"].as_str().unwrap().chars().count(), 256);
}

#[test]
fn property_work_refusal_is_not_a_client_argument_error() {
    use crate::{
        admission::work::Charge,
        ports::{Clock, Tick, Time},
    };
    struct FixedClock;
    impl Clock for FixedClock {
        fn sample(&self) -> Result<Time, crate::ports::Error> {
            Ok(Time {
                monotonic: Tick(1),
                utc_ms: 0,
            })
        }
    }
    let mut name = Name::new();
    name.push(b"subject").unwrap();
    let mut work = Meter::new(deadline(), Charge::default());
    assert_eq!(
        Selection::new().include_name(name.clone(), &mut work, &FixedClock),
        Err(Error::StorageCapacity)
    );
    let mut expired = Meter::new(
        crate::ports::Deadline::after(Tick(0), 1).unwrap(),
        Charge::default(),
    );
    assert_eq!(
        Selection::new().include_name(name, &mut expired, &FixedClock),
        Err(Error::ExecutionDeadline)
    );
}

#[test]
fn td_mail_batch_body_output_preserves_large_and_small_messages() {
    use super::super::mailbox_get::tests::run_configured_response;
    let large = "a".repeat(600_000);
    let first = format!("Content-Type: text/plain; charset=UTF-8\r\n\r\n{large}");
    let response = run_configured_response(
        &batch(&format!(
            r#"[["Email/get",{{"accountId":"{}","ids":["{}","{}"],"properties":["id","textBody","bodyValues"],"fetchTextBodyValues":true}},"g"],["Core/echo",{{"after":true}},"e"]]"#,
            account(),
            id(EMAIL),
            id(EmailId::from_bytes([4; 16]))
        )),
        1,
        (crate::limits::Limits::default(), 2 * 1024 * 1024),
        90,
        false,
        None,
        |store| {
            seed_at(store, first.as_bytes(), 0);
            seed_at(store, b"Content-Type: text/plain\r\n\r\nsmall", 1);
        },
    );
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Email/get"),
        "{response:?}"
    );
    let list = &response["methodResponses"][0][1]["list"];
    assert_eq!(
        list[0]["bodyValues"]["1"]["value"].as_str(),
        Some(large.as_str())
    );
    assert_eq!(list[1]["bodyValues"]["1"]["value"].as_str(), Some("small"));
    assert_eq!(
        response["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
}

#[test]
fn all_ids_enumeration_is_complete_or_refuses_its_object_cap() {
    for ids in ["", ",\"ids\":null"] {
        for maximum in [1, 2] {
            let response = run_setup(
                &batch(&format!(
                    r#"[["Email/get",{{"accountId":"{}","properties":["id"]{ids}}},"g"]]"#,
                    account()
                )),
                1,
                maximum,
                90,
                false,
                None,
                |store| {
                    seed_at(store, b"\r\none", 0);
                    seed_at(store, b"\r\ntwo", 1);
                },
            );
            if maximum == 1 {
                assert_eq!(
                    response["methodResponses"][0][1]["type"].as_str(),
                    Some("requestTooLarge"),
                    "{response}"
                );
            } else {
                assert_eq!(
                    response["methodResponses"][0][1]["list"]
                        .as_array()
                        .unwrap()
                        .len(),
                    2
                );
            }
        }
    }
}

#[test]
fn body_work_exhaustion_is_not_an_interpretation_refusal() {
    let source = format!("Content-Type: text/plain\r\n\r\n{}", "a".repeat(2_100_000));
    let response = execute(source.as_bytes(), r#", "properties":["id","preview"]"#);
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("serverFail"),
        "{response}"
    );
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("storageWorkLimit"),
        "{response}"
    );
    assert_eq!(
        response["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
}

#[test]
fn shared_verified_projection_preserves_work_refusals_through_mime_phases() {
    use crate::{
        admission::work::{Charge, Stop},
        ports::{Deadline, Tick, Time},
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<Time, crate::ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    struct Progress(AtomicU64);
    impl Clock for Progress {
        fn sample(&self) -> Result<Time, crate::ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(self.0.fetch_add(1, Ordering::Relaxed)),
            })
        }
    }
    struct Sink;
    impl body::Output for Sink {
        fn check(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn append(&mut self, _: &[u8]) -> Result<(), Error> {
            Ok(())
        }
    }
    let limits = Limits {
        header_bytes: 1024,
        mime_parts: 4,
        mime_depth: 4,
        ..Limits::default()
    };
    let mut selection = Selection::new();
    selection.text = true;
    crate::store_fs::with_pinned_fixture(
        b"Content-Type: text/plain\r\n\r\nhello",
        &Fixed,
        |mut pin| {
            for by_deadline in [false, true] {
                let mut refused = false;
                let mut completed = false;
                for allowance in (1..128).chain([4096]) {
                    let mut work = Meter::new(
                        Deadline::after(Tick(0), if by_deadline { allowance } else { 4096 })
                            .unwrap(),
                        Charge {
                            records: if by_deadline { 65536 } else { allowance },
                            io_bytes: 65536,
                            output_bytes: 65536,
                            unlinks: 0,
                        },
                    );
                    let clock = Progress(AtomicU64::new(1));
                    let result = body::write_verified(
                        &mut Sink,
                        &mut pin,
                        &limits,
                        (&selection, Options::default()),
                        &clock,
                        &mut work,
                    );
                    match work.stopped() {
                        Some(Stop::Deadline) => {
                            refused = true;
                            assert_eq!(
                                result,
                                Err(Error::ExecutionDeadline),
                                "deadline allowance {allowance}"
                            );
                        }
                        Some(_) => {
                            refused = true;
                            assert_eq!(
                                result,
                                Err(Error::StorageCapacity),
                                "record allowance {allowance}"
                            );
                        }
                        None => {
                            assert_eq!(result, Ok(()));
                            completed = true;
                        }
                    }
                }
                assert!(refused && completed);
            }
        },
    );
}
