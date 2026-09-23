//! Checked startup reservations, not allocation or measured RSS guarantees.
use std::fmt;

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * KIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceError {
    OutOfRange {
        field: &'static str,
        min: usize,
        max: usize,
    },
    Inconsistent(&'static str),
    /// Defensive arithmetic guard; current validated profile bounds fit usize.
    Overflow(&'static str),
    MemoryBudget {
        required: usize,
        available: usize,
    },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { field, min, max } => write!(f, "{field} must be in {min}..={max}"),
            Self::Inconsistent(rule) => write!(f, "inconsistent limits: {rule}"),
            Self::Overflow(component) => write!(f, "resource arithmetic overflow: {component}"),
            Self::MemoryBudget {
                required,
                available,
            } => write!(
                f,
                "memory reservation {required} exceeds budget {available}"
            ),
        }
    }
}

impl std::error::Error for ResourceError {}

macro_rules! limits {
    ($($field:ident: $default:expr, $min:expr, $max:expr;)+) => {
        /// Untrusted configuration values. Use `plan` before allocating resources.
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct Limits { $(pub $field: usize,)+ }

        impl Default for Limits {
            fn default() -> Self { Self { $($field: $default,)+ } }
        }

        impl Limits {
            fn validate_ranges(&self) -> Result<(), ResourceError> {
                $(if !($min..=$max).contains(&self.$field) {
                    return Err(ResourceError::OutOfRange { field: stringify!($field), min: $min, max: $max });
                })+
                Ok(())
            }
        }
    };
}

limits! {
    smtp_sessions: 8, 1, 32;
    smtp_per_peer: 2, 1, 32;
    https_connections: 8, 1, 32;
    tls_handshakes: 2, 1, 8;
    event_streams: 2, 0, 8;
    outbound_deliveries: 1, 1, 4;
    body_jobs: 2, 1, 8;
    storage_views: 2, 1, 8;
    message_bytes: 32 * MIB, 1, 128 * MIB;
    header_bytes: 256 * KIB, 1, MIB;
    mime_depth: 32, 1, 64;
    mime_parts: 1024, 1, 4096;
    smtp_recipients: 100, 100, 1000;
    json_bytes: MIB, 1, 4 * MIB;
    json_methods: 16, 1, 64;
    json_depth: 32, 1, 64;
    json_tokens: 32768, 1, 131072;
    objects_per_method: 256, 1, 1024;
    query_page: 256, 1, 1024;
    index_cache_bytes: 8 * MIB, 1, 32 * MIB;
    journal_bytes: 4 * MIB, 4 * MIB, 4 * MIB;
    journal_operations: 8192, 8192, 8192;
    frame_bytes: MIB, MIB, MIB;
    frame_operations: 4096, 4096, 4096;
    upload_disk_bytes: 128 * MIB, 1, 1024 * MIB;
    upload_expiry_seconds: 86400, 1, 604800;
    queue_disk_bytes: 256 * MIB, 1, 1024 * MIB;
    queue_submissions: 1000, 1, 10000;
    sort_disk_bytes: 64 * MIB, 1, 256 * MIB;
    log_file_bytes: 8 * MIB, 1, 64 * MIB;
    retained_logs: 4, 1, 16;
    memory_budget_bytes: 64 * MIB, 1, 128 * MIB;
}

/// One separately accounted reservation. Sizes include each slot's overhead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reservation {
    pub name: &'static str,
    pub count: usize,
    pub bytes_each: usize,
}

impl Reservation {
    pub fn bytes(self) -> Result<usize, ResourceError> {
        self.count
            .checked_mul(self.bytes_each)
            .ok_or(ResourceError::Overflow(self.name))
    }
}

/// Every entry is reserved at peak overlap; none is a mailbox-sized map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourcePlan {
    limits: Limits,
    reservations: [Reservation; 18],
    total_bytes: usize,
    log_disk_bytes: usize,
}

impl ResourcePlan {
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn reservations(&self) -> &[Reservation] {
        &self.reservations
    }
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
    pub fn log_disk_bytes(&self) -> usize {
        self.log_disk_bytes
    }
}

fn sum(name: &'static str, values: &[usize]) -> Result<usize, ResourceError> {
    values.iter().try_fold(0usize, |total, n| {
        total.checked_add(*n).ok_or(ResourceError::Overflow(name))
    })
}

fn product(name: &'static str, count: usize, bytes: usize) -> Result<usize, ResourceError> {
    Reservation {
        name,
        count,
        bytes_each: bytes,
    }
    .bytes()
}

