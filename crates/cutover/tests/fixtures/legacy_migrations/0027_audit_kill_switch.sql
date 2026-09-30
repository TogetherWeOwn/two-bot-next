-- TOG-3187: emergency kill switch for the audit mirror.
--
-- Row presence (id = 1) halts every mirror Discord send and every pending-row
-- retry for the running process, without a redeploy and without touching the
-- durable rows. DELETE the row to resume. A row rather than a boolean column:
-- presence is atomic to create and remove, and the row itself records who
-- pulled the lever and when - the switch is evidence too.
CREATE TABLE IF NOT EXISTS audit_kill_switch (
  id INTEGER PRIMARY KEY,
  engaged_at TIMESTAMPTZ NOT NULL,
  engaged_by TEXT NOT NULL
);
