//! Verify authority on the authenticated session after a real owner changes ACLs.
use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    postgres_probe::{Mode, Request},
    postgres_runtime_access::{self as access, ServiceRole},
};

pub(super) fn rejects_drift(fixture: &Fixture, request: &Request<'_>, trust: &TrustStore) {
    assert_eq!(request.user, ServiceRole::Receive.login());
    let check = Request {
        // runtime-check uses preparation mode to report its own precise error.
        mode: Mode::Preparation,
        ..*request
    };
    let receipt = access::inspect(&check, trust, deadline()).unwrap();
    for (change, restore) in [
        (
            "GRANT SELECT ON td_mta.config_material_chunks TO td_mta_receive",
            "REVOKE SELECT ON td_mta.config_material_chunks FROM td_mta_receive",
        ),
        (
            "GRANT SELECT (verifier) ON td_mta.devices TO td_mta_receive",
            "REVOKE SELECT (verifier) ON td_mta.devices FROM td_mta_receive",
        ),
        (
            "GRANT SELECT (tableoid) ON td_mta.devices TO td_mta_receive",
            "REVOKE SELECT (tableoid) ON td_mta.devices FROM td_mta_receive",
        ),
        (
            "GRANT SELECT (revoked_at) ON td_mta.devices TO PUBLIC",
            "REVOKE SELECT (revoked_at) ON td_mta.devices FROM PUBLIC",
        ),
        (
            "REVOKE UPDATE (changed) ON td_mta.mailboxes FROM td_mta_receive",
            "GRANT UPDATE (changed) ON td_mta.mailboxes TO td_mta_receive",
        ),
        (
            "GRANT INSERT ON td_mta.blobs TO td_mta_receive WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR INSERT ON td_mta.blobs FROM td_mta_receive",
        ),
    ] {
        sql(fixture, change);
        assert_eq!(
            access::inspect(&check, trust, deadline()),
            Err(access::Error::Privileges),
            "{change}"
        );
        sql(fixture, restore);
    }
    assert_eq!(access::inspect(&check, trust, deadline()).unwrap(), receipt);
}
