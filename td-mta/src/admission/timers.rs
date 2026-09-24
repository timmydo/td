//! Checked policy budgets. Protocol state machines own timer transitions.
use super::{add, mul, widen, Error, Plan};
use crate::ports::{Deadline, Tick};

pub const SESSION_SECONDS: u64 = 3600;
pub const CONNECTION_OPERATIONS: u64 = 100;
pub const ACME_SECONDS: u64 = 60;
pub const HTTP01_SECONDS: u64 = 5;
pub const DATA_BLOCK_BYTES: u64 = 16384;

settings! { NetworkLimits {
    handshake_seconds: 10, 10, 160;
    dns_seconds: 5, 5, 80;
    dial_seconds: 10, 10, 160;
    header_idle_seconds: 15, 15, 240;
    header_seconds: 30, 30, 480;
    body_idle_seconds: 15, 15, 240;
    response_idle_seconds: 15, 15, 240;
    keepalive_seconds: 15, 15, 240;
    event_stall_seconds: 15, 15, 240;
    smtp_idle_seconds: 300, 300, 4800;
    smtp_data_seconds: 1800, 1800, 28800;
    command_seconds: 300, 300, 4800;
    data_init_seconds: 120, 120, 1920;
    data_block_seconds: 180, 180, 2880;
    final_reply_seconds: 600, 600, 9600;
    migration_idle_seconds: 15, 15, 240;
    migration_minimum_seconds: 1800, 1800, 28800;
    transfer_minimum_seconds: 120, 120, 1920;
    transfer_base_seconds: 30, 30, 480;
    minimum_rate: 65536, 4096, 65536;
} }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeoutPlan {
    limits: NetworkLimits,
    execution_seconds: u64,
    final_commit_seconds: u64,
}

impl NetworkLimits {
    pub fn plan(self, admission: &Plan) -> Result<TimeoutPlan, Error> {
        self.validate()?;
        let work = admission.work();
        let final_commit_seconds = add(
            add(
                work.checkpoint_seconds,
                mul(2, work.commit_seconds, "final commit")?,
                "final commit",
            )?,
            work.admission_seconds,
            "final commit",
        )?;
        Ok(TimeoutPlan {
            limits: self,
            execution_seconds: work.request_seconds,
            final_commit_seconds,
        })
    }
}

fn ceil_div(value: u64, divisor: u64) -> Result<u64, Error> {
    let whole = value
        .checked_div(divisor)
        .ok_or(Error::Inconsistent("zero transfer divisor"))?;
    let remainder = value
        .checked_rem(divisor)
        .ok_or(Error::Inconsistent("zero transfer divisor"))?;
    add(whole, u64::from(remainder != 0), "ceiling division")
}

/// Checked conversion from seconds to the monotonic millisecond domain.
/// The caller takes the minimum with every enclosing lease.
pub fn deadline(now: Tick, seconds: u64) -> Result<Deadline, Error> {
    Deadline::after(now, mul(seconds, 1000, "deadline milliseconds")?)
        .map_err(|_| Error::Overflow("deadline tick"))
}

impl TimeoutPlan {
    pub fn limits(&self) -> &NetworkLimits {
        &self.limits
    }
    pub fn execution_seconds(&self) -> u64 {
        self.execution_seconds
    }
    pub fn final_commit_seconds(&self) -> u64 {
        self.final_commit_seconds
    }

