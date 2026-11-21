//! Notification profiles, as Signal Desktop keeps them
//! (`ts/types/NotificationProfile.std.ts`): a named set of people and
//! groups whose notifications still come while the profile is on, turned
//! on by a weekly schedule or by hand. The hand override is one settings
//! row; the profiles are a table, deleted ones kept (with when) so Storage
//! Service hears of it.

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{AppDb, ConversationId, Result};

/// Settings key: the manual override (JSON [`NotificationProfileOverride`]).
pub const NOTIFICATION_PROFILE_OVERRIDE: &str = "notification_profile_override";

/// Desktop's `DEFAULT_PROFILE_COLOR` (the A100 avatar background).
pub const DEFAULT_PROFILE_COLOR: u32 = 0xffe3e3fe;

/// Desktop's `NOTIFICATION_PROFILE_ID_LENGTH`, in bytes.
pub const NOTIFICATION_PROFILE_ID_LENGTH: usize = 16;

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Desktop's `NotificationProfileType`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationProfile {
    /// 16 random bytes as lowercase hex.
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    /// 0xAARRGGBB.
    pub color: u32,
    /// Unix millis. The newest profile wins when schedules overlap.
    pub created_at: u64,
    pub allow_all_calls: bool,
    /// Mentions and replies to us notify ("Notify for all mentions").
    pub allow_all_mentions: bool,
    pub allowed_members: Vec<ConversationId>,
    pub schedule_enabled: bool,
    /// 24-hour clock as hhmm: 900 is 9:00, 2345 is 23:45.
    pub schedule_start: Option<u32>,
    pub schedule_end: Option<u32>,
    /// Monday first: `schedule_days[0]` is Monday, `[6]` Sunday.
    pub schedule_days: [bool; 7],
    /// Unix millis; 0 while it's current.
    pub deleted_at: u64,
}

impl NotificationProfile {
    /// A new profile as Desktop's create flow starts one
    /// (`ts/components/PreferencesNotificationProfiles`): no one allowed,
    /// all calls let through but not mentions, and a 9:00 to 17:00 weekday
    /// schedule that is off.
    pub fn new(id: String, name: String, created_at: u64) -> Self {
        Self {
            id,
            name,
            emoji: None,
            color: DEFAULT_PROFILE_COLOR,
            created_at,
            allow_all_calls: true,
            allow_all_mentions: false,
            allowed_members: Vec::new(),
            schedule_enabled: false,
            schedule_start: Some(900),
            schedule_end: Some(1700),
            schedule_days: [true, true, true, true, true, false, false],
            deleted_at: 0,
        }
    }

    pub fn is_current(&self) -> bool {
        self.deleted_at == 0
    }

    /// Desktop's `shouldNotify` with this profile on: a call when all calls
    /// are allowed, a mention or reply when all mentions are, or anything
    /// from an allowed chat.
    pub fn allows(&self, conversation: &ConversationId, is_call: bool, mention_or_reply: bool) -> bool {
        (is_call && self.allow_all_calls)
            || (mention_or_reply && self.allow_all_mentions)
            || self.allowed_members.contains(conversation)
    }

    /// The days as Desktop's `DayOfWeek` numbers (Monday = 1 ... Sunday = 7).
    pub fn day_numbers(&self) -> Vec<u8> {
        (1..=7u8).filter(|d| self.schedule_days[usize::from(d - 1)]).collect()
    }

    /// Days from `DayOfWeek` numbers (Desktop's `fromDayOfWeekArray`: 0,
    /// unknown, counts as Monday; others are ignored).
    pub fn days_from_numbers(numbers: impl IntoIterator<Item = i32>) -> [bool; 7] {
        let mut days = [false; 7];
        for n in numbers {
            match n {
                0 => days[0] = true,
                1..=7 => days[(n - 1) as usize] = true,
                _ => {}
            }
        }
        days
    }

    fn days_mask(&self) -> i64 {
        self.schedule_days.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| 1i64 << i).sum()
    }
}

/// Desktop's `shouldNotify`: true when no profile is on.
pub fn should_notify(
    active: Option<&NotificationProfile>,
    conversation: &ConversationId,
    is_call: bool,
    mention_or_reply: bool,
) -> bool {
    active.is_none_or(|p| p.allows(conversation, is_call, mention_or_reply))
}

