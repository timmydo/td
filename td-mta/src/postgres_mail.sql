-- Mail metadata and chunk shape only; publication validates aggregates.
CREATE TABLE td_mta.accounts(
    id bytea NOT NULL CHECK(octet_length(id)=16),
    sequence bytea NOT NULL CHECK(octet_length(sequence)=8),
    floor bytea NOT NULL CHECK(octet_length(floor)=8 AND floor<=sequence),
    PRIMARY KEY(id)
);
CREATE TABLE td_mta.blob_ids(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id)
);
CREATE TABLE td_mta.blobs(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    length bigint NOT NULL CHECK(length BETWEEN 0 AND 33554432),
    digest bytea NOT NULL CHECK(octet_length(digest)=32),
    created_at bigint NOT NULL,
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    chunk_count bigint NOT NULL CHECK(chunk_count=(length+65535)/65536),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account,id) REFERENCES td_mta.blob_ids(account,id)
);
CREATE TABLE td_mta.blob_chunks(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    ordinal bigint NOT NULL CHECK(ordinal BETWEEN 0 AND 511),
    bytes bytea NOT NULL CHECK(octet_length(bytes) BETWEEN 1 AND 65536),
    PRIMARY KEY(account,blob_id,ordinal),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.mailboxes(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    name text COLLATE "C" NOT NULL CHECK(octet_length(name) BETWEEN 1 AND 1024),
    parent_id bytea CHECK(parent_id IS NULL OR octet_length(parent_id)=16),
    role text COLLATE "C" CHECK(role IS NULL OR octet_length(role) BETWEEN 1 AND 64),
    sort_order bigint NOT NULL CHECK(sort_order BETWEEN 0 AND 2147483647),
    subscribed bigint NOT NULL CHECK(subscribed IN (0,1)),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    CHECK(parent_id IS NULL OR parent_id<>id),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,parent_id) REFERENCES td_mta.mailboxes(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX mailboxes_parent ON td_mta.mailboxes(account,parent_id);
CREATE TABLE td_mta.threads(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id)
);
CREATE TABLE td_mta.emails(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    thread_id bytea NOT NULL CHECK(octet_length(thread_id)=16),
    received_at bigint NOT NULL,
    origin bigint NOT NULL CHECK(origin BETWEEN 1 AND 4),
    peer_family bigint,
    peer_octets bytea,
    gateway text COLLATE "C" CHECK(gateway IS NULL OR octet_length(gateway) BETWEEN 1 AND 64),
    tls bigint,
    ehlo text COLLATE "C",
    reverse_path text COLLATE "C",
    receipt_count bigint,
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    CHECK((origin=1 AND peer_family IS NOT NULL AND peer_family IN (4,6) AND peer_octets IS NOT NULL AND octet_length(peer_octets)=CASE peer_family WHEN 4 THEN 4 ELSE 16 END AND tls IS NOT NULL AND tls IN (0,1,2) AND ehlo IS NOT NULL AND octet_length(ehlo) BETWEEN 1 AND 255 AND reverse_path IS NOT NULL AND octet_length(reverse_path)<=254 AND receipt_count IS NOT NULL AND receipt_count BETWEEN 1 AND 1000) OR (origin<>1 AND peer_family IS NULL AND peer_octets IS NULL AND gateway IS NULL AND tls IS NULL AND ehlo IS NULL AND reverse_path IS NULL AND receipt_count IS NULL)),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY(account,thread_id) REFERENCES td_mta.threads(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX emails_blob ON td_mta.emails(account,blob_id);
CREATE INDEX emails_thread ON td_mta.emails(account,thread_id,id);
CREATE TABLE td_mta.smtp_receipt_recipients(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    ordinal bigint NOT NULL CHECK(ordinal BETWEEN 0 AND 999),
    address text COLLATE "C" NOT NULL CHECK(octet_length(address) BETWEEN 1 AND 254),
    PRIMARY KEY(account,email_id,ordinal),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.emails(account,id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.memberships(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    mailbox_id bytea NOT NULL CHECK(octet_length(mailbox_id)=16),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    PRIMARY KEY(account,email_id,mailbox_id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.emails(account,id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY(account,mailbox_id) REFERENCES td_mta.mailboxes(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX memberships_mailbox ON td_mta.memberships(account,mailbox_id,email_id);
CREATE TABLE td_mta.keywords(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    keyword text COLLATE "C" NOT NULL CHECK(octet_length(keyword) BETWEEN 1 AND 255),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    PRIMARY KEY(account,email_id,keyword),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.emails(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.thread_anchors(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    message_id text COLLATE "C" NOT NULL CHECK(octet_length(message_id) BETWEEN 1 AND 1004),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    PRIMARY KEY(account,message_id,email_id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES td_mta.emails(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX anchors_email ON td_mta.thread_anchors(account,email_id);
CREATE TABLE td_mta.submissions(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    thread_id bytea NOT NULL CHECK(octet_length(thread_id)=16),
    identity_id bytea NOT NULL CHECK(octet_length(identity_id)=16),
    transmitted_blob_id bytea NOT NULL CHECK(octet_length(transmitted_blob_id)=16),
    reverse_path text COLLATE "C" NOT NULL CHECK(octet_length(reverse_path) BETWEEN 0 AND 254),
    send_at bigint NOT NULL,
    expires_at bigint NOT NULL CHECK(expires_at>=send_at),
    recipient_count bigint NOT NULL CHECK(recipient_count BETWEEN 1 AND 1000),
    completed_at bigint,
    notification bigint NOT NULL CHECK(notification BETWEEN 0 AND 2),
    notification_email_id bytea CHECK(notification_email_id IS NULL OR octet_length(notification_email_id)=16),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    CHECK((notification=2)=(notification_email_id IS NOT NULL)),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,transmitted_blob_id) REFERENCES td_mta.blobs(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX submissions_blob ON td_mta.submissions(account,transmitted_blob_id);
CREATE TABLE td_mta.recipients(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    submission_id bytea NOT NULL CHECK(octet_length(submission_id)=16),
    ordinal bigint NOT NULL CHECK(ordinal BETWEEN 0 AND 999),
    address text COLLATE "C" NOT NULL CHECK(octet_length(address) BETWEEN 1 AND 254),
    state bigint NOT NULL CHECK(state BETWEEN 1 AND 7),
    uncertain bigint NOT NULL CHECK(uncertain IN (0,1)),
    attempt_id bytea CHECK(attempt_id IS NULL OR octet_length(attempt_id)=16),
    attempt_count bigint NOT NULL CHECK(attempt_count BETWEEN 0 AND 4294967295),
    last_attempt_at bigint,
    phase bigint NOT NULL CHECK(phase BETWEEN 0 AND 4),
    next_attempt_at bigint,
    rcpt_reply text COLLATE "C" CHECK(rcpt_reply IS NULL OR octet_length(rcpt_reply) BETWEEN 1 AND 4096),
    data_reply text COLLATE "C" CHECK(data_reply IS NULL OR octet_length(data_reply) BETWEEN 1 AND 4096),
    reason bigint NOT NULL CHECK(reason BETWEEN 0 AND 9),
    diagnostic text COLLATE "C" NOT NULL CHECK(octet_length(diagnostic) BETWEEN 0 AND 512),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    CHECK((attempt_count>0)=(attempt_id IS NOT NULL) AND (attempt_count>0)=(last_attempt_at IS NOT NULL) AND (attempt_count>0)=(phase<>0)),
    CHECK(state<>7 OR uncertain=1),
    CHECK(state<>6 OR uncertain=0),
    PRIMARY KEY(account,submission_id,ordinal),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,submission_id) REFERENCES td_mta.submissions(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.leases(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    device_id bytea NOT NULL CHECK(octet_length(device_id)=16),
    expires_at bigint NOT NULL,
    uses bigint NOT NULL CHECK(uses IN (1,2,3)),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    PRIMARY KEY(account,blob_id),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE td_mta.imports(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    source_instance bytea NOT NULL CHECK(octet_length(source_instance)=16),
    source_kind bigint NOT NULL CHECK(source_kind IN (1,3)),
    source_account bytea NOT NULL CHECK(octet_length(source_account) BETWEEN 1 AND 998),
    source_object bytea NOT NULL CHECK(octet_length(source_object) BETWEEN 1 AND 998),
    local_object bytea NOT NULL CHECK(octet_length(local_object)=16),
    historical_blob bytea CHECK(historical_blob IS NULL OR octet_length(historical_blob)=16),
    source_digest bytea NOT NULL CHECK(octet_length(source_digest)=32),
    changed bytea NOT NULL CHECK(octet_length(changed)=8),
    CHECK(octet_length(source_account)+octet_length(source_object)<=999),
    CHECK((source_kind=3)=(historical_blob IS NOT NULL)),
    PRIMARY KEY(account,source_instance,source_kind,source_account,source_object),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id)
);
CREATE TABLE td_mta.changes(
    account bytea NOT NULL CHECK(octet_length(account)=16),
    sequence bytea NOT NULL CHECK(octet_length(sequence)=8),
    operation bigint NOT NULL CHECK(operation BETWEEN 0 AND 4095),
    kind bigint NOT NULL CHECK(kind IN (1,2,3,5)),
    action bigint NOT NULL CHECK(action BETWEEN 1 AND 3),
    object bytea NOT NULL CHECK(octet_length(object)=16),
    PRIMARY KEY(account,sequence,operation),
    FOREIGN KEY(account) REFERENCES td_mta.accounts(id)
);
CREATE INDEX changes_kind ON td_mta.changes(account,kind,sequence,operation);
CREATE UNIQUE INDEX changes_object ON td_mta.changes(account,sequence,kind,object);

REVOKE ALL ON TABLE td_mta.accounts FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.blob_ids FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.blobs FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.blob_chunks FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.mailboxes FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.threads FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.emails FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.smtp_receipt_recipients FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.memberships FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.keywords FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.thread_anchors FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.submissions FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.recipients FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.leases FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.imports FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.changes FROM PUBLIC;
