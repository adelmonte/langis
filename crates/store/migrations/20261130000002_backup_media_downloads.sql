-- Desktop's backup media download jobs (`attachment_downloads` rows with
-- source `backup_import`): attachments from a transferred backup that
-- download from the backup tier, each with the bytes it counts for in
-- Desktop's "Syncing media" progress (`ciphertextSize`). A row goes when
-- its download finished or gave up; the bytes done are kept in settings.
CREATE TABLE backup_media_downloads (
    message_id INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (message_id, idx)
) WITHOUT ROWID;