/// Desktop's `NotificationProfileOverride`: a profile turned on or off by
/// hand, until its schedule next says otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationProfileOverride {
    /// Every profile off since `at` (unix millis), until a schedule next
    /// turns one on.
    Disabled { at: u64 },
    /// This profile on until `ends_at`, or with none, until its schedule
    /// next turns it off. `set_at` (unix millis, local only) is when it was
    /// turned on, to find that time again after a restart.
    Enabled {
        id: String,
        ends_at: Option<u64>,
        #[serde(default)]
        set_at: u64,
    },
}

/// Where a moment falls in local time: the start of its day (unix millis)
/// and the day of the week (Monday = 1 ... Sunday = 7). [`local_calendar`]
/// is the machine's; tests pass a fixed one.
pub type Calendar<'a> = &'a dyn Fn(u64) -> (u64, u8);

/// The local time zone's [`Calendar`] (Desktop's `getMidnight` and
/// `getDayOfWeek`, which use the local date).
pub fn local_calendar(time: u64) -> (u64, u8) {
    use chrono::{Datelike, Local, TimeZone};
    let Some(at) = Local.timestamp_millis_opt(time as i64).earliest() else { return (time - time % DAY, 1) };
    let midnight = at
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|m| Local.from_local_datetime(&m).earliest())
        .map_or(time - time % DAY, |m| m.timestamp_millis().max(0) as u64);
    (midnight, at.weekday().number_from_monday() as u8)
}

/// Desktop's `NextProfileEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextProfileEvent {
    NoChange { active: Option<String> },
    WillDisable { at: u64, active: String, clear_enable_override: bool },
    WillEnable { at: u64, to_enable: String, clear_disable_override: bool },
}

impl NextProfileEvent {
    pub fn active(&self) -> Option<&str> {
        match self {
            Self::NoChange { active } => active.as_deref(),
            Self::WillDisable { active, .. } => Some(active),
            Self::WillEnable { .. } => None,
        }
    }

    pub fn at(&self) -> Option<u64> {
        match self {
            Self::NoChange { .. } => None,
            Self::WillDisable { at, .. } | Self::WillEnable { at, .. } => Some(*at),
        }
    }
}

/// Desktop's `scheduleToTime`.
fn schedule_to_time(midnight: u64, hhmm: u32) -> u64 {
    midnight + u64::from(hhmm / 100) * HOUR + u64::from(hhmm % 100) * MINUTE
}

/// Desktop's `getEndTime`: an end at or before the start is the next day.
fn end_time(midnight: u64, start: u32, end: u32) -> u64 {
    let (s, e) = (schedule_to_time(midnight, start), schedule_to_time(midnight, end));
    if e > s { e } else { e + DAY }
}

/// The schedule when it's on for `day` (1 to 7): start and end hhmm.
fn schedule_for(profile: &NotificationProfile, day: u8) -> Option<(u32, u32)> {
    let on = profile.schedule_enabled && profile.schedule_days.get(usize::from(day.wrapping_sub(1))).copied().unwrap_or(false);
    on.then_some((profile.schedule_start?, profile.schedule_end?))
}

/// Desktop's `isProfileEnabledBySchedule`.
fn enabled_by_schedule(cal: Calendar, time: u64, schedule_time: u64, profile: &NotificationProfile) -> bool {
    let (midnight, day) = cal(schedule_time);
    let Some((start, end)) = schedule_for(profile, day) else { return false };
    time >= schedule_to_time(midnight, start) && time <= end_time(midnight, start, end)
}

fn enabled_now_or_from_yesterday(cal: Calendar, time: u64, profile: &NotificationProfile) -> bool {
    enabled_by_schedule(cal, time, time.saturating_sub(DAY), profile) || enabled_by_schedule(cal, time, time, profile)
}

/// Desktop's `loopThroughWeek`: yesterday, today and the six days after,
/// until `check` says stop.
fn loop_week(cal: Calendar, time: u64, mut check: impl FnMut(u64, u8) -> bool) {
    let (today, starting_day) = cal(time);
    for index in -1i64..7 {
        let mut day = i64::from(starting_day) + index;
        if day <= 0 {
            day += 7;
        }
        if day > 7 {
            day -= 7;
        }
        let start_of_day = (today as i64 + DAY as i64 * index).max(0) as u64;
        if check(start_of_day, day as u8) {
            return;
        }
    }
}

