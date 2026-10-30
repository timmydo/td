SELECT id, received_at, changed, membership_changed
FROM (
SELECT e.id, e.received_at, e.changed, m.changed AS membership_changed
FROM td_mta.emails e
JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id
JOIN td_mta.blob_search s ON s.account=b.account AND s.blob_id=b.id
LEFT JOIN td_mta.memberships m
    ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2
WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL)
    AND ($5::bigint IS NULL OR e.received_at >= $5)
    AND ($6::bigint IS NULL OR e.received_at < $6)
    AND s.version=$8 AND s.body_length=b.length AND s.body_digest=b.digest
    AND s.candidate_text LIKE $7 ESCAPE E'\\'
UNION ALL
SELECT e.id, e.received_at, e.changed, m.changed AS membership_changed
FROM td_mta.emails e
JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id
LEFT JOIN td_mta.memberships m
    ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2
WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL)
    AND ($5::bigint IS NULL OR e.received_at >= $5)
    AND ($6::bigint IS NULL OR e.received_at < $6)
    AND NOT EXISTS (
        SELECT 1 FROM td_mta.blob_search s
        WHERE s.account=b.account AND s.blob_id=b.id AND s.version=$8
            AND s.body_length=b.length AND s.body_digest=b.digest
            AND s.candidate_text IS NOT NULL)
) candidates
ORDER BY received_at ASC, id ASC
LIMIT $3 OFFSET $4
