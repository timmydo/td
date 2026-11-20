-- Inspect plain, non-inherited tables. Read every row with the independent auditor.
WITH required(name) AS (
    VALUES ('schema_migrations'), ('accounts'), ('mailboxes'), ('emails'),
           ('blobs'), ('blob_chunks')
)
SELECT
    NOT EXISTS (
        SELECT FROM required r WHERE NOT EXISTS (
            SELECT FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'td_mta' AND c.relname = r.name AND c.relkind = 'r'
              AND NOT c.relrowsecurity AND NOT c.relhassubclass
        )
    )
    AND NOT EXISTS (SELECT FROM ONLY td_mta.accounts WHERE id IS NULL OR octet_length(id) <> 16)
    AND NOT EXISTS (SELECT FROM ONLY td_mta.mailboxes m WHERE NOT EXISTS
                    (SELECT FROM ONLY td_mta.accounts a WHERE a.id = m.account))
    AND NOT EXISTS (SELECT FROM ONLY td_mta.blobs b WHERE NOT EXISTS
                    (SELECT FROM ONLY td_mta.accounts a WHERE a.id = b.account))
    AND NOT EXISTS (SELECT FROM ONLY td_mta.emails e WHERE NOT EXISTS
                    (SELECT FROM ONLY td_mta.blobs b WHERE b.account = e.account AND b.id = e.blob_id));
