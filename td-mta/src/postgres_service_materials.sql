CREATE SCHEMA td_mta_service;
REVOKE ALL ON SCHEMA td_mta_service FROM PUBLIC;

CREATE TABLE td_mta_service.material_bindings (
    login name NOT NULL, version_id bytea NOT NULL, material_id bytea NOT NULL,
    PRIMARY KEY (login, material_id)
);
CREATE TABLE td_mta_service.certificate_bindings (
    login name NOT NULL, version_id bytea NOT NULL, profile text NOT NULL,
    PRIMARY KEY (login, profile)
);
REVOKE ALL ON ALL TABLES IN SCHEMA td_mta_service FROM PUBLIC;
