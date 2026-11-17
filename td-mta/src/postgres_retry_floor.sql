-- Route backoff starts at the serving process's minimum retry interval,
-- 1..300 seconds; retained rows any permitted floor wrote stay valid.
ALTER TABLE td_mta.route_health DROP CONSTRAINT route_health_check2;
ALTER TABLE td_mta.route_health ADD CONSTRAINT route_health_check2
    CHECK(failure_count=0 OR (next_at>=failed_at AND next_at-failed_at BETWEEN 1000 AND 3600000));
ALTER TABLE td_mta.route_health_receipts DROP CONSTRAINT route_health_receipts_check3;
ALTER TABLE td_mta.route_health_receipts ADD CONSTRAINT route_health_receipts_check3
    CHECK(failure_count=0 OR (next_at>=failed_at AND next_at-failed_at BETWEEN 1000 AND 3600000));
