-- A live or saved session always resumes with the agent it started with.
ALTER TABLE sessions ADD COLUMN agent_kind TEXT;
ALTER TABLE schedules ADD COLUMN agent_kind TEXT;
ALTER TABLE project_reviews ADD COLUMN agent_kind TEXT;
ALTER TABLE autopilots ADD COLUMN agent_kind TEXT;

CREATE VIEW session_credentials AS
  SELECT id, user_id, status, on_runners, 'claude' AS agent_kind FROM claude_credentials
  UNION ALL
  SELECT id, user_id, status, 0 AS on_runners, 'codex' AS agent_kind FROM codex_credentials;