    pub fn http_transfer_seconds(&self, bytes: u64) -> Result<u64, Error> {
        Ok(self.limits.transfer_minimum_seconds.max(add(
            self.limits.transfer_base_seconds,
            ceil_div(bytes, self.limits.minimum_rate)?,
            "HTTP transfer",
        )?))
    }
    /// Set once at admission; phase progress must not renew this budget.
    pub fn http_exchange_seconds(&self, body_max: u64, response_max: u64) -> Result<u64, Error> {
        add(
            add(
                self.limits.header_seconds,
                self.http_transfer_seconds(body_max)?,
                "HTTP exchange",
            )?,
            add(
                self.execution_seconds,
                self.http_transfer_seconds(response_max)?,
                "HTTP exchange",
            )?,
            "HTTP exchange",
        )
    }
    pub fn migration_transfer_seconds(&self, bytes: u64) -> Result<u64, Error> {
        Ok(self
            .limits
            .migration_minimum_seconds
            .max(self.http_transfer_seconds(bytes)?))
    }
    /// Wire length is counted over the immutable file, including dot stuffing
    /// and terminator. This does not perform the required preflight scan.
    pub fn smtp_attempt_seconds(&self, recipients: u64, wire_bytes: u64) -> Result<u64, Error> {
        let max = widen(
            crate::limits::OUTBOUND_RECIPIENT_BATCH,
            "attempt recipients",
        )?;
        if !(1..=max).contains(&recipients) {
            return Err(Error::Range {
                field: "attempt recipients",
                min: 1,
                max,
            });
        }
        let l = self.limits;
        let mut total = add(
            add(l.dns_seconds, l.dial_seconds, "SMTP attempt")?,
            l.handshake_seconds,
            "SMTP attempt",
        )?;
        for budget in [
            mul(
                add(16, recipients, "SMTP commands")?,
                l.command_seconds,
                "SMTP commands",
            )?,
            l.data_init_seconds,
            mul(
                ceil_div(wire_bytes, DATA_BLOCK_BYTES)?.max(1),
                l.data_block_seconds,
                "SMTP blocks",
            )?,
            l.final_reply_seconds,
            mul(4, self.final_commit_seconds, "SMTP commits")?,
        ] {
            total = add(total, budget, "SMTP attempt")?;
        }
        Ok(total)
    }
    /// Minimum lease remaining before requesting AcceptancePossible. The
    /// scheduler must also hold capacity and prioritize fence/outcome work.
    pub fn smtp_final_seconds(&self) -> Result<u64, Error> {
        add(
            mul(2, self.final_commit_seconds, "SMTP final")?,
            add(
                self.limits.data_block_seconds,
                self.limits.final_reply_seconds,
                "SMTP final",
            )?,
            "SMTP final",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        limits::Limits,
    };
    fn plan(limits: NetworkLimits) -> Result<TimeoutPlan, Box<dyn std::error::Error>> {
        Ok(limits.plan(&DiskLimits::default().plan(
            &Limits::default().plan()?,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )?)?)
    }

    #[test]
    fn transfer_rounding_and_outer_exchange_have_numeric_oracles(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(NetworkLimits::default())?;
        assert_eq!(p.http_transfer_seconds(0)?, 120);
        assert_eq!(p.http_transfer_seconds(90 * 65536)?, 120);
        assert_eq!(p.http_transfer_seconds(90 * 65536 + 1)?, 121);
        assert_eq!(p.http_transfer_seconds(32 * 1048576)?, 542);
        assert_eq!(p.http_exchange_seconds(32 * 1048576, 128 * 1048576)?, 2950);
        assert_eq!(p.migration_transfer_seconds(32 * 1048576)?, 1800);
        assert_eq!(p.migration_transfer_seconds(128 * 1048576)?, 2078);
        let slow = plan(NetworkLimits {
            minimum_rate: 4096,
            ..NetworkLimits::default()
        })?;
        assert_eq!(slow.http_transfer_seconds(32 * 1048576)?, 8222);
        assert_eq!(p.http_transfer_seconds(u64::MAX)?, 281474976710686);
        Ok(())
    }

    #[test]
    fn smtp_lease_counts_commands_blocks_and_all_commit_phases(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(NetworkLimits::default())?;
        assert_eq!(p.final_commit_seconds(), 121);
        assert_eq!(p.smtp_final_seconds()?, 1022);
        assert_eq!(p.smtp_attempt_seconds(1, 0)?, 6509);
        assert_eq!(p.smtp_attempt_seconds(1, 16384)?, 6509);
        assert_eq!(p.smtp_attempt_seconds(1, 16385)?, 6689);
        assert_eq!(p.smtp_attempt_seconds(100, 1)?, 36209);
        for count in [0, 101] {
            assert_eq!(
                p.smtp_attempt_seconds(count, 1),
                Err(Error::Range {
                    field: "attempt recipients",
                    min: 1,
                    max: 100
                })
            );
        }
        // Representable seconds can overflow conversion to milliseconds.
        assert!(deadline(Tick(0), p.smtp_attempt_seconds(1, u64::MAX)?).is_err());
        Ok(())
    }

    #[test]
    fn independently_configured_phases_cannot_substitute_equal_defaults(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let p = plan(NetworkLimits {
            handshake_seconds: 11,
            dns_seconds: 7,
            dial_seconds: 13,
            header_seconds: 41,
            transfer_base_seconds: 37,
            transfer_minimum_seconds: 173,
            minimum_rate: 8192,
            command_seconds: 311,
            data_init_seconds: 137,
            data_block_seconds: 191,
            final_reply_seconds: 619,
            migration_minimum_seconds: 1901,
            smtp_data_seconds: 2003,
            ..NetworkLimits::default()
        })?;
        assert_eq!(p.execution_seconds(), 300);
        assert_eq!(p.http_transfer_seconds(1048576)?, 173);
        assert_eq!(p.http_transfer_seconds(2097152)?, 293);
        assert_eq!(p.http_exchange_seconds(1048576, 2097152)?, 807);
        assert_eq!(p.migration_transfer_seconds(1048576)?, 1901);
        assert_eq!(p.migration_transfer_seconds(25 * 1048576)?, 3237);
        assert_eq!(p.smtp_attempt_seconds(2, 16385)?, 7251);
        assert_eq!(p.smtp_final_seconds()?, 1052);
        Ok(())
    }

    #[test]
    fn ranges_raised_budgets_and_tick_overflow_are_checked(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for bad in [
            NetworkLimits {
                minimum_rate: 0,
                ..NetworkLimits::default()
            },
            NetworkLimits {
                minimum_rate: 4095,
                ..NetworkLimits::default()
            },
            NetworkLimits {
                minimum_rate: 65537,
                ..NetworkLimits::default()
            },
            NetworkLimits {
                handshake_seconds: 9,
                ..NetworkLimits::default()
            },
            NetworkLimits {
                command_seconds: 4801,
                ..NetworkLimits::default()
            },
        ] {
            let Err(error) = plan(bad) else {
                return Err("invalid network settings accepted".into());
            };
            let expected_field = if bad.minimum_rate != 65536 {
                "minimum_rate"
            } else if bad.handshake_seconds != 10 {
                "handshake_seconds"
            } else {
                "command_seconds"
            };
            assert!(
                matches!(error.downcast_ref::<Error>(), Some(Error::Range { field, .. }) if *field == expected_field)
            );
        }
        let raised = plan(NetworkLimits {
            command_seconds: 4800,
            ..NetworkLimits::default()
        })?;
        assert_eq!(raised.smtp_attempt_seconds(1, 1)?, 83009);
        let admission = DiskLimits::default().plan(
            &Limits::default().plan()?,
            WorkLimits {
                request_seconds: 600,
                checkpoint_seconds: 120,
                commit_seconds: 60,
                gc_seconds: 180,
                ..WorkLimits::default()
            },
            ViewMode::OnlineBackground,
        )?;
        let raised_work = NetworkLimits::default().plan(&admission)?;
        assert_eq!(raised_work.final_commit_seconds(), 241);
        assert_eq!(raised_work.smtp_attempt_seconds(1, 1)?, 6989);
        assert_eq!(raised_work.smtp_final_seconds()?, 1262);
        assert_eq!(
            raised_work.http_exchange_seconds(32 * 1048576, 128 * 1048576)?,
            3250
        );
        assert_eq!(deadline(Tick(5), 2)?.tick(), Tick(2005));
        assert_eq!(
            deadline(Tick(u64::MAX), 1),
            Err(Error::Overflow("deadline tick"))
        );
        assert_eq!(
            deadline(Tick(0), u64::MAX),
            Err(Error::Overflow("deadline milliseconds"))
        );
        assert_eq!(ceil_div(u64::MAX, 16384)?, 1125899906842624);
        Ok(())
    }
}
