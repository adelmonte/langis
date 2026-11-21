//! Hidden sticker packs, as Signal Desktop keeps them (`hidden_sticker_packs`,
//! `ts/sql/server/stickers.std.ts`, merged by `mergeHiddenStickerPackRecord`
//! in `ts/services/storageRecordOps.preload.ts`, Desktop commit 5edf85d87):
//! a pack someone sent us that we don't want offered in the sticker
//! manager's "Shared With You", by its id with its key, or when it was
//! unhidden (a tombstone kept for Storage Service). Installing a pack
//! unhides it. They live in one `settings` row as JSON, as favorite
//! stickers do.

use serde::{Deserialize, Serialize};

use crate::{AppDb, Result};

const SETTING: &str = "hidden_sticker_packs";

/// One hidden pack, or the tombstone of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenStickerPack {
    /// Hex.
    pub pack_id: String,
    /// Hex; unset on a tombstone.
    #[serde(default)]
    pub pack_key: Option<String>,
    /// Unix millis; set on a tombstone.
    #[serde(default)]
    pub unhidden_at: Option<i64>,
    /// Changed here and not yet written to Storage Service (Desktop's
    /// `storageNeedsSync`).
    #[serde(default)]
    pub needs_sync: bool,
}

impl HiddenStickerPack {
    pub fn is_hidden(&self) -> bool {
        self.unhidden_at.is_none()
    }
}

/// A `HiddenStickerPackRecord` as Storage Service has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHiddenStickerPack {
    /// Hex.
    pub pack_id: String,
    /// Hex, when the record has one.
    pub pack_key: Option<String>,
    /// Unix millis, 0 while it's hidden.
    pub unhidden_at: i64,
}

/// What a merge changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HiddenPackMerge {
    pub changed: bool,
    /// Hidden remote records without a pack key, skipped (Desktop throws).
    pub keyless: usize,
}

/// Desktop's merge (`mergeHiddenStickerPackRecord`, and `processManifest`
/// for local records the manifest doesn't have), applied to `local`:
///
/// - A remote tombstone older than `drop_before` (now less the message
///   queue time) is dropped, the local record with it.
/// - A remote tombstone or hidden pack replaces the local record; a hidden
///   one without a key is skipped.
/// - A local hidden pack the manifest doesn't have is written again; a
///   local tombstone it doesn't have goes.
///
/// A local record with a change not yet written is left for that write,
/// as `merge_favorites` leaves one.
pub fn merge_hidden_packs(local: &mut Vec<HiddenStickerPack>, remote: &[RemoteHiddenStickerPack], drop_before: i64) -> HiddenPackMerge {
    let mut merge = HiddenPackMerge::default();
    let before = local.clone();
    let mut seen = std::collections::HashSet::new();
    for record in remote {
        seen.insert(record.pack_id.clone());
        let at = local.iter().position(|p| p.pack_id == record.pack_id);
        if at.is_some_and(|i| local[i].needs_sync) {
            continue;
        }
        let next = if record.unhidden_at > 0 {
            (record.unhidden_at >= drop_before).then(|| HiddenStickerPack {
                pack_id: record.pack_id.clone(),
                pack_key: None,
                unhidden_at: Some(record.unhidden_at),
                needs_sync: false,
            })
        } else {
            let Some(key) = record.pack_key.clone() else {
                merge.keyless += 1;
                continue;
            };
            Some(HiddenStickerPack { pack_id: record.pack_id.clone(), pack_key: Some(key), unhidden_at: None, needs_sync: false })
        };
        match (at, next) {
            (Some(i), Some(next)) => local[i] = next,
            (Some(i), None) => {
                local.remove(i);
            }
            (None, Some(next)) => local.push(next),
            (None, None) => {}
        }
    }
    local.retain_mut(|p| {
        if p.needs_sync || seen.contains(&p.pack_id) {
            return true;
        }
        if p.is_hidden() {
            p.needs_sync = true;
            return true;
        }
        false
    });
    local.sort_by(|a, b| a.pack_id.cmp(&b.pack_id));
    merge.changed = *local != before;
    merge
}

