CREATE TABLE td_mta.submission_attempts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    attempt_id bytea NOT NULL CHECK(octet_length(attempt_id)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    submission_id bytea NOT NULL CHECK(octet_length(submission_id)=16),
    route_name text COLLATE "C" NOT NULL CHECK(route_name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    destination bytea NOT NULL CHECK(octet_length(destination)=32),
    owner_boot bytea NOT NULL CHECK(octet_length(owner_boot)=16),
    owner_operation bytea NOT NULL CHECK(octet_length(owner_operation)=16),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    prepared_at bigint NOT NULL,
    ordinals bytea NOT NULL CHECK(octet_length(ordinals) BETWEEN 4 AND 400 AND octet_length(ordinals)%4=0),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    active boolean NOT NULL,
    PRIMARY KEY(account,attempt_id)
);
CREATE UNIQUE INDEX active_route_attempt ON td_mta.submission_attempts(account,route_name) WHERE active;
REVOKE ALL ON TABLE td_mta.submission_attempts FROM PUBLIC;
