//! What moving a td-mta cluster across each PostgreSQL 17 minor release
//! requires, from each release's "Migration to Version 17.N" notes and the
//! changelog entries they name (https://www.postgresql.org/docs/release/17.N/).
//! This table replaces an operator's reading of the release notes: the
//! binary switch evaluates every note of every release after the live
//! server's up to td's, and refuses a release the table does not list until
//! a reviewed change extends it.
use crate::{common::require, R};

/// One release's notes: what its migration section names, and changelog
/// entries an update may concern. A release with neither has none.
pub struct Minor {
    pub minor: u32,
    pub notes: &'static [Note],
}

pub struct Note {
    /// A fixed name for refusals and the receipt.
    pub id: &'static str,
    pub check: Check,
}

pub enum Check {
    /// A count, in every database that takes connections, as the trial
    /// copy's superuser, of the objects the note concerns; it must be 0.
    Database(&'static str),
    /// A count once for the cluster, through the live server's owner login;
    /// it must be 0.
    Cluster(&'static str),
    /// Rebuild every GIN index. The mail database's are rebuilt on the
    /// trial and then on production under td's server before writers
    /// return; a GIN index in another database refuses.
    ReindexGin,
    /// Nothing a td-mta cluster holds or a td-mta tool does is concerned,
    /// for the reason given.
    Exempt(&'static str),
}

/// The count of a database's indexes with an operator class of access
/// method `$am` that meets `$opclass`, a condition on `o.opcname`.
macro_rules! opclass_indexes {
    ($am:literal, $opclass:literal) => {
        concat!(
            "SELECT count(*) FROM pg_catalog.pg_index i ",
            "JOIN pg_catalog.pg_opclass o ON o.oid = ANY (i.indclass::pg_catalog.oid[]) ",
            "JOIN pg_catalog.pg_am a ON a.oid = o.opcmethod ",
            "WHERE a.amname = '",
            $am,
            "' AND ",
            $opclass
        )
    };
}

/// 17.1, fifth changelog entry: the release notes' own query for
/// foreign keys whose catalog state a partition detach or attach broke.
const DETACHED_FOREIGN_KEYS: &str = "SELECT count(*) FROM pg_catalog.pg_constraint c \
     WHERE contype = 'f' AND conparentid = 0 AND \
     (SELECT count(*) FROM pg_catalog.pg_constraint c2 WHERE c2.conparentid = c.oid) <> \
     ((SELECT count(*) FROM pg_catalog.pg_inherits i \
       WHERE (i.inhparent = c.conrelid OR i.inhparent = c.confrelid) AND \
       EXISTS (SELECT 1 FROM pg_catalog.pg_partitioned_table WHERE partrelid = i.inhparent)) + \
      CASE WHEN pg_catalog.pg_partition_root(conrelid) = confrelid THEN \
        (SELECT count(*) FROM pg_catalog.pg_partition_tree(confrelid) WHERE level = 1) \
      ELSE 0 END)";

pub const MINORS: &[Minor] = &[
    Minor {
        minor: 1,
        notes: &[
            Note {
                id: "17.1-foreign-key-partition-detach",
                check: Check::Database(DETACHED_FOREIGN_KEYS),
            },
            // Sixth entry: with libc, a database whose LC_CTYPE is C while
            // LC_COLLATE is not may hold corrupt text indexes.
            Note {
                id: "17.1-ctype-c-collate-other",
                check: Check::Cluster(
                    "SELECT count(*) FROM pg_catalog.pg_database \
                     WHERE datlocprovider = 'c' AND datctype IN ('C', 'POSIX') \
                     AND datcollate NOT IN ('C', 'POSIX')",
                ),
            },
            Note {
                id: "17.1-ctype-c-collate-other-collation",
                check: Check::Database(
                    "SELECT count(*) FROM pg_catalog.pg_collation c \
                     WHERE c.collprovider = 'c' AND c.collctype IN ('C', 'POSIX') \
                     AND c.collcollate NOT IN ('C', 'POSIX') \
                     AND (EXISTS (SELECT FROM pg_catalog.pg_attribute a \
                                  WHERE a.attcollation = c.oid AND NOT a.attisdropped) \
                          OR EXISTS (SELECT FROM pg_catalog.pg_index i \
                                     WHERE c.oid = ANY (i.indcollation::pg_catalog.oid[])))",
                ),
            },
            Note {
                id: "17.1-windows-locale-names",
                check: Check::Exempt("Windows locale names only"),
            },
        ],
    },
    Minor {
        minor: 2,
        notes: &[],
    },
    Minor {
        minor: 3,
        notes: &[Note {
            id: "17.3-earthdistance-pg-upgrade",
            check: Check::Exempt("an optional extension update, which td-mta does not install"),
        }],
    },
    Minor {
        minor: 4,
        notes: &[],
    },
    Minor {
        minor: 5,
        notes: &[
            // Second entry: self-referential foreign keys on partitioned
            // tables may not be enforced.
            Note {
                id: "17.5-partitioned-self-reference-foreign-key",
                check: Check::Database(
                    "SELECT count(*) FROM pg_catalog.pg_constraint c \
                     JOIN pg_catalog.pg_class r ON r.oid = c.conrelid \
                     WHERE c.contype = 'f' AND c.conrelid = c.confrelid AND r.relkind = 'p'",
                ),
            },
            // Third entry: BRIN bloom summaries may have lost rows.
            Note {
                id: "17.5-brin-bloom",
                check: Check::Database(opclass_indexes!("brin", "o.opcname LIKE '%bloom_ops'")),
            },
        ],
    },
    Minor {
        minor: 6,
        notes: &[
            // Fourth entry.
            Note {
                id: "17.6-brin-numeric-minmax-multi",
                check: Check::Database(opclass_indexes!(
                    "brin",
                    "o.opcname = 'numeric_minmax_multi_ops'"
                )),
            },
            Note {
                id: "17.6-pg-upgrade-not-null",
                check: Check::Exempt("pg_upgrade only: the switch keeps the data directory"),
            },
        ],
    },
    Minor {
        minor: 7,
        notes: &[],
    },
    Minor {
        minor: 8,
        notes: &[Note {
            id: "17.8-file-extend-method",
            check: Check::Exempt("a new optional setting whose default needs nothing"),
        }],
    },
    Minor {
        minor: 9,
        notes: &[],
    },
    Minor {
        minor: 10,
        notes: &[Note {
            id: "17.10-attach-partition-index",
            check: Check::Exempt("a new corrective command, nothing to apply"),
        }],
    },
    Minor {
        minor: 11,
        notes: &[
            // First security entry: logical decoding admits only listed
            // output plugins.
            Note {
                id: "17.11-output-plugin-libraries",
                check: Check::Cluster(
                    "SELECT count(*) FROM pg_catalog.pg_replication_slots \
                     WHERE slot_type = 'logical' AND plugin NOT IN ('pgoutput', 'test_decoding')",
                ),
            },
            // Second: pgcrypto data an unsupported cipher left unencrypted.
            Note {
                id: "17.11-pgcrypto-cipher",
                check: Check::Database(
                    "SELECT count(*) FROM pg_catalog.pg_extension WHERE extname = 'pgcrypto'",
                ),
            },
            Note {
                id: "17.11-psql-copy-script",
                check: Check::Exempt(
                    "psql scripts only: td-mta's tools give psql no COPY ... FROM STDIN",
                ),
            },
            Note {
                id: "17.11-btree-gist-float",
                check: Check::Database(opclass_indexes!(
                    "gist",
                    "o.opcname IN ('gist_float4_ops', 'gist_float8_ops')"
                )),
            },
            Note {
                id: "17.11-ltree-btree",
                check: Check::Database(opclass_indexes!("btree", "o.opcname = 'ltree_ops'")),
            },
        ],
    },
    Minor {
        minor: 12,
        notes: &[
            // De-duplicated btree entries on unconstrained bpchar.
            Note {
                id: "17.12-bpchar-btree",
                check: Check::Database(
                    "SELECT count(*) FROM pg_catalog.pg_index i \
                     JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
                     JOIN pg_catalog.pg_am a ON a.oid = ic.relam \
                     JOIN pg_catalog.pg_attribute att ON att.attrelid = i.indexrelid \
                     JOIN pg_catalog.pg_opclass o ON o.oid = i.indclass[att.attnum - 1] \
                     WHERE a.amname = 'btree' AND att.atttypmod < 0 \
                     AND o.opcname IN ('bpchar_ops', 'bpchar_pattern_ops')",
                ),
            },
            // VACUUM could leave dead GIN entries, and a posting tree page
            // deletion after an incomplete split could disconnect pages: a
            // GIN index may miss or misdirect rows, which nothing can check.
            Note {
                id: "17.12-gin-vacuum-and-page-deletion",
                check: Check::ReindexGin,
            },
            Note {
                id: "17.12-toast-index-reindex-concurrently",
                check: Check::Exempt(
                    "after REINDEX CONCURRENTLY of a TOAST index only, which no td-mta tool, migration or runbook runs",
                ),
            },
            Note {
                id: "17.12-composite-type-extensions",
                check: Check::Exempt(
                    "rebuilds C-language extensions, which td builds with the server",
                ),
            },
            Note {
                id: "17.12-pg-trgm-gist",
                check: Check::Database(opclass_indexes!("gist", "o.opcname = 'gist_trgm_ops'")),
            },
            // Incremental backups need WAL summaries.
            Note {
                id: "17.12-incremental-backup",
                check: Check::Cluster(
                    "SELECT count(*) FROM pg_catalog.pg_settings \
                     WHERE name = 'summarize_wal' AND setting <> 'off'",
                ),
            },
        ],
    },
];

/// The bytes of the database's GIN indexes.
pub const GIN_BYTES: &str = "SELECT coalesce(sum(pg_catalog.pg_relation_size(c.oid)), 0) \
     FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_am a ON a.oid = c.relam WHERE a.amname = 'gin' AND c.relkind = 'i'";

/// The database's GIN indexes.
pub const GIN_INDEXES: &str = "SELECT count(*) FROM pg_catalog.pg_class c \
     JOIN pg_catalog.pg_am a ON a.oid = c.relam WHERE a.amname = 'gin' AND c.relkind = 'i'";

/// Rebuild every GIN index of the database in one transaction, then count
/// them.
pub const REINDEX_GIN: &str = "SET statement_timeout = '600s';
DO $$
DECLARE r record;
BEGIN
  FOR r IN SELECT format('%I.%I', n.nspname, c.relname) AS name FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_am a ON a.oid = c.relam
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           WHERE a.amname = 'gin' AND c.relkind = 'i' LOOP
    EXECUTE 'REINDEX INDEX ' || r.name;
  END LOOP;
END
$$;
SELECT count(*) FROM pg_catalog.pg_class c
  JOIN pg_catalog.pg_am a ON a.oid = c.relam WHERE a.amname = 'gin' AND c.relkind = 'i';
";

/// The notes of every release after `live` up to `td`, refusing a move
/// backwards and a release the table does not list.
pub fn span(live: u32, td: u32) -> R<impl Iterator<Item = (u32, &'static Note)>> {
    movable(live, td)?;
    Ok(MINORS
        .iter()
        .filter(move |m| m.minor > live && m.minor <= td)
        .flat_map(|m| m.notes.iter().map(move |note| (m.minor, note))))
}

/// The switch may move from 17.`live` to 17.`td`: forwards, and within
/// the table.
pub fn movable(live: u32, td: u32) -> R<()> {
    require(
        live <= td,
        "the live PostgreSQL is newer than the td-built one",
    )?;
    covered(td)
}

/// The table lists every release from 17.1 up to `minor`.
pub fn covered(minor: u32) -> R<()> {
    require(
        MINORS
            .iter()
            .enumerate()
            .all(|(i, m)| {
                u32::try_from(i).ok().and_then(|i| i.checked_add(1)) == Some(m.minor)
            })
            && MINORS.last().is_some_and(|m| m.minor >= minor),
        "the PostgreSQL release is not in td-mta-deploy's reviewed minor-release table (postgres_minors.rs): extend it in a reviewed change",
    )
}

#[cfg(test)]
#[path = "tests/minors.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/minors_pg.rs"]
mod postgres_test;
