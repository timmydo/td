-- Diagnostic retention is independent of durable mail publication.
CREATE TABLE td_mta.application_log_state(
    singleton boolean PRIMARY KEY CHECK(singleton),
    next_id bigint NOT NULL CHECK(next_id > 0),
    window_ms bigint NOT NULL CHECK(window_ms >= 0),
    used integer NOT NULL CHECK(used BETWEEN 0 AND 600),
    suppressed bigint NOT NULL CHECK(suppressed >= 0),
    evicted bigint NOT NULL CHECK(evicted >= 0)
);
INSERT INTO td_mta.application_log_state VALUES(true,1,0,0,0,0);
CREATE TABLE td_mta.application_logs(
    slot integer PRIMARY KEY CHECK(slot BETWEEN 0 AND 65535),
    id bigint NOT NULL UNIQUE CHECK(id > 0),
    recorded_ms bigint NOT NULL CHECK(recorded_ms >= 0),
    profile text COLLATE "C" NOT NULL CHECK(profile ~ '^[a-z][a-z0-9_-]{0,63}$'),
    event text NOT NULL CHECK(octet_length(event) BETWEEN 1 AND 1024)
) WITH (fillfactor=80,autovacuum_vacuum_scale_factor=0.01,
    autovacuum_vacuum_threshold=50);
CREATE INDEX application_logs_age ON td_mta.application_logs(recorded_ms,slot);
REVOKE ALL ON TABLE td_mta.application_log_state,td_mta.application_logs FROM PUBLIC;
