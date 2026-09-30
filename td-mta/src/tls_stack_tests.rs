//! Representative compiled-policy and retained-transport stack qualification.
use super::*;
#[path = "../tests/support/stack.rs"]
mod stack;

fn scenarios() {
    compiled_policy_roles_with(|pending| pending.construct().unwrap());
    queued_and_native_reservations_retain_capacity_and_recover_exact_buffers();
    gateway_comparison_survives_reordering_but_rejects_policy_or_binding_changes();
    material_errors_never_publish_and_acme_requests_are_explicit();
    material_reader_caps_bytes_eof_and_interrupted_work();
    compiled_configs_enforce_relay_trust_protocol_and_gateway_client_auth();
    https_profiles_narrow_shared_smtp_names_before_native_routing();
    io_tests::handoff_refuses_plaintext_tails_and_deadlines_and_returns_original_buffers();
    io_tests::authorized_connections_release_handshake_capacity_and_retain_generation_until_teardown();
    io_tests::gateway_without_client_certificate_never_exposes_mail_proof_and_recovers_permits();
    io_tests::handshake_deadline_can_only_tighten_and_failure_is_sticky();
    io_tests::changed_gateway_policy_aborts_pending_connection_and_releases_capacity();
    io_tests::established_clock_failure_clears_cached_authorization_and_aborts_socket();
    client_tests::client_only_acme_bootstrap_never_opens_server_material();
    client_tests::expired_server_does_not_block_clients_but_invalid_client_trust_still_refuses();
    client_tests::client_generation_transitions_to_complete_within_the_same_two_slots();
    starttls_tests::server_starttls_flushes_220_before_handoff_and_verifies_real_tls();
    starttls_tests::server_starttls_refuses_unframed_parameterized_tailed_and_wrong_role_commands();
    starttls_tests::server_starttls_deadlines_clock_refusal_and_cancel_release_reservations();
    client_starttls_tests::client_starttls_and_server_upgrade_verify_real_tls();
    client_starttls_tests::client_starttls_refuses_bad_replies_and_accepts_fragmented_220();
    client_starttls_tests::client_starttls_roles_scratch_deadlines_and_cancel_return_buffers();
    gateway_process_tests::gateway_mutual_tls_accepts_current_and_next_verified_leaf_pins();
    gateway_process_tests::gateway_mutual_tls_refuses_verified_leaf_with_wrong_pin_or_actual_peer();
    gateway_process_tests::gateway_starttls_process_requires_verified_pin_after_plaintext_reply();
}

#[test]
fn host_scenarios_detect_fixture_drift() {
    // Fixture drift only; the host harness stack is not target evidence.
    scenarios();
}

#[test]
#[ignore = "run by the isolated pinned-musl qualification"]
fn portable_tls_policy_transport_stack() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) || cfg!(debug_assertions)
    {
        return Err("requires the pinned release x86-64 musl artifact".into());
    }
    std::thread::Builder::new()
        .name("tls-policy-transport-stack".into())
        .stack_size(240 * 1024)
        .spawn(|| {
            stack::bounded_stack_mapping("tls_policy_transport_stack_mapping_bytes", 256 * 1024);
            scenarios();
        })?
        .join()
        .map_err(|_| "TLS policy/transport stack worker failed")?;
    Ok(())
}
