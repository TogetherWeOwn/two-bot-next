-- Synthetic fixture data only: no guild, member, endpoint or credential is real.
INSERT INTO events (id,event_type,member_id,guild_id,occurred_at,recorded_at,source,metadata,idempotency_key) VALUES
 (2,'member_joined','m2','g1','2026-01-01T01:00:00.123456+01:00','2026-01-01T00:00:01.123456Z','fixture',NULL,'event2'),
 (10,'message_created','m10','g1','2026-01-02T00:00:00Z','2026-01-02T00:00:01Z','fixture','{"n":10}','event10'),
 (9007199254740993,'landing_viewed',NULL,'g2','2026-01-03T00:00:00Z','2026-01-03T00:00:01Z','fixture','{}','eventlarge');
INSERT INTO members VALUES ('g1','m2','2026-01-01T00:00:00Z','fixture',NULL,NULL,'2026-01-03T00:00:00Z',NULL,NULL,FALSE,'2026-01-01T00:01:00Z',NULL),
 ('g2','m10',NULL,NULL,NULL,NULL,NULL,NULL,NULL,TRUE,NULL,NULL);
INSERT INTO invite_snapshots VALUES ('g1','invite',3,'m2',NULL,'2026-01-03T00:00:00Z');
INSERT INTO member_levels VALUES ('g1','m2',9007199254740991,20,30,9007199254740941,'2026-01-03T00:00:00.123456Z');
INSERT INTO xp_cooldowns VALUES ('g1','m2','message','2026-01-03T00:00:00Z'),('g1','m2','voice','2026-01-03T00:01:00Z');
INSERT INTO xp_awards VALUES (10,'g1','m2','message',20,'2026-01-03T00:00:00Z',NULL);
INSERT INTO level_role_rewards VALUES ('g1',3,'r3');
INSERT INTO level_import_runs VALUES (10,'g1','mee6',2,2,1,1,0,0,9007199254740941,'2026-01-03T00:00:00Z');
UPDATE web_contract_meta SET guild_id='g1';
INSERT INTO guild_counters VALUES ('g1',50,'2026-01-03T00:00:00Z',NULL,NULL);
UPDATE rank_ladder SET role_id='r1' WHERE rank_key='prospect';
INSERT INTO rank_ladder VALUES ('fixture','Fixture rank',6,NULL);
INSERT INTO rank_snapshots VALUES ('g1','fixture',2,NULL,'2026-01-03T00:00:00Z');
INSERT INTO member_ranks VALUES ('g1','m2','fixture','2026-01-03T00:00:00Z'),('g1','m10',NULL,'2026-01-03T00:01:00Z');
INSERT INTO scheduled_events VALUES ('g1','event1','Fixture event','2026-01-04T00:00:00Z',NULL,'fixture description','scheduled','2026-01-03T00:00:00Z');
INSERT INTO counter_snapshots VALUES ('g1',NULL,NULL,5,'2026-01-03T00:00:00Z');
INSERT INTO member_exclusions VALUES ('g1','raid1','raid','2026-01-03T00:00:00Z');
INSERT INTO presence_probe VALUES ('g1','2026-01-03T00:00:00Z',5,NULL,FALSE),('g1','2026-01-03T00:01:00Z',8,2,TRUE);
INSERT INTO community_facts VALUES (10,'g1','message_created','message1','m2','2026-01-03T00:00:00Z','2026-01-03T00:00:01Z','fixture','v1','eligible_human','human',NULL,'fact1');
INSERT INTO community_stream_heartbeats VALUES ('g1','message_created','2026-01-01T00:00:00Z','2026-01-03T00:00:00Z','2026-01-03T00:00:00Z');
INSERT INTO community_scorecard_runs VALUES (10,'g1','2026-01-01','2026-01-08','v1',10,1,'fixturehash','run1',1,'completed','complete','sufficient','{"messages":1}','none','2026-01-08T00:00:00Z');
INSERT INTO community_scorecard_alerts VALUES ('g1','2026-01-01','alert1','2026-01-08T00:00:00Z');
INSERT INTO guild_settings VALUES ('g1','TWO_RAID_JOIN_THRESHOLD','3',19,'2026-01-03T00:00:00.123456Z','fixture'),
 ('g2','TWO_RAID_WINDOW_SECONDS','{"nested":[null,2,"x"]}',20,'2026-01-03T00:00:00Z','fixture');
INSERT INTO guild_settings_audit VALUES (10,'g1','TWO_RAID_JOIN_THRESHOLD',NULL,'3','fixture','2026-01-03T00:00:00.123456Z');
INSERT INTO operational_audit_log (entry_id,event_kind,guild_id,occurred_at,actor_id,target_id,source_channel_id,destination_channel_id,message_id,action,metadata_json,created_at,mirror_channel_id,delivery_state,delivery_attempts,delivery_attempted_at,delivery_last_error,delivery_lease_until,mirrored_at,delivery_nonce,mirror_message_id,delivery_search_before,delivery_claim_token,mirror_checked_at) VALUES
 ('audit1','member_join','g1','2026-01-03T00:00:00Z',NULL,'m2',NULL,NULL,NULL,NULL,'{}','2026-01-03T00:00:01Z','channel1','delivered',1,'2026-01-03T00:00:02Z',NULL,NULL,'2026-01-03T00:00:03Z','nonce1','message1',NULL,'claim1','2026-01-03T00:00:04Z'),
 ('audit2','member_join','g1','2026-01-03T00:00:00Z',NULL,'m10',NULL,NULL,NULL,NULL,'{}','2026-01-03T00:00:01Z',NULL,'none',0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL);
INSERT INTO audit_kill_switch VALUES (1,'2026-01-03T00:00:00Z','fixture');
