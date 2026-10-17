-- Provider-specific settings (e.g. Bedrock region, AWS key id, session token)
-- as a JSON object of strings.
ALTER TABLE llm_settings ADD COLUMN options TEXT NOT NULL DEFAULT '{}';
