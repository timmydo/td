//! Explicit authoritative tables; opaque application values are never stored.
pub(super) const SCHEMA: &str = r#"
CREATE TABLE store(
    id INTEGER PRIMARY KEY CHECK(id=1),
    epoch BLOB NOT NULL CHECK(length(epoch)=16)
) STRICT;
CREATE TABLE accounts(
    id BLOB NOT NULL CHECK(length(id)=16),
    sequence BLOB NOT NULL CHECK(length(sequence)=8),
    floor BLOB NOT NULL CHECK(length(floor)=8),
    PRIMARY KEY(id)
) STRICT, WITHOUT ROWID;
CREATE TABLE blob_ids(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES accounts(id)
) STRICT, WITHOUT ROWID;
CREATE TABLE blobs(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    length INTEGER NOT NULL CHECK(length BETWEEN 0 AND 33554432),
    digest BLOB NOT NULL CHECK(length(digest)=32),
    created_at INTEGER NOT NULL,
    changed BLOB NOT NULL CHECK(length(changed)=8),
    body BLOB NOT NULL CHECK(length(body)=length),
    UNIQUE(account,id),
    FOREIGN KEY(account,id) REFERENCES blob_ids(account,id)
) STRICT;
CREATE TABLE mailboxes(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 1024),
    parent_id BLOB CHECK(parent_id IS NULL OR length(parent_id)=16),
    role TEXT CHECK(role IS NULL OR length(CAST(role AS BLOB)) BETWEEN 1 AND 64),
    sort_order INTEGER NOT NULL CHECK(sort_order BETWEEN 0 AND 2147483647),
    subscribed INTEGER NOT NULL CHECK(subscribed IN (0,1)),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    CHECK(parent_id IS NULL OR parent_id<>id),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,parent_id) REFERENCES mailboxes(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE INDEX mailboxes_parent ON mailboxes(account,parent_id);
CREATE TABLE threads(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES accounts(id)
) STRICT, WITHOUT ROWID;
CREATE TABLE emails(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    blob_id BLOB NOT NULL CHECK(length(blob_id)=16),
    thread_id BLOB NOT NULL CHECK(length(thread_id)=16),
    received_at INTEGER NOT NULL,
    origin INTEGER NOT NULL CHECK(origin BETWEEN 1 AND 4),
    peer_family INTEGER,
    peer_octets BLOB,
    gateway TEXT CHECK(gateway IS NULL OR length(CAST(gateway AS BLOB)) BETWEEN 1 AND 64),
    tls INTEGER,
    ehlo TEXT,
    reverse_path TEXT,
    receipt_count INTEGER,
    changed BLOB NOT NULL CHECK(length(changed)=8),
    CHECK((origin=1 AND peer_family IS NOT NULL AND peer_family IN (4,6) AND peer_octets IS NOT NULL AND length(peer_octets)=CASE peer_family WHEN 4 THEN 4 ELSE 16 END AND tls IS NOT NULL AND tls IN (0,1,2) AND ehlo IS NOT NULL AND length(CAST(ehlo AS BLOB)) BETWEEN 1 AND 255 AND reverse_path IS NOT NULL AND length(CAST(reverse_path AS BLOB))<=254 AND receipt_count IS NOT NULL AND receipt_count BETWEEN 1 AND 1000) OR (origin<>1 AND peer_family IS NULL AND peer_octets IS NULL AND gateway IS NULL AND tls IS NULL AND ehlo IS NULL AND reverse_path IS NULL AND receipt_count IS NULL)),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,blob_id) REFERENCES blobs(account,id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY(account,thread_id) REFERENCES threads(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE INDEX emails_blob ON emails(account,blob_id);
CREATE INDEX emails_thread ON emails(account,thread_id,id);
CREATE TABLE smtp_receipt_recipients(
    account BLOB NOT NULL CHECK(length(account)=16),
    email_id BLOB NOT NULL CHECK(length(email_id)=16),
    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 999),
    address TEXT NOT NULL CHECK(length(CAST(address AS BLOB)) BETWEEN 1 AND 254),
    PRIMARY KEY(account,email_id,ordinal),
    FOREIGN KEY(account,email_id) REFERENCES emails(account,id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE TABLE memberships(
    account BLOB NOT NULL CHECK(length(account)=16),
    email_id BLOB NOT NULL CHECK(length(email_id)=16),
    mailbox_id BLOB NOT NULL CHECK(length(mailbox_id)=16),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    PRIMARY KEY(account,email_id,mailbox_id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES emails(account,id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY(account,mailbox_id) REFERENCES mailboxes(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE INDEX memberships_mailbox ON memberships(account,mailbox_id,email_id);
CREATE TABLE keywords(
    account BLOB NOT NULL CHECK(length(account)=16),
    email_id BLOB NOT NULL CHECK(length(email_id)=16),
    keyword TEXT NOT NULL CHECK(length(CAST(keyword AS BLOB)) BETWEEN 1 AND 255),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    PRIMARY KEY(account,email_id,keyword),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES emails(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE TABLE thread_anchors(
    account BLOB NOT NULL CHECK(length(account)=16),
    message_id TEXT COLLATE BINARY NOT NULL CHECK(length(CAST(message_id AS BLOB)) BETWEEN 1 AND 1004),
    email_id BLOB NOT NULL CHECK(length(email_id)=16),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    PRIMARY KEY(account,message_id,email_id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,email_id) REFERENCES emails(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE INDEX anchors_email ON thread_anchors(account,email_id);
CREATE TABLE submissions(
    account BLOB NOT NULL CHECK(length(account)=16),
    id BLOB NOT NULL CHECK(length(id)=16),
    email_id BLOB NOT NULL CHECK(length(email_id)=16),
    thread_id BLOB NOT NULL CHECK(length(thread_id)=16),
    identity_id BLOB NOT NULL CHECK(length(identity_id)=16),
    transmitted_blob_id BLOB NOT NULL CHECK(length(transmitted_blob_id)=16),
    reverse_path TEXT NOT NULL CHECK(length(CAST(reverse_path AS BLOB)) BETWEEN 0 AND 254),
    send_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK(expires_at>=send_at),
    recipient_count INTEGER NOT NULL CHECK(recipient_count BETWEEN 1 AND 1000),
    completed_at INTEGER,
    notification INTEGER NOT NULL CHECK(notification BETWEEN 0 AND 2),
    notification_email_id BLOB CHECK(notification_email_id IS NULL OR length(notification_email_id)=16),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    CHECK((notification=2)=(notification_email_id IS NOT NULL)),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,transmitted_blob_id) REFERENCES blobs(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE INDEX submissions_blob ON submissions(account,transmitted_blob_id);
CREATE TABLE recipients(
    account BLOB NOT NULL CHECK(length(account)=16),
    submission_id BLOB NOT NULL CHECK(length(submission_id)=16),
    ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 999),
    address TEXT NOT NULL CHECK(length(CAST(address AS BLOB)) BETWEEN 1 AND 254),
    state INTEGER NOT NULL CHECK(state BETWEEN 1 AND 7),
    uncertain INTEGER NOT NULL CHECK(uncertain IN (0,1)),
    attempt_id BLOB CHECK(attempt_id IS NULL OR length(attempt_id)=16),
    attempt_count INTEGER NOT NULL CHECK(attempt_count BETWEEN 0 AND 4294967295),
    last_attempt_at INTEGER,
    phase INTEGER NOT NULL CHECK(phase BETWEEN 0 AND 4),
    next_attempt_at INTEGER,
    rcpt_reply TEXT CHECK(rcpt_reply IS NULL OR length(CAST(rcpt_reply AS BLOB)) BETWEEN 1 AND 4096),
    data_reply TEXT CHECK(data_reply IS NULL OR length(CAST(data_reply AS BLOB)) BETWEEN 1 AND 4096),
    reason INTEGER NOT NULL CHECK(reason BETWEEN 0 AND 9),
    diagnostic TEXT NOT NULL CHECK(length(CAST(diagnostic AS BLOB)) BETWEEN 0 AND 512),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    CHECK((attempt_count>0)=(attempt_id IS NOT NULL) AND (attempt_count>0)=(last_attempt_at IS NOT NULL) AND (attempt_count>0)=(phase<>0)),
    CHECK(state<>7 OR uncertain=1),
    CHECK(state<>6 OR uncertain=0),
    PRIMARY KEY(account,submission_id,ordinal),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,submission_id) REFERENCES submissions(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE TABLE leases(
    account BLOB NOT NULL CHECK(length(account)=16),
    blob_id BLOB NOT NULL CHECK(length(blob_id)=16),
    device_id BLOB NOT NULL CHECK(length(device_id)=16),
    expires_at INTEGER NOT NULL,
    uses INTEGER NOT NULL CHECK(uses IN (1,2,3)),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    PRIMARY KEY(account,blob_id),
    FOREIGN KEY(account) REFERENCES accounts(id),
    FOREIGN KEY(account,blob_id) REFERENCES blobs(account,id) DEFERRABLE INITIALLY DEFERRED
) STRICT, WITHOUT ROWID;
CREATE TABLE imports(
    account BLOB NOT NULL CHECK(length(account)=16),
    source_instance BLOB NOT NULL CHECK(length(source_instance)=16),
    source_kind INTEGER NOT NULL CHECK(source_kind IN (1,3)),
    source_account BLOB NOT NULL CHECK(length(source_account) BETWEEN 1 AND 998),
    source_object BLOB NOT NULL CHECK(length(source_object) BETWEEN 1 AND 998),
    local_object BLOB NOT NULL CHECK(length(local_object)=16),
    historical_blob BLOB CHECK(historical_blob IS NULL OR length(historical_blob)=16),
    source_digest BLOB NOT NULL CHECK(length(source_digest)=32),
    changed BLOB NOT NULL CHECK(length(changed)=8),
    CHECK(length(source_account)+length(source_object)<=999),
    CHECK((source_kind=3)=(historical_blob IS NOT NULL)),
    PRIMARY KEY(account,source_instance,source_kind,source_account,source_object),
    FOREIGN KEY(account) REFERENCES accounts(id)
) STRICT, WITHOUT ROWID;
CREATE TABLE changes(
    account BLOB NOT NULL CHECK(length(account)=16),
    sequence BLOB NOT NULL CHECK(length(sequence)=8),
    operation INTEGER NOT NULL CHECK(operation BETWEEN 0 AND 4095),
    kind INTEGER NOT NULL CHECK(kind IN (1,2,3,5)),
    action INTEGER NOT NULL CHECK(action BETWEEN 1 AND 3),
    object BLOB NOT NULL CHECK(length(object)=16),
    PRIMARY KEY(account,sequence,operation),
    FOREIGN KEY(account) REFERENCES accounts(id)
) STRICT, WITHOUT ROWID;
CREATE INDEX changes_kind ON changes(account,kind,sequence,operation);
CREATE UNIQUE INDEX changes_object ON changes(account,sequence,kind,object);
"#;

// Closed statement inventory. Parameter 1 is always the authorized account.
use crate::format::Table;
pub(super) struct Queries {
    pub get: &'static str,
    pub first: &'static str,
    pub next: &'static str,
    pub delete: &'static str,
}
pub(super) fn queries(table: Table) -> Queries {
    match table {
        Table::Blobs => BLOBS,
        Table::Mailboxes => MAILBOXES,
        Table::Emails => EMAILS,
        Table::Memberships => MEMBERSHIPS,
        Table::Keywords => KEYWORDS,
        Table::Threads => THREADS,
        Table::ThreadAnchors => THREADANCHORS,
        Table::Submissions => SUBMISSIONS,
        Table::Recipients => RECIPIENTS,
        Table::Leases => LEASES,
        Table::Imports => IMPORTS,
    }
}

const BLOBS: Queries = Queries {
    get: "SELECT id,length,digest,created_at,changed FROM blobs WHERE account=?1 AND id=?2",
    first: concat!(
        "SELECT id,length,digest,created_at,",
        "changed FROM blobs WHERE account=?1 ORDER BY id LIMIT 1",
    ),
    next: concat!(
        "SELECT id,length,digest,created_at,",
        "changed FROM blobs WHERE account=?1 AND id>?2 ORDER BY id LIMIT 1",
    ),
    delete: "DELETE FROM blobs WHERE account=?1 AND id=?2",
};

const MAILBOXES: Queries = Queries {
    get: concat!(
        "SELECT id,name,parent_id,role,sort_order,subscribed,",
        "changed FROM mailboxes WHERE account=?1 AND id=?2",
    ),
    first: concat!(
        "SELECT id,name,parent_id,role,sort_order,subscribed,",
        "changed FROM mailboxes WHERE account=?1 ORDER BY id LIMIT 1",
    ),
    next: concat!(
        "SELECT id,name,parent_id,role,sort_order,subscribed,",
        "changed FROM mailboxes WHERE account=?1 AND id>?2 ORDER BY id LIMIT 1",
    ),
    delete: "DELETE FROM mailboxes WHERE account=?1 AND id=?2",
};

const EMAILS: Queries = Queries {
    get: concat!(
        "SELECT id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,gateway,tls,ehlo,",
        "reverse_path,receipt_count,changed FROM emails WHERE account=?1 AND id=?2",
    ),
    first: concat!(
        "SELECT id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,gateway,tls,ehlo,",
        "reverse_path,receipt_count,changed FROM emails WHERE account=?1 ORDER BY id LIMIT 1",
    ),
    next: concat!(
    "SELECT id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,gateway,tls,ehlo,",
    "reverse_path,receipt_count,changed FROM emails WHERE account=?1 AND id>?2 ORDER BY id LIMIT 1",
),
    delete: "DELETE FROM emails WHERE account=?1 AND id=?2",
};

const MEMBERSHIPS: Queries = Queries {
    get: concat!(
        "SELECT email_id,mailbox_id,",
        "changed FROM memberships WHERE account=?1 AND email_id=?2 AND mailbox_id=?3",
    ),
    first: concat!(
        "SELECT email_id,mailbox_id,changed FROM memberships WHERE account=?1 ORDER BY email_id,",
        "mailbox_id LIMIT 1",
    ),
    next: concat!(
        "SELECT email_id,mailbox_id,changed FROM memberships WHERE account=?1 AND (email_id,",
        "mailbox_id)>(?2,?3) ORDER BY email_id,mailbox_id LIMIT 1",
    ),
    delete: "DELETE FROM memberships WHERE account=?1 AND email_id=?2 AND mailbox_id=?3",
};

const KEYWORDS: Queries = Queries {
    get: "SELECT email_id,keyword,changed FROM keywords WHERE account=?1 AND email_id=?2 AND keyword=?3",
    first: concat!(
        "SELECT email_id,keyword,changed FROM keywords WHERE account=?1 ORDER BY email_id,",
        "keyword LIMIT 1",
    ),
    next: concat!(
    "SELECT email_id,keyword,changed FROM keywords WHERE account=?1 AND (email_id,keyword)>(?2,",
    "?3) ORDER BY email_id,keyword LIMIT 1",
),
    delete: "DELETE FROM keywords WHERE account=?1 AND email_id=?2 AND keyword=?3",
};

const THREADS: Queries = Queries {
    get: "SELECT id,changed FROM threads WHERE account=?1 AND id=?2",
    first: "SELECT id,changed FROM threads WHERE account=?1 ORDER BY id LIMIT 1",
    next: "SELECT id,changed FROM threads WHERE account=?1 AND id>?2 ORDER BY id LIMIT 1",
    delete: "DELETE FROM threads WHERE account=?1 AND id=?2",
};

const THREADANCHORS: Queries = Queries {
    get: concat!(
        "SELECT message_id,email_id,",
        "changed FROM thread_anchors WHERE account=?1 AND message_id=?2 AND email_id=?3",
    ),
    first: concat!(
        "SELECT message_id,email_id,",
        "changed FROM thread_anchors WHERE account=?1 ORDER BY ",
        "message_id,email_id LIMIT 1",
    ),
    next: concat!(
        "SELECT message_id,email_id,changed FROM thread_anchors WHERE account=?1 AND (",
        "message_id,email_id)>(?2,?3) ORDER BY ",
        "message_id,email_id LIMIT 1",
    ),
    delete: "DELETE FROM thread_anchors WHERE account=?1 AND message_id=?2 AND email_id=?3",
};

const SUBMISSIONS: Queries = Queries {
    get: concat!(
    "SELECT id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,",
    "recipient_count,completed_at,notification,notification_email_id,",
    "changed FROM submissions WHERE account=?1 AND id=?2",
),
    first: concat!(
    "SELECT id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,",
    "recipient_count,completed_at,notification,notification_email_id,",
    "changed FROM submissions WHERE account=?1 ORDER BY id LIMIT 1",
),
    next: concat!(
    "SELECT id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,",
    "recipient_count,completed_at,notification,notification_email_id,",
    "changed FROM submissions WHERE account=?1 AND id>?2 ORDER BY id LIMIT 1",
),
    delete: "DELETE FROM submissions WHERE account=?1 AND id=?2",
};

const RECIPIENTS: Queries = Queries {

get: concat!(
    "SELECT submission_id,ordinal,address,state,uncertain,attempt_id,attempt_count,last_attempt_at,",
    "phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,",
    "changed FROM recipients WHERE account=?1 AND submission_id=?2 AND ordinal=?3",
),
first: concat!(
    "SELECT submission_id,ordinal,address,state,uncertain,attempt_id,attempt_count,last_attempt_at,",
    "phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,",
    "changed FROM recipients WHERE account=?1 ORDER BY submission_id,ordinal LIMIT 1",
),
next: concat!(
    "SELECT submission_id,ordinal,address,state,uncertain,attempt_id,attempt_count,last_attempt_at,",
    "phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,",
    "changed FROM recipients WHERE account=?1 AND (submission_id,ordinal)>(?2,",
    "?3) ORDER BY submission_id,ordinal LIMIT 1",
),
delete: "DELETE FROM recipients WHERE account=?1 AND submission_id=?2 AND ordinal=?3",

};

const LEASES: Queries = Queries {
    get: concat!(
        "SELECT blob_id,account,device_id,expires_at,uses,",
        "changed FROM leases WHERE account=?1 AND blob_id=?2",
    ),
    first: concat!(
        "SELECT blob_id,account,device_id,expires_at,uses,",
        "changed FROM leases WHERE account=?1 ORDER BY blob_id LIMIT 1",
    ),
    next: concat!(
        "SELECT blob_id,account,device_id,expires_at,uses,",
        "changed FROM leases WHERE account=?1 AND blob_id>?2 ORDER BY blob_id LIMIT 1",
    ),
    delete: "DELETE FROM leases WHERE account=?1 AND blob_id=?2",
};

const IMPORTS: Queries = Queries {

get: concat!(
    "SELECT source_instance,source_kind,source_account,source_object,local_object,historical_blob,",
    "source_digest,",
    "changed FROM imports WHERE account=?1 AND source_instance=?2 AND source_kind=?3 AND ",
    "source_account=?4 AND source_object=?5",
),
first: concat!(
    "SELECT source_instance,source_kind,source_account,source_object,local_object,historical_blob,",
    "source_digest,changed FROM imports WHERE account=?1 ORDER BY source_instance,source_kind,",
    "source_account,source_object LIMIT 1",
),
next: concat!(
    "SELECT source_instance,source_kind,source_account,source_object,local_object,historical_blob,",
    "source_digest,changed FROM imports WHERE account=?1 AND (source_instance,source_kind,",
    "source_account,source_object)>(?2,?3,?4,",
    "?5) ORDER BY source_instance,source_kind,source_account,",
    "source_object LIMIT 1",
),
delete: "DELETE FROM imports WHERE account=?1 AND source_instance=?2 AND source_kind=?3 AND source_account=?4 AND source_object=?5",

};

// Identical immutable metadata puts retain the original changed sequence.
pub(super) const PUT_BLOBS: &str = concat!(
    "INSERT INTO blobs(account,id,length,digest,created_at,changed,body) ",
    "VALUES(?1,?2,?3,?4,?5,?6,zeroblob(?3)) ON CONFLICT(account,id) DO NOTHING",
);

pub(super) const PUT_MAILBOXES: &str = concat!(
    "INSERT INTO mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) VALUES(?1,",
    "?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(account,id) DO UPDATE SET name=excluded.name,",
    "parent_id=excluded.parent_id,role=excluded.role,sort_order=excluded.sort_order,",
    "subscribed=excluded.subscribed,changed=excluded.changed",
);

pub(super) const PUT_EMAILS: &str = concat!(
    "INSERT INTO emails(account,id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,",
    "gateway,tls,ehlo,reverse_path,receipt_count,changed) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,",
    "?11,?12,?13,?14) ON CONFLICT(account,id) DO UPDATE SET blob_id=excluded.blob_id,",
    "thread_id=excluded.thread_id,received_at=excluded.received_at,origin=excluded.origin,",
    "peer_family=excluded.peer_family,peer_octets=excluded.peer_octets,gateway=excluded.gateway,",
    "tls=excluded.tls,ehlo=excluded.ehlo,reverse_path=excluded.reverse_path,",
    "receipt_count=excluded.receipt_count,changed=excluded.changed",
);

pub(super) const PUT_MEMBERSHIPS: &str = concat!(
    "INSERT INTO memberships(account,email_id,mailbox_id,changed) VALUES(?1,?2,?3,",
    "?4) ON CONFLICT(account,email_id,mailbox_id) DO UPDATE SET changed=excluded.changed",
);

pub(super) const PUT_KEYWORDS: &str = concat!(
    "INSERT INTO keywords(account,email_id,keyword,changed) VALUES(?1,?2,?3,",
    "?4) ON CONFLICT(account,email_id,keyword) DO UPDATE SET changed=excluded.changed",
);

pub(super) const PUT_THREADS: &str = concat!(
    "INSERT INTO threads(account,id,changed) VALUES(?1,?2,?3) ON CONFLICT(account,",
    "id) DO UPDATE SET changed=excluded.changed",
);

pub(super) const PUT_THREAD_ANCHORS: &str = concat!(
    "INSERT INTO thread_anchors(account,message_id,email_id,changed) VALUES(?1,?2,?3,",
    "?4) ON CONFLICT(account,message_id,email_id) DO UPDATE SET changed=excluded.changed",
);

pub(super) const PUT_SUBMISSIONS: &str = concat!(
    "INSERT INTO submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,",
    "reverse_path,send_at,expires_at,recipient_count,completed_at,notification,",
    "notification_email_id,changed) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,",
    "?14) ON CONFLICT(account,id) DO UPDATE SET email_id=excluded.email_id,",
    "thread_id=excluded.thread_id,identity_id=excluded.identity_id,",
    "transmitted_blob_id=excluded.transmitted_blob_id,reverse_path=excluded.reverse_path,",
    "send_at=excluded.send_at,expires_at=excluded.expires_at,",
    "recipient_count=excluded.recipient_count,completed_at=excluded.completed_at,",
    "notification=excluded.notification,notification_email_id=excluded.notification_email_id,",
    "changed=excluded.changed",
);

pub(super) const PUT_RECIPIENTS: &str = concat!(
    "INSERT INTO recipients(account,submission_id,ordinal,address,state,uncertain,attempt_id,",
    "attempt_count,last_attempt_at,phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,",
    "changed) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) ON CONFLICT(account,",
    "submission_id,ordinal) DO UPDATE SET address=excluded.address,state=excluded.state,",
    "uncertain=excluded.uncertain,attempt_id=excluded.attempt_id,",
    "attempt_count=excluded.attempt_count,last_attempt_at=excluded.last_attempt_at,",
    "phase=excluded.phase,next_attempt_at=excluded.next_attempt_at,rcpt_reply=excluded.rcpt_reply,",
    "data_reply=excluded.data_reply,reason=excluded.reason,diagnostic=excluded.diagnostic,",
    "changed=excluded.changed",
);

pub(super) const PUT_LEASES: &str = concat!(
    "INSERT INTO leases(account,blob_id,device_id,expires_at,uses,changed) VALUES(?1,?2,?3,?4,?5,",
    "?6) ON CONFLICT(account,blob_id) DO UPDATE SET device_id=excluded.device_id,",
    "expires_at=excluded.expires_at,uses=excluded.uses,changed=excluded.changed",
);

pub(super) const PUT_IMPORTS: &str = concat!(
    "INSERT INTO imports(account,source_instance,source_kind,source_account,source_object,",
    "local_object,historical_blob,source_digest,changed) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,",
    "?9) ON CONFLICT(account,source_instance,source_kind,source_account,",
    "source_object) DO UPDATE SET local_object=excluded.local_object,",
    "historical_blob=excluded.historical_blob,source_digest=excluded.source_digest,",
    "changed=excluded.changed",
);

pub(super) const INSERT_RECEIPT: &str = concat!(
    "INSERT INTO smtp_receipt_recipients(account,email_id,ordinal,address) ",
    "VALUES(?1,?2,?3,?4)"
);
