CREATE TABLE td_mta.submission_routes(
    account bytea NOT NULL,
    submission_id bytea NOT NULL,
    submitting_profile text COLLATE "C" NOT NULL
        CHECK(submitting_profile ~ '^[a-z][a-z0-9_-]{0,63}$'),
    route_name text COLLATE "C" NOT NULL
        CHECK(route_name ~ '^[a-z][a-z0-9_-]{0,63}$'),
    destination bytea NOT NULL CHECK(octet_length(destination)=32),
    PRIMARY KEY(account,submission_id),
    FOREIGN KEY(account,submission_id) REFERENCES td_mta.submissions(account,id)
        ON DELETE CASCADE
);
CREATE INDEX submission_routes_destination
    ON td_mta.submission_routes(account,route_name,destination,submission_id);
REVOKE ALL ON TABLE td_mta.submission_routes FROM PUBLIC;
