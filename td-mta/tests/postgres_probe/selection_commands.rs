use super::{command_output, Fixture};
use std::path::Path;
use td_mta::{
    ids::StoreEpoch,
    postgres_configuration_store::Version,
    postgres_selection::{Change, RETAINED_BYTES},
};

pub(super) fn prepare(
    fixture: &Fixture,
    connection: &Path,
    epoch: StoreEpoch,
    version: Version,
) -> Change {
    let output = command_output(
        &[
            "postgres",
            "config-selection-prepare",
            "--connection-file",
            connection.to_str().unwrap(),
            "--epoch",
            &epoch.to_string(),
            "--version",
            &version.to_string(),
            "--generation",
            "0",
        ],
        fixture.root.path(),
        0,
    );
    let parsed = td_json::parse(&output).unwrap();
    assert_eq!(
        parsed.get("published").and_then(td_json::Json::as_bool),
        Some(false)
    );
    let request = parsed
        .get("request")
        .and_then(td_json::Json::as_str)
        .unwrap();
    assert_eq!(request.len(), RETAINED_BYTES * 2);
    let mut bytes = [0; RETAINED_BYTES];
    for (byte, pair) in bytes.iter_mut().zip(request.as_bytes().as_chunks::<2>().0) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    Change::decode(&bytes).unwrap()
}
fn invoke(
    fixture: &Fixture,
    connection: &Path,
    command: &str,
    change: Change,
    status: i32,
) -> String {
    let request: String = change
        .encode()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let output = command_output(
        &[
            "postgres",
            command,
            "--connection-file",
            connection.to_str().unwrap(),
            "--request",
            &request,
        ],
        fixture.root.path(),
        status,
    );
    assert!(!output.contains(connection.to_str().unwrap()));
    assert!(!output.contains(std::str::from_utf8(super::PASSWORD).unwrap()));
    output
}
pub(super) fn absent(fixture: &Fixture, connection: &Path, change: Change) {
    let output = invoke(fixture, connection, "config-selection-inspect", change, 0);
    assert!(output.contains("\"recorded\":false"));
}
pub(super) fn recorded(fixture: &Fixture, connection: &Path, change: Change, generation: u64) {
    for command in ["config-selection-inspect", "config-select"] {
        let output = invoke(fixture, connection, command, change, 0);
        let parsed = td_json::parse(&output).unwrap();
        assert_eq!(
            parsed.get("recorded").and_then(td_json::Json::as_bool),
            Some(true)
        );
        assert!(output.contains(&format!("\"generation\":{generation},")));
        assert!(output.contains(&change.operation().to_string()));
        assert!(output.contains(&change.version().to_string()));
    }
    let mut bytes = change.encode();
    bytes[57] ^= 1;
    let altered = Change::decode(&bytes).unwrap();
    for command in ["config-selection-inspect", "config-select"] {
        assert!(invoke(fixture, connection, command, altered, 1)
            .contains("request-or-generation-conflict"));
    }
}
pub(super) fn uncertain(fixture: &Fixture, connection: &Path, change: Change) {
    let output = invoke(fixture, connection, "config-select", change, 1);
    assert!(output.contains("indeterminate-retain-request"));
    assert!(!output.contains("\"status\":\"ok\""));
}