impl Limits {
    pub fn plan(self) -> Result<ResourcePlan, ResourceError> {
        self.validate_ranges()?;
        for (valid, rule) in [
            (
                self.smtp_per_peer <= self.smtp_sessions,
                "per-peer SMTP cap exceeds session pool",
            ),
            (
                self.event_streams < self.https_connections,
                "event streams must leave an HTTPS slot",
            ),
            (
                self.header_bytes <= self.message_bytes,
                "headers exceed message limit",
            ),
            (
                self.json_depth <= self.json_tokens,
                "JSON depth exceeds token capacity",
            ),
            (
                self.mime_depth <= self.mime_parts,
                "MIME depth exceeds part capacity",
            ),
            (
                self.json_methods <= self.json_tokens,
                "method count exceeds token capacity",
            ),
            (
                self.upload_disk_bytes >= self.message_bytes,
                "upload quota cannot fit one blob",
            ),
            (
                self.queue_disk_bytes >= self.message_bytes,
                "queue quota cannot fit one message",
            ),
        ] {
            if !valid {
                return Err(ResourceError::Inconsistent(rule));
            }
        }

        let sessions = sum(
            "TLS session count",
            &[
                self.smtp_sessions,
                self.https_connections,
                self.outbound_deliveries,
            ],
        )?;
        if self.tls_handshakes > sessions {
            return Err(ResourceError::Inconsistent(
                "handshakes exceed transport slots",
            ));
        }
        let json_descriptors = product("JSON tokens", self.json_tokens, 16)?;
        let mime_descriptors = product("MIME parts", self.mime_parts, 64)?;
        let recipient_bytes = product("SMTP recipients", self.smtp_recipients, 320)?;
        let journal_descriptors = product("journal descriptors", self.journal_operations, 32)?;
        let response_ids = product(
            "method object scratch",
            self.objects_per_method.max(self.query_page),
            128,
        )?;

        let reservations = [
            Reservation {
                name: "smtp slots",
                count: self.smtp_sessions,
                bytes_each: sum(
                    "smtp slot",
                    &[self.header_bytes, recipient_bytes, 64 * KIB, 16 * KIB],
                )?,
            },
            Reservation {
                name: "https slots",
                count: self.https_connections,
                bytes_each: sum(
                    "https slot",
                    &[self.json_bytes, json_descriptors, response_ids, 96 * KIB],
                )?,
            },
            Reservation {
                name: "body jobs",
                count: self.body_jobs,
                bytes_each: sum("body job", &[self.header_bytes, mime_descriptors, 96 * KIB])?,
            },
            Reservation {
                name: "read views",
                count: self.storage_views,
                bytes_each: sum(
                    "read view",
                    &[self.journal_bytes, journal_descriptors, 128 * KIB],
                )?,
            },
            Reservation {
                name: "writer/checkpoint",
                count: 1,
                bytes_each: sum(
                    "writer/checkpoint",
                    &[
                        self.journal_bytes,
                        journal_descriptors,
                        self.frame_bytes,
                        256 * KIB,
                    ],
                )?,
            },
            Reservation {
                name: "index cache",
                count: 1,
                bytes_each: self.index_cache_bytes,
            },
            Reservation {
                name: "outbound slots",
                count: self.outbound_deliveries,
                bytes_each: sum("outbound slot", &[recipient_bytes, 128 * KIB])?,
            },
            Reservation {
                name: "sort run and merge buffers",
                count: 1,
                bytes_each: MIB,
            },
            Reservation {
                name: "log queue and formatting",
                count: 1,
                bytes_each: 128 * KIB,
            },
            Reservation {
                name: "DNS/ACME/control scratch",
                count: 1,
                bytes_each: 512 * KIB,
            },
            Reservation {
                name: "slot queues and queue window",
                count: 1,
                bytes_each: 128 * KIB,
            },
            Reservation {
                name: "worker stacks",
                count: 8,
                bytes_each: 256 * KIB,
            },
            Reservation {
                name: "main stack allowance",
                count: 1,
                bytes_each: MIB,
            },
            Reservation {
                name: "TLS sessions",
                count: sessions,
                bytes_each: 128 * KIB,
            },
            Reservation {
                name: "TLS handshakes",
                count: self.tls_handshakes,
                bytes_each: MIB,
            },
            Reservation {
                name: "certificate generations",
                count: 2,
                bytes_each: MIB,
            },
            Reservation {
                name: "cold reload overlap",
                count: 1,
                bytes_each: 2 * MIB,
            },
            Reservation {
                name: "process and allocator allowance",
                count: 1,
                bytes_each: 8 * MIB,
            },
        ];
        let total_bytes = reservations.iter().try_fold(0usize, |total, entry| {
            total
                .checked_add(entry.bytes()?)
                .ok_or(ResourceError::Overflow("total memory"))
        })?;
        if total_bytes > self.memory_budget_bytes {
            return Err(ResourceError::MemoryBudget {
                required: total_bytes,
                available: self.memory_budget_bytes,
            });
        }
        let log_count = sum("log generations", &[self.retained_logs, 1])?;
        let log_disk_bytes = product("log disk bytes", log_count, self.log_file_bytes)?;
        Ok(ResourcePlan {
            limits: self,
            reservations,
            total_bytes,
            log_disk_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ledger_pins_documented_budget() -> Result<(), ResourceError> {
        let plan = Limits::default().plan()?;
        assert_eq!(plan.total_bytes(), 62_874_880);
        assert!(plan.total_bytes() < 64 * MIB);
        assert_eq!(plan.log_disk_bytes(), 40 * MIB);
        Ok(())
    }

    #[test]
    fn bad_configuration_refuses_before_any_allocation() {
        let limits = Limits {
            smtp_sessions: 0,
            ..Limits::default()
        };
        assert!(matches!(
            limits.plan(),
            Err(ResourceError::OutOfRange {
                field: "smtp_sessions",
                ..
            })
        ));
        let limits = Limits {
            json_tokens: usize::MAX,
            ..Limits::default()
        };
        assert!(matches!(
            limits.plan(),
            Err(ResourceError::OutOfRange {
                field: "json_tokens",
                ..
            })
        ));
        let limits = Limits {
            smtp_per_peer: 9,
            ..Limits::default()
        };
        assert!(matches!(limits.plan(), Err(ResourceError::Inconsistent(_))));
        let limits = Limits {
            event_streams: 8,
            ..Limits::default()
        };
        assert!(matches!(limits.plan(), Err(ResourceError::Inconsistent(_))));
        let limits = Limits {
            memory_budget_bytes: MIB,
            ..Limits::default()
        };
        assert!(matches!(
            limits.plan(),
            Err(ResourceError::MemoryBudget { .. })
        ));
    }

    #[test]
    fn arithmetic_overflow_is_an_error() {
        assert_eq!(
            product("test", usize::MAX, 2),
            Err(ResourceError::Overflow("test"))
        );
        assert_eq!(
            sum("test", &[usize::MAX, 1]),
            Err(ResourceError::Overflow("test"))
        );
    }

    #[test]
    fn incompatible_capacities_are_rejected() {
        let defaults = Limits::default();
        let cases = [
            (
                Limits {
                    message_bytes: 1,
                    ..defaults
                },
                "headers exceed message limit",
            ),
            (
                Limits {
                    json_tokens: 1,
                    ..defaults
                },
                "JSON depth exceeds token capacity",
            ),
            (
                Limits {
                    json_depth: 1,
                    json_tokens: 1,
                    ..defaults
                },
                "method count exceeds token capacity",
            ),
            (
                Limits {
                    mime_parts: 1,
                    ..defaults
                },
                "MIME depth exceeds part capacity",
            ),
            (
                Limits {
                    upload_disk_bytes: 1,
                    ..defaults
                },
                "upload quota cannot fit one blob",
            ),
            (
                Limits {
                    queue_disk_bytes: 1,
                    ..defaults
                },
                "queue quota cannot fit one message",
            ),
            (
                Limits {
                    smtp_sessions: 1,
                    smtp_per_peer: 1,
                    https_connections: 1,
                    event_streams: 0,
                    tls_handshakes: 4,
                    ..defaults
                },
                "handshakes exceed transport slots",
            ),
        ];
        for (limits, rule) in cases {
            assert_eq!(limits.plan(), Err(ResourceError::Inconsistent(rule)));
        }
    }

    #[test]
    fn streamed_body_and_disk_quotas_do_not_allocate_message_sized_slots(
    ) -> Result<(), ResourceError> {
        let original = Limits::default().plan()?;
        let changed = Limits {
            message_bytes: 64 * MIB,
            upload_disk_bytes: 256 * MIB,
            queue_submissions: 2000,
            ..Limits::default()
        }
        .plan()?;
        assert_eq!(original.total_bytes(), changed.total_bytes());
        Ok(())
    }

    #[test]
    fn larger_pool_cannot_keep_default_memory_claim() -> Result<(), ResourceError> {
        let limits = Limits {
            https_connections: 32,
            ..Limits::default()
        };
        assert!(matches!(
            limits.plan(),
            Err(ResourceError::MemoryBudget { .. })
        ));
        let plan = Limits {
            memory_budget_bytes: 128 * MIB,
            ..limits
        }
        .plan()?;
        assert!(plan.total_bytes() > 64 * MIB);
        Ok(())
    }
}
