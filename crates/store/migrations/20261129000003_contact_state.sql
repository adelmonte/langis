-- When sends found a person no longer registered (Desktop's
-- `discoveredUnregisteredAt`); cleared when they're found again.
ALTER TABLE conversations ADD COLUMN unregistered_at INTEGER;
