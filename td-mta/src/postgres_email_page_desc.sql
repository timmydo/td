SELECT e.id, e.received_at, e.changed, m.changed
FROM td_mta.emails e
LEFT JOIN td_mta.memberships m
    ON m.account=e.account AND m.email_id=e.id AND m.mailbox_id=$2
WHERE e.account=$1 AND ($2::bytea IS NULL OR m.email_id IS NOT NULL)
    AND ($5::bigint IS NULL OR e.received_at >= $5)
    AND ($6::bigint IS NULL OR e.received_at < $6)
ORDER BY e.received_at DESC, e.id ASC
LIMIT $3 OFFSET $4