/// Desktop's `findNextScheduledDisable`.
fn next_scheduled_disable(cal: Calendar, time: u64, profile: &NotificationProfile) -> Option<u64> {
    let mut result = None;
    loop_week(cal, time, |start_of_day, day| match schedule_for(profile, day) {
        Some((start, end)) if time < end_time(start_of_day, start, end) => {
            result = Some(end_time(start_of_day, start, end));
            true
        }
        _ => false,
    });
    result
}

/// Desktop's `findNextScheduledEnable`.
fn next_scheduled_enable(cal: Calendar, time: u64, profile: &NotificationProfile) -> Option<u64> {
    let mut result = None;
    loop_week(cal, time, |start_of_day, day| match schedule_for(profile, day) {
        Some((start, _)) if time < schedule_to_time(start_of_day, start) => {
            result = Some(schedule_to_time(start_of_day, start));
            true
        }
        _ => false,
    });
    result
}

/// Desktop's `findNextScheduledEnableForAll`: the earliest, the first
/// (newest) on a tie.
fn next_enable_for_all<'a>(cal: Calendar, time: u64, profiles: &[&'a NotificationProfile]) -> Option<(&'a NotificationProfile, u64)> {
    let mut earliest: Option<(&NotificationProfile, u64)> = None;
    for profile in profiles {
        if let Some(at) = next_scheduled_enable(cal, time, profile)
            && earliest.is_none_or(|(_, e)| at < e)
        {
            earliest = Some((profile, at));
        }
    }
    earliest
}

/// The current profiles, newest first (Desktop's `sortProfiles`, which
/// gives the newest precedence when schedules overlap).
pub fn current_profiles(profiles: &[NotificationProfile]) -> Vec<&NotificationProfile> {
    let mut current: Vec<&NotificationProfile> = profiles.iter().filter(|p| p.is_current()).collect();
    current.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    current
}

/// Desktop's `findNextProfileEvent`: which profile is on at `time` and when
/// that changes next. `None` where Desktop throws (an override naming a
/// profile that's gone, which Desktop then clears).
pub fn find_next_profile_event(
    cal: Calendar,
    override_: Option<&NotificationProfileOverride>,
    profiles: &[NotificationProfile],
    time: u64,
) -> Option<NextProfileEvent> {
    let profiles = current_profiles(profiles);
    match override_ {
        Some(NotificationProfileOverride::Enabled { id, ends_at, .. }) => {
            let profile = profiles.iter().find(|p| p.id == *id)?;
            if let Some(at) = ends_at.filter(|at| *at > 0) {
                return Some(NextProfileEvent::WillDisable { at, active: id.clone(), clear_enable_override: true });
            }
            if enabled_now_or_from_yesterday(cal, time, profile) {
                let at = next_scheduled_disable(cal, time, profile)?;
                return Some(NextProfileEvent::WillDisable { at, active: id.clone(), clear_enable_override: true });
            }
            let Some(enable) = next_scheduled_enable(cal, time, profile) else {
                return Some(NextProfileEvent::NoChange { active: Some(id.clone()) });
            };
            let at = next_scheduled_disable(cal, enable + 1, profile)?;
            Some(NextProfileEvent::WillDisable { at, active: id.clone(), clear_enable_override: true })
        }
        Some(NotificationProfileOverride::Disabled { at }) if *at > 0 => {
            Some(match next_enable_for_all(cal, at + 1, &profiles) {
                Some((profile, when)) => {
                    NextProfileEvent::WillEnable { at: when, to_enable: profile.id.clone(), clear_disable_override: true }
                }
                None => NextProfileEvent::NoChange { active: None },
            })
        }
        _ => {
            if let Some(active) = profiles.iter().find(|p| enabled_now_or_from_yesterday(cal, time, p)) {
                let disabled_at = next_scheduled_disable(cal, time, active)?;
                // A newer profile whose schedule starts sooner takes over.
                let newer: Vec<&NotificationProfile> =
                    profiles.iter().copied().filter(|p| p.created_at > active.created_at).collect();
                let at = next_enable_for_all(cal, time + 1, &newer).map_or(disabled_at, |(_, t)| t.min(disabled_at));
                return Some(NextProfileEvent::WillDisable { at, active: active.id.clone(), clear_enable_override: false });
            }
            Some(match next_enable_for_all(cal, time, &profiles) {
                Some((profile, at)) => {
                    NextProfileEvent::WillEnable { at, to_enable: profile.id.clone(), clear_disable_override: false }
                }
                None => NextProfileEvent::NoChange { active: None },
            })
        }
    }
}

