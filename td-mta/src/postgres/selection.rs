//! Explicit selection custody and receipt operations; no serving activation.
use super::{configuration, report_error, with_addresses, AttemptError};
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ids::StoreEpoch,
    postgres_configuration_store::{self as versions, Version},
    postgres_input::Loaded,
    postgres_selection::{self as selection, Change, Error, Selection, RETAINED_BYTES},
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Operation {
    Prepare,
    Publish,
    Inspect,
}
impl Operation {
    pub(super) fn parse(value: &OsStr) -> Option<Self> {
        match value.to_str()? {
            "config-selection-prepare" => Some(Self::Prepare),
            "config-select" => Some(Self::Publish),
            "config-selection-inspect" => Some(Self::Inspect),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "postgres-config-selection-prepare",
            Self::Publish => "postgres-config-select",
            Self::Inspect => "postgres-config-selection-inspect",
        }
    }
}
enum Input {
    Prepare {
        epoch: StoreEpoch,
        version: Version,
        generation: u64,
    },
    Retained([u8; RETAINED_BYTES]),
}
struct Options {
    connection: String,
    input: Input,
}
fn arguments(operation: Operation, mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut version, mut generation, mut request) =
        (None, None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if operation == Operation::Prepare && epoch.is_none() => {
                epoch = Some(StoreEpoch::parse(&value).ok()?)
            }
            "--version" if operation == Operation::Prepare && version.is_none() => {
                version = Some(Version::parse(&value).ok()?)
            }
            "--generation" if operation == Operation::Prepare && generation.is_none() => {
                if value.is_empty()
                    || value.len() > 20
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                generation = Some(value.parse().ok()?);
            }
            "--request" if operation != Operation::Prepare && request.is_none() => {
                request = Some(decode_hex(&value)?)
            }
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        input: match operation {
            Operation::Prepare => Input::Prepare {
                epoch: epoch?,
                version: version?,
                generation: generation?,
            },
            Operation::Publish | Operation::Inspect => Input::Retained(request?),
        },
    })
}
fn decode_hex(value: &str) -> Option<[u8; RETAINED_BYTES]> {
    if value.len() != RETAINED_BYTES * 2 {
        return None;
    }
    let mut bytes = [0; RETAINED_BYTES];
    for (out, pair) in bytes.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        *out = digit(*pair.first()?)?
            .checked_mul(16)?
            .checked_add(digit(*pair.get(1)?)?)?;
    }
    Some(bytes)
}
fn encode_hex(bytes: &[u8; RETAINED_BYTES]) -> Result<String, io::Error> {
    use std::fmt::Write;
    let mut out = String::new();
    out.try_reserve_exact(RETAINED_BYTES * 2)
        .map_err(|_| io::Error::other("selection receipt capacity"))?;
    for byte in bytes {
        write!(&mut out, "{byte:02x}")
            .map_err(|_| io::Error::other("selection receipt encoding"))?;
    }
    Ok(out)
}
pub(super) fn run(
    operation: Operation,
    args: impl Iterator<Item = OsString>,
) -> io::Result<ExitCode> {
    let name = operation.name();
    let Some(options) = arguments(operation, args) else {
        report_error(name, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, "configuration") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(name, "bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let change = match options.input {
        Input::Prepare {
            epoch,
            version,
            generation,
        } => {
            let candidate = match with_addresses(&loaded, |request, deadline| {
                versions::read(request, &loaded.trust, deadline, epoch, version)
                    .map_err(configuration::classify)
            }) {
                Ok(candidate) => candidate,
                Err((stage, code)) => {
                    report_error(name, stage, code)?;
                    return Ok(ExitCode::FAILURE);
                }
            };
            selection::prepare(epoch, generation, version, &candidate)
        }
        Input::Retained(bytes) => Change::decode(&bytes),
    };
    let change = match change {
        Ok(change) => change,
        Err(error) => {
            let (stage, code) = fixed(error);
            report_error(name, stage, code)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    if operation == Operation::Prepare {
        let request = encode_hex(&change.encode())?;
        writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"request\":\"{request}\",\"published\":false}}")?;
        return Ok(ExitCode::SUCCESS);
    }
    let result = with_addresses(&loaded, |request, deadline| match operation {
        Operation::Publish => selection::publish(request, &loaded.trust, deadline, change)
            .map(Some)
            .map_err(classify),
        Operation::Inspect => {
            selection::inspect(request, &loaded.trust, deadline, change).map_err(classify)
        }
        Operation::Prepare => Err(AttemptError::Failed("selection", "invalid-operation")),
    });
    match result {
        Ok(receipt) => {
            report(name, change, receipt)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(name, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}
fn report(name: &str, change: Change, receipt: Option<Selection>) -> io::Result<()> {
    match receipt {
        Some(receipt)=>writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"operation\":\"{}\",\"recorded\":true,\"generation\":{},\"version\":\"{}\",\"profile_activated\":false}}",change.operation(),receipt.generation,receipt.version),
        None=>writeln!(io::stdout().lock(),"{{\"schema\":1,\"command\":\"{name}\",\"status\":\"ok\",\"operation\":\"{}\",\"recorded\":false,\"profile_activated\":false}}",change.operation()),
    }
}
fn fixed(error: Error) -> (&'static str, &'static str) {
    match classify(error) {
        AttemptError::Failed(stage, code) => (stage, code),
        AttemptError::Transport => ("selection", "transport"),
    }
}
pub(super) fn classify(error: Error) -> AttemptError {
    match error {
        Error::Connection(error) => td_mta::postgres_schema::Error::Connection(error).into(),
        Error::Schema(error) => error.into(),
        Error::Configuration(error) => configuration::classify(error),
        other => AttemptError::Failed(
            "selection",
            match other {
                Error::Epoch => "epoch-mismatch",
                Error::Conflict => "request-or-generation-conflict",
                Error::Integrity => "selection-integrity",
                Error::Capacity => "selection-capacity",
                Error::Crypto => "crypto",
                Error::Database => "query-failed",
                Error::Indeterminate => "indeterminate-retain-request",
                Error::Connection(_) | Error::Schema(_) | Error::Configuration(_) => {
                    "connection-or-configuration"
                }
            },
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn public_request_hex_refuses_partial_nonascii_and_extra_bytes() {
        let bytes = [0xab; RETAINED_BYTES];
        let hex = encode_hex(&bytes).unwrap();
        assert_eq!(decode_hex(&hex), Some(bytes));
        assert_eq!(decode_hex(&hex.to_ascii_uppercase()), Some(bytes));
        for value in [
            String::new(),
            hex[..hex.len() - 1].to_owned(),
            format!("{hex}00"),
            "é".repeat(RETAINED_BYTES),
            "g0".repeat(RETAINED_BYTES),
        ] {
            assert_eq!(decode_hex(&value), None);
        }
    }
    #[test]
    fn preparation_and_publication_arguments_cannot_mix_or_duplicate() {
        let prepare = [
            "--connection-file",
            "/private/db",
            "--epoch",
            "01010101010101010101010101010101",
            "--version",
            "02020202020202020202020202020202",
            "--generation",
            "0",
        ];
        assert!(arguments(Operation::Prepare, prepare.iter().map(OsString::from)).is_some());
        let token = "00".repeat(RETAINED_BYTES);
        let publish = [
            "--connection-file",
            "/private/db",
            "--request",
            token.as_str(),
        ];
        for operation in [Operation::Publish, Operation::Inspect] {
            assert!(arguments(operation, publish.iter().map(OsString::from)).is_some());
            assert!(arguments(operation, prepare.iter().map(OsString::from)).is_none());
            assert!(arguments(
                operation,
                publish
                    .iter()
                    .chain(["--request", token.as_str()].iter())
                    .map(OsString::from)
            )
            .is_none());
        }
        assert!(arguments(Operation::Prepare, publish.iter().map(OsString::from)).is_none());
        for bad in ["-1", "+1", "18446744073709551616", "", "1 "] {
            let mut values = prepare;
            values[7] = bad;
            assert!(arguments(Operation::Prepare, values.iter().map(OsString::from)).is_none());
        }
    }
}
