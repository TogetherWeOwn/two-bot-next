-- Repair committed self-role targets without rewriting published migration 0022.
-- The exact latest audit row is authoritative only when it completed successfully
-- and its persisted desired state agrees with the claim target. Recompute both
-- directions so a rejected or ambiguous target cannot remain trusted.
UPDATE self_role_panel_claims AS claims
SET target_committed = EXISTS (
  SELECT 1 FROM self_role_audit AS audit
  WHERE audit.event_id = claims.latest_event_id
    AND audit.guild_id = claims.guild_id
    AND audit.member_id = claims.member_id
    AND audit.panel_id = claims.panel_id
    AND CASE
      WHEN jsonb_array_length(audit.desired_role_ids::jsonb) = 0 THEN NULL
      WHEN jsonb_array_length(audit.desired_role_ids::jsonb) = 1 THEN audit.option_key
      ELSE '__invalid_multi_target__'
    END IS NOT DISTINCT FROM claims.latest_option_key
    AND audit.outcome IN ('assigned', 'removed', 'switched', 'already_held', 'already_absent')
);
