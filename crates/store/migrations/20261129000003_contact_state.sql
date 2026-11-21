-- When sends found a person no longer registered (Desktop's
-- `discoveredUnregisteredAt`); cleared when they're found again.
ALTER TABLE conversations ADD COLUMN unregistered_at INTEGER;
-- A person we removed (Desktop's `removalStage`): 1 `justNotification`,
-- 2 `messageRequest` once they messaged us since.
ALTER TABLE conversations ADD COLUMN removal_stage INTEGER;
