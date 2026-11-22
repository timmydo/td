-- Direct-MX sender authentication observed when an Email was received.
-- Rows are observation only; no delivery decision reads them.
CREATE TABLE td_mta.email_authentication(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    spf_result text COLLATE "C" NOT NULL CHECK(spf_result IN ('none','neutral','pass','fail','softfail','temperror','permerror')),
    spf_domain text COLLATE "C" CHECK(octet_length(spf_domain) BETWEEN 1 AND 253),
    spf_helo boolean NOT NULL,
    dkim_complete boolean NOT NULL,
    dmarc_result text COLLATE "C" NOT NULL CHECK(dmarc_result IN ('none','pass','fail','temperror','permerror')),
    dmarc_author text COLLATE "C" CHECK(octet_length(dmarc_author) BETWEEN 1 AND 253),
    dmarc_policy_domain text COLLATE "C" CHECK(octet_length(dmarc_policy_domain) BETWEEN 1 AND 253),
    dmarc_policy text COLLATE "C" CHECK(dmarc_policy IN ('none','quarantine','reject')),
    dmarc_strict_spf boolean,
    dmarc_strict_dkim boolean,
    dmarc_aligned text COLLATE "C" CHECK(dmarc_aligned IN ('spf','dkim')),
    CHECK((dmarc_policy IS NULL) = (dmarc_strict_spf IS NULL)
        AND (dmarc_policy IS NULL) = (dmarc_strict_dkim IS NULL)
        AND (dmarc_policy IS NULL OR dmarc_policy_domain IS NOT NULL)),
    CHECK(dmarc_aligned IS NULL OR (dmarc_result='pass' AND dmarc_policy IS NOT NULL)),
    PRIMARY KEY(account,email_id),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.emails(account,id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.email_dkim_signatures(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    ordinal bigint NOT NULL CHECK(ordinal BETWEEN 0 AND 3),
    result text COLLATE "C" NOT NULL CHECK(result IN ('pass','fail','temperror','permerror')),
    algorithm text COLLATE "C" CHECK(algorithm IN ('rsa-sha256','ed25519-sha256')),
    domain text COLLATE "C" CHECK(octet_length(domain) BETWEEN 1 AND 253),
    selector text COLLATE "C" CHECK(octet_length(selector) BETWEEN 1 AND 253),
    key_bits integer CHECK(key_bits BETWEEN 1 AND 65535),
    PRIMARY KEY(account,email_id,ordinal),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.email_authentication(account,email_id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
REVOKE ALL ON TABLE td_mta.email_authentication FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.email_dkim_signatures FROM PUBLIC;
