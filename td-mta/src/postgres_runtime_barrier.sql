-- Inert runtime refusal point; owner recovery transitions are separate.
CREATE TABLE td_mta.runtime_barrier(
    singleton boolean PRIMARY KEY CHECK(singleton),
    blocked boolean NOT NULL
);
INSERT INTO td_mta.runtime_barrier VALUES(true,false);
REVOKE ALL ON TABLE td_mta.runtime_barrier FROM PUBLIC;
