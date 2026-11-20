-- Our stories: who a send didn't reach (Desktop's per-recipient send
-- state, "Send failed" / "Partially sent"), and the story message as sent,
-- to try them again.
ALTER TABLE stories ADD COLUMN failed TEXT;
ALTER TABLE stories ADD COLUMN wire BLOB;