/// Whether the override has run out by `now`: Desktop's
/// `notificationProfilesService` clears it when the event it set up passes
/// (a timed "on" ending, the schedule's next end after "on", the
/// schedule's next start after "off") or when it names a deleted profile.
pub fn override_expired(
    cal: Calendar,
    override_: &NotificationProfileOverride,
    profiles: &[NotificationProfile],
    now: u64,
) -> bool {
    let set_at = match override_ {
        NotificationProfileOverride::Enabled { set_at, .. } => (*set_at).min(now),
        NotificationProfileOverride::Disabled { at } => (*at).min(now),
    };
    match find_next_profile_event(cal, Some(override_), profiles, set_at) {
        None => true,
        Some(NextProfileEvent::WillDisable { at, clear_enable_override: true, .. }) => now >= at,
        Some(NextProfileEvent::WillEnable { at, clear_disable_override: true, .. }) => now >= at,
        Some(_) => false,
    }
}

/// Which profile is on at a moment, and until when.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NotificationProfileState {
    /// The profile on now, if any.
    pub active: Option<NotificationProfile>,
    /// When that may change (unix millis): look again then.
    pub next_change: Option<u64>,
    /// The manual override in effect (an expired one is left out).
    pub override_: Option<NotificationProfileOverride>,
}

/// The profile on at `now`, given every profile and the stored override.
pub fn notification_profile_state(
    cal: Calendar,
    profiles: &[NotificationProfile],
    override_: Option<&NotificationProfileOverride>,
    now: u64,
) -> NotificationProfileState {
    let override_ = override_.filter(|o| !override_expired(cal, o, profiles, now));
    let event = find_next_profile_event(cal, override_, profiles, now).or_else(|| find_next_profile_event(cal, None, profiles, now));
    let Some(event) = event else { return NotificationProfileState::default() };
    let active = event.active().and_then(|id| profiles.iter().find(|p| p.id == id)).cloned();
    NotificationProfileState { active, next_change: event.at(), override_: override_.cloned() }
}

/// A change to the profiles or the override.
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationProfileEdit {
    /// Adds a profile or replaces the one with its id (Desktop's
    /// `createProfile` and `updateProfile`).
    Save(NotificationProfile),
    /// Desktop's `markProfileDeleted` at this time (unix millis): kept with
    /// its deletion time; an override naming it is cleared.
    Delete { id: String, at: u64 },
    /// Turns a profile on, until `ends_at` (unix millis) or its schedule
    /// next turns it off (Desktop's `setProfileOverride(id, true, endsAt)`).
    /// `at` is now.
    Enable { id: String, ends_at: Option<u64>, at: u64 },
    /// Turns profiles off at `at` until a schedule next turns one on
    /// (`setProfileOverride(id, false)`).
    Disable { at: u64 },
    /// A profile as Storage Service has it (Desktop's
    /// `mergeNotificationProfileRecord`): the remote's fields, and the
    /// earlier deletion time of the two.
    Merge(NotificationProfile),
    /// The account record's override (`None`: no override). `at` is now,
    /// the `set_at` of an "on" override that differs from ours.
    MergeOverride { override_: Option<NotificationProfileOverride>, at: u64 },
}

/// What an edit changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationProfileChanges {
    /// The profiles saved or deleted (to save to Storage Service).
    pub profiles: Vec<String>,
    /// The override changed.
    pub override_: bool,
}

impl NotificationProfileChanges {
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty() && !self.override_
    }
}

fn from_row(row: &sqlx::sqlite::SqliteRow) -> NotificationProfile {
    let members: String = row.get("allowed_members");
    let mask: i64 = row.get("schedule_days");
    let days = std::array::from_fn(|i| mask & (1 << i) != 0);
    NotificationProfile {
        id: row.get("id"),
        name: row.get("name"),
        emoji: row.get("emoji"),
        color: row.get::<i64, _>("color") as u32,
        created_at: row.get::<i64, _>("created_at").max(0) as u64,
        allow_all_calls: row.get("allow_all_calls"),
        allow_all_mentions: row.get("allow_all_mentions"),
        allowed_members: serde_json::from_str(&members).unwrap_or_default(),
        schedule_enabled: row.get("schedule_enabled"),
        schedule_start: row.get::<Option<i64>, _>("schedule_start").map(|v| v as u32),
        schedule_end: row.get::<Option<i64>, _>("schedule_end").map(|v| v as u32),
        schedule_days: days,
        deleted_at: row.get::<i64, _>("deleted_at").max(0) as u64,
    }
}

