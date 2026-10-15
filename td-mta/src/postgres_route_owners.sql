CREATE TABLE td_mta.route_owners(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    route_name text COLLATE "C" NOT NULL CHECK(route_name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    destination bytea NOT NULL CHECK(octet_length(destination)=32),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    owner_boot bytea CHECK(owner_boot IS NULL OR octet_length(owner_boot)=16),
    operation bytea CHECK(operation IS NULL OR octet_length(operation)=16),
    last_clock bigint NOT NULL,
    expires_at bigint,
    CHECK((owner_boot IS NULL)=(operation IS NULL) AND (owner_boot IS NULL)=(expires_at IS NULL)),
    CHECK(expires_at IS NULL OR expires_at>last_clock),
    PRIMARY KEY(account,route_name)
);
CREATE TABLE td_mta.route_owner_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    claimed_at bigint NOT NULL,
    expires_at bigint NOT NULL CHECK(expires_at>claimed_at),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.route_owners,td_mta.route_owner_receipts FROM PUBLIC;