fn parse(value: Option<String>) -> Vec<HiddenStickerPack> {
    value.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

impl AppDb {
    /// Every hidden pack, and the tombstones.
    pub async fn hidden_sticker_packs(&self) -> Result<Vec<HiddenStickerPack>> {
        Ok(parse(self.setting(SETTING).await?))
    }

    /// The hex ids of the packs hidden now.
    pub async fn hidden_sticker_pack_ids(&self) -> Result<Vec<String>> {
        Ok(self.hidden_sticker_packs().await?.into_iter().filter(HiddenStickerPack::is_hidden).map(|p| p.pack_id).collect())
    }

    async fn edit_hidden_packs<T>(&self, edit: impl FnOnce(&mut Vec<HiddenStickerPack>) -> T) -> Result<T> {
        let mut tx = self.write().await?;
        let value: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(SETTING).fetch_optional(&mut *tx).await?;
        let mut packs = parse(value);
        let before = packs.clone();
        let result = edit(&mut packs);
        if packs != before {
            packs.sort_by(|a, b| a.pack_id.cmp(&b.pack_id));
            sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
                .bind(SETTING)
                .bind(serde_json::to_string(&packs)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(result)
    }

    /// Hides a pack (Desktop's `markStickerPackAsHidden`), from its
    /// tombstone too: the pack, to be written, or `None` when it was hidden
    /// already.
    pub async fn hide_sticker_pack(&self, pack_id: &str, pack_key: &str) -> Result<Option<HiddenStickerPack>> {
        self.edit_hidden_packs(|packs| {
            let hidden = HiddenStickerPack {
                pack_id: pack_id.to_owned(),
                pack_key: Some(pack_key.to_owned()),
                unhidden_at: None,
                needs_sync: true,
            };
            match packs.iter().position(|p| p.pack_id == pack_id) {
                Some(i) if packs[i].is_hidden() => return None,
                Some(i) => packs[i] = hidden.clone(),
                None => packs.push(hidden.clone()),
            }
            Some(hidden)
        })
        .await
    }

    /// Unhides a pack at `now` (Desktop's `markHiddenStickerPackDeleted`,
    /// on undo and when the pack is installed): its tombstone, to be
    /// written, or `None` when it wasn't hidden.
    pub async fn unhide_sticker_pack(&self, pack_id: &str, now: i64) -> Result<Option<HiddenStickerPack>> {
        self.edit_hidden_packs(|packs| {
            let pack = packs.iter_mut().find(|p| p.pack_id == pack_id && p.is_hidden())?;
            *pack = HiddenStickerPack { pack_id: pack_id.to_owned(), pack_key: None, unhidden_at: Some(now), needs_sync: true };
            Some(pack.clone())
        })
        .await
    }

    /// Storage Service's hidden packs, merged (see [`merge_hidden_packs`]).
    pub async fn merge_storage_hidden_packs(&self, remote: &[RemoteHiddenStickerPack], drop_before: i64) -> Result<HiddenPackMerge> {
        self.edit_hidden_packs(|packs| merge_hidden_packs(packs, remote, drop_before)).await
    }

    /// `written` (as it was then) is in Storage Service: no longer pending,
    /// unless it changed again meanwhile.
    pub async fn hidden_sticker_pack_written(&self, written: &HiddenStickerPack) -> Result<()> {
        self.edit_hidden_packs(|packs| {
            if let Some(p) = packs.iter_mut().find(|p| {
                p.pack_id == written.pack_id && (&p.pack_key, p.unhidden_at) == (&written.pack_key, written.unhidden_at)
            }) {
                p.needs_sync = false;
            }
        })
        .await
    }

    /// Hidden packs from a transferred backup (Desktop's
    /// `fromHiddenStickerPack`): `(id, key)` in hex, each as Storage Service
    /// already has it, skipping one we have in any state. Returns how many
    /// were added.
    pub async fn import_hidden_sticker_packs(&self, imported: &[(String, String)]) -> Result<usize> {
        self.edit_hidden_packs(|packs| {
            let mut added = 0;
            for (id, key) in imported {
                if packs.iter().any(|p| &p.pack_id == id) {
                    continue;
                }
                packs.push(HiddenStickerPack { pack_id: id.clone(), pack_key: Some(key.clone()), unhidden_at: None, needs_sync: false });
                added += 1;
            }
            added
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(id: &str, needs_sync: bool) -> HiddenStickerPack {
        HiddenStickerPack { pack_id: id.into(), pack_key: Some("k".into()), unhidden_at: None, needs_sync }
    }

    fn remote(id: &str, key: Option<&str>, unhidden_at: i64) -> RemoteHiddenStickerPack {
        RemoteHiddenStickerPack { pack_id: id.into(), pack_key: key.map(Into::into), unhidden_at }
    }

    #[test]
    fn storage_records_merge_as_desktop_merges_them() {
        let mut local = vec![
            hidden("aa", false),
            // Ours, not yet written: left for the write.
            HiddenStickerPack { unhidden_at: Some(5), pack_key: None, ..hidden("bb", true) },
            // Not in the manifest: written again.
            hidden("cc", false),
            // A tombstone not in the manifest: gone.
            HiddenStickerPack { unhidden_at: Some(5), pack_key: None, ..hidden("dd", false) },
        ];
        let merge = merge_hidden_packs(
            &mut local,
            &[
                // Unhidden elsewhere, recently.
                remote("aa", None, 100),
                remote("bb", Some("k2"), 0),
                // Hidden elsewhere.
                remote("ee", Some("k3"), 0),
                // Without a key: skipped.
                remote("ff", None, 0),
                // An old tombstone: dropped.
                remote("gg", None, 10),
            ],
            50,
        );
        assert!(merge.changed);
        assert_eq!(merge.keyless, 1);
        let state: Vec<_> = local.iter().map(|p| (p.pack_id.as_str(), p.is_hidden(), p.needs_sync)).collect();
        assert_eq!(state, [("aa", false, false), ("bb", false, true), ("cc", true, true), ("ee", true, false)]);
    }
}
