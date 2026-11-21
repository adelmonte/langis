-- Notification profiles (Desktop's `notificationProfiles` table and the
-- Storage Service `NotificationProfile` record). A deleted one is kept,
-- with when, so other devices hear of it.
CREATE TABLE notification_profiles (
    -- 16 random bytes, lowercase hex.
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    emoji TEXT,
    -- 0xAARRGGBB.
    color INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    allow_all_calls INTEGER NOT NULL DEFAULT 0,
    allow_all_mentions INTEGER NOT NULL DEFAULT 0,
    -- JSON array of conversation ids.
    allowed_members TEXT NOT NULL DEFAULT '[]',
    schedule_enabled INTEGER NOT NULL DEFAULT 0,
    -- 24-hour clock as hhmm (900 is 9:00, 2345 is 23:45).
    schedule_start INTEGER,
    schedule_end INTEGER,
    -- Bit n set: day n+1 of Desktop's DayOfWeek (Monday = 1 ... Sunday = 7).
    schedule_days INTEGER NOT NULL DEFAULT 0,
    -- Unix millis; 0 while it's current.
    deleted_at INTEGER NOT NULL DEFAULT 0
);
