-- Every immutable version has fewer than 256 ordinary, ASCII-named tables.
-- Reject separators in names so the LF-delimited encoding is unambiguous.
SELECT CASE WHEN count(*) <= 256
             AND bool_and(relkind = 'r' AND relname COLLATE "C" !~ '[^a-z0-9_]')
       THEN encode(sha256(convert_to(string_agg(relname::text || chr(10), ''
                     ORDER BY relname COLLATE "C"), 'UTF8')), 'hex')
       END
FROM (SELECT c.relname, c.relkind
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = 'td_mta' AND c.relkind IN ('r', 'f', 'p')
      LIMIT 257) AS tables;
