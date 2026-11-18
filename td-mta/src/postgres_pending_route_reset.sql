EXISTS (
    SELECT 1 FROM td_mta.attempt_acceptance_receipts accepted
    JOIN td_mta.route_owners owner ON owner.account=accepted.account
    WHERE accepted.account=a.account AND accepted.attempt_id=a.attempt_id
      AND accepted.phase=4 AND owner.route_name=a.route_name
      AND owner.destination=a.destination AND owner.generation=a.generation
      AND owner.owner_boot=a.owner_boot AND owner.operation=a.owner_operation
      AND NOT EXISTS (
          SELECT 1 FROM td_mta.route_health_receipts reset
          WHERE reset.account=a.account AND reset.operation=a.attempt_id AND reset.action=3
      )
)
