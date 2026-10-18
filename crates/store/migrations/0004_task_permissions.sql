-- Changing tools (or `tool:action` pairs) a scheduled task may use without
-- asking, as a JSON array the merchant confirmed when creating the task.
ALTER TABLE scheduled_tasks ADD COLUMN allowed_tools TEXT NOT NULL DEFAULT '[]';