impl AppDb {
    /// Every profile, deleted ones too, newest first.
    pub async fn notification_profiles(&self) -> Result<Vec<NotificationProfile>> {
        let rows = sqlx::query("SELECT * FROM notification_profiles ORDER BY created_at DESC, id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(from_row).collect())
    }

    /// The stored manual override (it may have run out; see
    /// [`notification_profile_state`]).
    pub async fn notification_profile_override(&self) -> Result<Option<NotificationProfileOverride>> {
        Ok(self.setting(NOTIFICATION_PROFILE_OVERRIDE).await?.and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// The profile on at `now` (unix millis) in local time, and when that
    /// changes next.
    pub async fn notification_profile_state(&self, now: u64) -> Result<NotificationProfileState> {
        let profiles = self.notification_profiles().await?;
        let override_ = self.notification_profile_override().await?;
        Ok(notification_profile_state(&local_calendar, &profiles, override_.as_ref(), now))
    }

    /// Whether a notification from `conversation` is silenced by the
    /// profile on at `now` (Desktop's `shouldNotify`, negated): `is_call`
    /// for a ringing call, `mention_or_reply` for a message that mentions or
    /// replies to us.
    pub async fn notification_silenced(
        &self,
        conversation: &ConversationId,
        is_call: bool,
        mention_or_reply: bool,
        now: u64,
    ) -> Result<bool> {
        let state = self.notification_profile_state(now).await?;
        Ok(!should_notify(state.active.as_ref(), conversation, is_call, mention_or_reply))
    }

    /// Changes the profiles or override in one transaction.
    pub async fn edit_notification_profiles(&self, edit: NotificationProfileEdit) -> Result<NotificationProfileChanges> {
        let mut tx = self.write().await?;
        let mut changes = NotificationProfileChanges::default();
        let json: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(NOTIFICATION_PROFILE_OVERRIDE)
            .fetch_optional(&mut *tx)
            .await?;
        let old_override: Option<NotificationProfileOverride> = json.and_then(|j| serde_json::from_str(&j).ok());
        let mut new_override = old_override.clone();
        let load = |id: String| {
            sqlx::query("SELECT * FROM notification_profiles WHERE id = ?").bind(id)
        };
        let mut save = None;
        match edit {
            NotificationProfileEdit::Save(profile) => save = Some(profile),
            NotificationProfileEdit::Delete { id, at } => {
                if let Some(row) = load(id.clone()).fetch_optional(&mut *tx).await? {
                    let mut profile = from_row(&row);
                    if profile.is_current() {
                        profile.deleted_at = at.max(1);
                        save = Some(profile);
                    }
                }
                if matches!(&new_override, Some(NotificationProfileOverride::Enabled { id: on, .. }) if *on == id) {
                    new_override = None;
                }
            }
            NotificationProfileEdit::Enable { id, ends_at, at } => {
                new_override = Some(NotificationProfileOverride::Enabled { id, ends_at, set_at: at });
            }
            NotificationProfileEdit::Disable { at } => {
                new_override = Some(NotificationProfileOverride::Disabled { at: at.max(1) });
            }
            NotificationProfileEdit::Merge(mut remote) => {
                let local = load(remote.id.clone()).fetch_optional(&mut *tx).await?.map(|r| from_row(&r));
                // Desktop's `getEarliestTimestamp`, 0 meaning none.
                let local_deleted = local.as_ref().map_or(0, |l| l.deleted_at);
                remote.deleted_at = match (remote.deleted_at, local_deleted) {
                    (0, l) => l,
                    (r, 0) => r,
                    (r, l) => r.min(l),
                };
                save = Some(remote);
            }
            NotificationProfileEdit::MergeOverride { override_, at } => {
                new_override = match (override_, &old_override) {
                    // The same profile on until the same time: keep when we
                    // learned of it.
                    (
                        Some(NotificationProfileOverride::Enabled { id, ends_at, .. }),
                        Some(NotificationProfileOverride::Enabled { id: old_id, ends_at: old_ends, set_at }),
                    ) if id == *old_id && ends_at == *old_ends => {
                        Some(NotificationProfileOverride::Enabled { id, ends_at, set_at: *set_at })
                    }
                    (Some(NotificationProfileOverride::Enabled { id, ends_at, .. }), _) => {
                        Some(NotificationProfileOverride::Enabled { id, ends_at, set_at: at })
                    }
                    (other, _) => other,
                };
            }
        }
        if let Some(profile) = save {
            let old = load(profile.id.clone()).fetch_optional(&mut *tx).await?.map(|r| from_row(&r));
            if old.as_ref() != Some(&profile) {
                sqlx::query(
                    "INSERT OR REPLACE INTO notification_profiles (id, name, emoji, color, created_at, allow_all_calls, \
                     allow_all_mentions, allowed_members, schedule_enabled, schedule_start, schedule_end, schedule_days, \
                     deleted_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&profile.id)
                .bind(&profile.name)
                .bind(&profile.emoji)
                .bind(i64::from(profile.color))
                .bind(profile.created_at as i64)
                .bind(profile.allow_all_calls)
                .bind(profile.allow_all_mentions)
                .bind(serde_json::to_string(&profile.allowed_members)?)
                .bind(profile.schedule_enabled)
                .bind(profile.schedule_start.map(i64::from))
                .bind(profile.schedule_end.map(i64::from))
                .bind(profile.days_mask())
                .bind(profile.deleted_at as i64)
                .execute(&mut *tx)
                .await?;
                changes.profiles.push(profile.id);
            }
        }
        if new_override != old_override {
            match &new_override {
                Some(o) => {
                    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
                        .bind(NOTIFICATION_PROFILE_OVERRIDE)
                        .bind(serde_json::to_string(o)?)
                        .execute(&mut *tx)
                        .await?;
                }
                None => {
                    sqlx::query("DELETE FROM settings WHERE key = ?").bind(NOTIFICATION_PROFILE_OVERRIDE).execute(&mut *tx).await?;
                }
            }
            changes.override_ = true;
        }
        if changes.is_empty() {
            return Ok(changes);
        }
        tx.commit().await?;
        Ok(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> AppDb {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = AppDb::open(&dir.join("app.db"), &[3; 32]).await.unwrap();
        std::mem::forget(db.clone());
        db
    }

    /// UTC, with 1970-01-05 (day 4) a Monday.
    fn utc(time: u64) -> (u64, u8) {
        let days = time / DAY;
        (days * DAY, ((days + 3) % 7 + 1) as u8)
    }

    /// Monday of week `week` (from 1970-01-05) plus `day` days, at hh:mm.
    fn at(day: u64, hh: u64, mm: u64) -> u64 {
        4 * DAY + day * DAY + hh * HOUR + mm * MINUTE
    }

    fn profile(id: &str, created_at: u64) -> NotificationProfile {
        NotificationProfile::new(id.into(), id.into(), created_at)
    }

    fn scheduled(id: &str, created_at: u64, start: u32, end: u32, days: [bool; 7]) -> NotificationProfile {
        NotificationProfile {
            schedule_enabled: true,
            schedule_start: Some(start),
            schedule_end: Some(end),
            schedule_days: days,
            ..profile(id, created_at)
        }
    }

    const WEEKDAYS: [bool; 7] = [true, true, true, true, true, false, false];
    const EVERY_DAY: [bool; 7] = [true; 7];

    #[test]
    fn calendar_in_tests_starts_weeks_on_monday() {
        assert_eq!(utc(at(0, 10, 0)), (at(0, 0, 0), 1));
        assert_eq!(utc(at(6, 23, 59)), (at(6, 0, 0), 7));
    }

    #[test]
    fn a_schedule_turns_a_profile_on_and_off() {
        let work = scheduled("work", 1, 900, 1700, WEEKDAYS);
        let profiles = [work];
        let monday_morning = find_next_profile_event(&utc, None, &profiles, at(0, 8, 0)).unwrap();
        assert_eq!(monday_morning, NextProfileEvent::WillEnable { at: at(0, 9, 0), to_enable: "work".into(), clear_disable_override: false });
        let monday_noon = find_next_profile_event(&utc, None, &profiles, at(0, 12, 0)).unwrap();
        assert_eq!(monday_noon, NextProfileEvent::WillDisable { at: at(0, 17, 0), active: "work".into(), clear_enable_override: false });
        // Friday evening: next on Monday.
        let friday = find_next_profile_event(&utc, None, &profiles, at(4, 18, 0)).unwrap();
        assert_eq!(friday.at(), Some(at(7, 9, 0)));
        assert_eq!(friday.active(), None);
    }

    #[test]
    fn an_overnight_schedule_runs_into_the_next_day() {
        let sleep = scheduled("sleep", 1, 2200, 700, [true, false, false, false, false, false, false]);
        let profiles = [sleep];
        // Tuesday 3:00 is in Monday's night.
        let event = find_next_profile_event(&utc, None, &profiles, at(1, 3, 0)).unwrap();
        assert_eq!(event, NextProfileEvent::WillDisable { at: at(1, 7, 0), active: "sleep".into(), clear_enable_override: false });
        assert_eq!(find_next_profile_event(&utc, None, &profiles, at(1, 8, 0)).unwrap().active(), None);
    }

    #[test]
    fn the_newer_profile_wins_an_overlap() {
        let older = scheduled("older", 1, 900, 1700, EVERY_DAY);
        let newer = scheduled("newer", 2, 1200, 1300, EVERY_DAY);
        let profiles = [older, newer];
        let event = find_next_profile_event(&utc, None, &profiles, at(0, 12, 30)).unwrap();
        assert_eq!(event.active(), Some("newer"));
        // Before the newer one starts, the older is on until it does.
        let event = find_next_profile_event(&utc, None, &profiles, at(0, 10, 0)).unwrap();
        assert_eq!(event, NextProfileEvent::WillDisable { at: at(0, 12, 0), active: "older".into(), clear_enable_override: false });
    }

    #[test]
    fn manual_overrides_last_until_the_schedule_next_changes() {
        let work = scheduled("work", 1, 900, 1700, WEEKDAYS);
        let profiles = [work];
        // On by hand Monday at 8: stays on through Monday's schedule.
        let on = NotificationProfileOverride::Enabled { id: "work".into(), ends_at: None, set_at: at(0, 8, 0) };
        let state = notification_profile_state(&utc, &profiles, Some(&on), at(0, 8, 30));
        assert_eq!(state.active.as_ref().map(|p| p.id.as_str()), Some("work"));
        assert_eq!(state.next_change, Some(at(0, 17, 0)));
        assert!(!override_expired(&utc, &on, &profiles, at(0, 16, 59)));
        assert!(override_expired(&utc, &on, &profiles, at(0, 17, 0)));
        assert_eq!(notification_profile_state(&utc, &profiles, Some(&on), at(0, 18, 0)).active, None);

        // On for an hour.
        let hour = NotificationProfileOverride::Enabled { id: "work".into(), ends_at: Some(at(5, 11, 0)), set_at: at(5, 10, 0) };
        assert!(notification_profile_state(&utc, &profiles, Some(&hour), at(5, 10, 30)).active.is_some());
        assert!(notification_profile_state(&utc, &profiles, Some(&hour), at(5, 11, 0)).active.is_none());

        // Off by hand during the schedule: off until it next starts.
        let off = NotificationProfileOverride::Disabled { at: at(0, 10, 0) };
        assert_eq!(notification_profile_state(&utc, &profiles, Some(&off), at(0, 12, 0)).active, None);
        assert!(notification_profile_state(&utc, &profiles, Some(&off), at(1, 9, 30)).active.is_some());

        // An override naming a deleted profile is gone.
        let deleted = [NotificationProfile { deleted_at: 5, ..profiles[0].clone() }];
        assert!(override_expired(&utc, &on, &deleted, at(0, 8, 30)));
    }

    #[test]
    fn active_profiles_let_through_what_they_allow() {
        let alice = ConversationId("d:alice".into());
        let bob = ConversationId("d:bob".into());
        let mut p = profile("p", 1);
        p.allow_all_calls = false;
        p.allowed_members.push(alice.clone());
        assert!(should_notify(None, &bob, false, false));
        assert!(should_notify(Some(&p), &alice, false, false));
        assert!(!should_notify(Some(&p), &bob, false, false));
        assert!(!should_notify(Some(&p), &bob, true, true));
        p.allow_all_calls = true;
        p.allow_all_mentions = true;
        assert!(should_notify(Some(&p), &bob, true, false));
        assert!(should_notify(Some(&p), &bob, false, true));
    }

    #[test]
    fn days_go_to_and_from_desktops_numbers() {
        let p = profile("p", 1);
        assert_eq!(p.day_numbers(), vec![1, 2, 3, 4, 5]);
        assert_eq!(NotificationProfile::days_from_numbers([0, 7, 9]), [true, false, false, false, false, false, true]);
    }

    #[tokio::test]
    async fn profiles_are_saved_deleted_and_merged() {
        let db = test_db().await;
        let mut work = scheduled("aa", 10, 900, 1700, WEEKDAYS);
        work.emoji = Some("💼".into());
        work.allowed_members = vec![ConversationId("d:x".into()), ConversationId("g:00".into())];
        let changes = db.edit_notification_profiles(NotificationProfileEdit::Save(work.clone())).await.unwrap();
        assert_eq!(changes.profiles, vec!["aa".to_string()]);
        assert_eq!(db.notification_profiles().await.unwrap(), vec![work.clone()]);
        // Saving the same is no change.
        assert!(db.edit_notification_profiles(NotificationProfileEdit::Save(work.clone())).await.unwrap().is_empty());

        let changes = db
            .edit_notification_profiles(NotificationProfileEdit::Enable { id: "aa".into(), ends_at: None, at: 5 })
            .await
            .unwrap();
        assert!(changes.override_);
        assert_eq!(
            db.notification_profile_override().await.unwrap(),
            Some(NotificationProfileOverride::Enabled { id: "aa".into(), ends_at: None, set_at: 5 })
        );

        // Deleting keeps it with its time and clears the override naming it.
        let changes = db.edit_notification_profiles(NotificationProfileEdit::Delete { id: "aa".into(), at: 50 }).await.unwrap();
        assert_eq!(changes, NotificationProfileChanges { profiles: vec!["aa".into()], override_: true });
        assert_eq!(db.notification_profiles().await.unwrap()[0].deleted_at, 50);
        assert_eq!(db.notification_profile_override().await.unwrap(), None);

        // A remote copy that isn't deleted keeps our deletion; a remote
        // deletion earlier than ours wins.
        db.edit_notification_profiles(NotificationProfileEdit::Merge(work.clone())).await.unwrap();
        assert_eq!(db.notification_profiles().await.unwrap()[0].deleted_at, 50);
        let earlier = NotificationProfile { deleted_at: 20, ..work.clone() };
        db.edit_notification_profiles(NotificationProfileEdit::Merge(earlier)).await.unwrap();
        assert_eq!(db.notification_profiles().await.unwrap()[0].deleted_at, 20);

        // A merged "on" override is dated when we learn of it, once.
        let on = Some(NotificationProfileOverride::Enabled { id: "bb".into(), ends_at: None, set_at: 0 });
        db.edit_notification_profiles(NotificationProfileEdit::MergeOverride { override_: on.clone(), at: 70 }).await.unwrap();
        let again = db.edit_notification_profiles(NotificationProfileEdit::MergeOverride { override_: on, at: 90 }).await.unwrap();
        assert!(again.is_empty());
        assert_eq!(
            db.notification_profile_override().await.unwrap(),
            Some(NotificationProfileOverride::Enabled { id: "bb".into(), ends_at: None, set_at: 70 })
        );
    }

    #[tokio::test]
    async fn silenced_follows_the_profile_on_now() {
        let db = test_db().await;
        let mut p = profile("cc", 1);
        p.allowed_members.push(ConversationId("d:ok".into()));
        db.edit_notification_profiles(NotificationProfileEdit::Save(p)).await.unwrap();
        let other = ConversationId("d:other".into());
        assert!(!db.notification_silenced(&other, false, false, 1000).await.unwrap());
        db.edit_notification_profiles(NotificationProfileEdit::Enable { id: "cc".into(), ends_at: None, at: 1000 }).await.unwrap();
        assert!(db.notification_silenced(&other, false, false, 2000).await.unwrap());
        assert!(!db.notification_silenced(&ConversationId("d:ok".into()), false, false, 2000).await.unwrap());
        db.edit_notification_profiles(NotificationProfileEdit::Disable { at: 3000 }).await.unwrap();
        assert!(!db.notification_silenced(&other, false, false, 4000).await.unwrap());
    }
}
