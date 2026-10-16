CREATE TABLE td_mta.route_health(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    route_name text COLLATE "C" NOT NULL CHECK(route_name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    destination bytea NOT NULL CHECK(octet_length(destination)=32),
    revision bytea NOT NULL CHECK(octet_length(revision)=8 AND revision>decode(repeat('00',8),'hex')),
    failure_count bigint NOT NULL CHECK(failure_count BETWEEN 0 AND 4294967295),
    reason bigint NOT NULL CHECK(reason IN(0,3,4,5,8)),
    failed_at bigint CHECK(failed_at>=0),
    next_at bigint,
    repair_generation bytea CHECK(repair_generation IS NULL OR (octet_length(repair_generation)=8 AND repair_generation>decode(repeat('00',8),'hex'))),
    CHECK((failure_count=0)=(failed_at IS NULL) AND (failure_count=0)=(next_at IS NULL)),
    CHECK(failure_count<>0 OR (reason=0 AND repair_generation IS NULL)),
    CHECK(failure_count=0 OR (next_at>=failed_at AND next_at-failed_at BETWEEN 300000 AND 3600000)),
    CHECK(repair_generation IS NULL OR reason IN(4,5)),
    PRIMARY KEY(account,route_name,destination)
);
CREATE TABLE td_mta.route_health_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    route_name text COLLATE "C" NOT NULL CHECK(route_name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    destination bytea NOT NULL CHECK(octet_length(destination)=32),
    action bigint NOT NULL CHECK(action BETWEEN 1 AND 3),
    old_revision bytea NOT NULL CHECK(octet_length(old_revision)=8),
    new_revision bytea NOT NULL CHECK(octet_length(new_revision)=8 AND new_revision>old_revision),
    failure_count bigint NOT NULL CHECK(failure_count BETWEEN 0 AND 4294967295),
    reason bigint NOT NULL CHECK(reason IN(0,3,4,5,8)),
    failed_at bigint CHECK(failed_at>=0),
    next_at bigint,
    repair_generation bytea CHECK(repair_generation IS NULL OR (octet_length(repair_generation)=8 AND repair_generation>decode(repeat('00',8),'hex'))),
    CHECK((failure_count=0)=(failed_at IS NULL) AND (failure_count=0)=(next_at IS NULL)),
    CHECK(failure_count<>0 OR (reason=0 AND repair_generation IS NULL)),
    CHECK(failure_count=0 OR (next_at>=failed_at AND next_at-failed_at BETWEEN 300000 AND 3600000)),
    CHECK(repair_generation IS NULL OR reason IN(4,5)),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.route_health,td_mta.route_health_receipts FROM PUBLIC;
