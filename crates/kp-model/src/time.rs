use std::borrow::Cow;
use std::fmt;

pub use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::{KpError, Result};

/// Returns the host's configured time zone (falling back safely if unavailable).
pub fn system_time_zone() -> TimeZone {
    TimeZone::system()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpochMs(pub i64);

impl EpochMs {
    pub const OPEN_END: Self = Self(i64::MAX);
}

impl fmt::Display for EpochMs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidityWindow {
    pub valid_from: EpochMs,
    pub valid_until: EpochMs,
}

impl ValidityWindow {
    pub fn contains(&self, at: EpochMs) -> bool {
        self.valid_from <= at && at < self.valid_until
    }

    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let valid_from = self.valid_from.max(other.valid_from);
        let valid_until = self.valid_until.min(other.valid_until);
        (valid_from < valid_until).then_some(Self {
            valid_from,
            valid_until,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitemporalStamp {
    pub valid: ValidityWindow,
    pub recorded_at: EpochMs,
    pub expired_at: Option<EpochMs>,
}

/// Civil date with ISO-8601 weekday (`1 = Monday` .. `7 = Sunday`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CivilDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub weekday_iso: u8,
}

/// One precomputed relative-date entry fed to extraction and checked deterministically on return.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarAnchor {
    pub label: String,
    pub date_iso: String,
    pub weekday_iso: u8,
    pub day_window: ValidityWindow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CalendarTable {
    pub reference_at: EpochMs,
    pub utc_offset_minutes: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone_iana: Option<String>,
    pub reference_date: CivilDate,
    pub anchors: Vec<CalendarAnchor>,
    #[serde(skip)]
    time_zone: Option<TimeZone>,
}

impl PartialEq for CalendarTable {
    fn eq(&self, other: &Self) -> bool {
        self.reference_at == other.reference_at
            && self.utc_offset_minutes == other.utc_offset_minutes
            && self.time_zone_iana == other.time_zone_iana
            && self.reference_date == other.reference_date
            && self.anchors == other.anchors
    }
}

impl Eq for CalendarTable {}

const MS_PER_SEC: i64 = 1_000;
const UNKNOWN_DATE_LABEL: &str = "日期未知";
const EPOCH_FALLBACK_RFC3339_UTC: &str = "1970-01-01T00:00:00Z";

fn fixed_offset_tz(utc_offset_minutes: i32) -> Option<TimeZone> {
    if !(-14 * 60..=14 * 60).contains(&utc_offset_minutes) {
        return None;
    }
    jiff::tz::Offset::from_seconds(utc_offset_minutes * 60)
        .ok()
        .map(TimeZone::fixed)
}

fn civil_date_from_jiff(date: jiff::civil::Date) -> CivilDate {
    CivilDate {
        year: i32::from(date.year()),
        month: date.month().unsigned_abs(),
        day: date.day().unsigned_abs(),
        weekday_iso: date.weekday().to_monday_one_offset().unsigned_abs(),
    }
}

fn local_day_window(
    day_date: jiff::civil::Date,
    tz: &TimeZone,
    field: &'static str,
) -> Result<ValidityWindow> {
    let next_date = day_date
        .checked_add(jiff::ToSpan::days(1))
        .map_err(|_| KpError::invalid(field, "day window overflow"))?;
    let start = day_date
        .to_zoned(tz.clone())
        .and_then(|z| z.start_of_day())
        .map_err(|_| KpError::invalid(field, "day window overflow"))?;
    let end = next_date
        .to_zoned(tz.clone())
        .and_then(|z| z.start_of_day())
        .map_err(|_| KpError::invalid(field, "day window overflow"))?;
    Ok(ValidityWindow {
        valid_from: EpochMs(start.timestamp().as_millisecond()),
        valid_until: EpochMs(end.timestamp().as_millisecond()),
    })
}

/// Formats `ts` as `YYYY-MM-DD` in `tz`.
///
/// Infallible: returns `"日期未知"` whenever `ts` overflows or falls outside the 4-digit civil
/// calendar (`0001-01-01..=9999-12-31`), including `EpochMs::OPEN_END`, `EpochMs(i64::MAX - 1)`,
/// and `EpochMs(i64::MIN)`.
pub fn format_date_iso_in_tz(ts: EpochMs, tz: &TimeZone) -> String {
    if ts == EpochMs::OPEN_END {
        return UNKNOWN_DATE_LABEL.to_owned();
    }
    let Ok(timestamp) = jiff::Timestamp::from_millisecond(ts.0) else {
        return UNKNOWN_DATE_LABEL.to_owned();
    };
    let zoned = timestamp.to_zoned(tz.clone());
    let year = zoned.year();
    if !(1..=9999).contains(&year) {
        return UNKNOWN_DATE_LABEL.to_owned();
    }
    format!("{year:04}-{:02}-{:02}", zoned.month(), zoned.day())
}

/// Formats Unix epoch seconds as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_epoch_sec_rfc3339_utc(epoch_sec: i64) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(epoch_sec) else {
        return EPOCH_FALLBACK_RFC3339_UTC.to_owned();
    };
    let utc = ts.to_zoned(TimeZone::UTC);
    if !(1..=9999).contains(&utc.year()) {
        return EPOCH_FALLBACK_RFC3339_UTC.to_owned();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        utc.year(),
        utc.month(),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second()
    )
}

/// Formats `EpochMs` as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_epoch_ms_rfc3339_utc(epoch_ms: EpochMs) -> String {
    format_epoch_sec_rfc3339_utc(epoch_ms.0.div_euclid(MS_PER_SEC))
}

/// Converts a numeric epoch value into milliseconds (magnitudes `< 100_000_000_000` are seconds).
pub fn normalize_numeric_epoch(value: i64) -> EpochMs {
    if value.unsigned_abs() < 100_000_000_000 {
        EpochMs(value.saturating_mul(MS_PER_SEC))
    } else {
        EpochMs(value)
    }
}

/// Parses a numeric Unix timestamp (seconds or milliseconds) or an ISO-8601 / RFC 3339 / civil
/// date-time string (`YYYY-MM-DD`, `YYYY/MM/DD`, `YYYY-MM-DD[T ]HH:MM[:SS][.fff][Z|±HH:MM|±HHMM|±HH]`)
/// into `EpochMs`. Naive inputs without an explicit timezone suffix use `default_tz`.
pub fn parse_timestamp_ms_in_tz(raw: &str, default_tz: &TimeZone) -> Option<EpochMs> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(int_val) = trimmed.parse::<i64>() {
        return Some(normalize_numeric_epoch(int_val));
    }
    let normalized: Cow<'_, str> = {
        let bytes = trimmed.as_bytes();
        if bytes.len() >= 10
            && bytes[0..4].iter().all(u8::is_ascii_digit)
            && bytes[4] == b'/'
            && bytes[5..7].iter().all(u8::is_ascii_digit)
            && bytes[7] == b'/'
            && bytes[8..10].iter().all(u8::is_ascii_digit)
        {
            let mut owned = trimmed.to_owned();
            owned.replace_range(4..5, "-");
            owned.replace_range(7..8, "-");
            Cow::Owned(owned)
        } else {
            Cow::Borrowed(trimmed)
        }
    };
    if let Ok(ts) = normalized.parse::<jiff::Timestamp>() {
        return Some(EpochMs(ts.as_millisecond()));
    }
    if let Ok(zoned) = normalized.parse::<jiff::Zoned>() {
        return Some(EpochMs(zoned.timestamp().as_millisecond()));
    }
    let pieces = jiff::fmt::temporal::Pieces::parse(normalized.as_ref()).ok()?;
    if pieces.offset().is_some() || pieces.time_zone_annotation().is_some() {
        return None;
    }
    if pieces.time().is_none() {
        let date = normalized.parse::<jiff::civil::Date>().ok()?;
        let start = date
            .to_zoned(default_tz.clone())
            .ok()?
            .start_of_day()
            .ok()?;
        Some(EpochMs(start.timestamp().as_millisecond()))
    } else {
        let dt = normalized.parse::<jiff::civil::DateTime>().ok()?;
        let zoned = dt.to_zoned(default_tz.clone()).ok()?;
        Some(EpochMs(zoned.timestamp().as_millisecond()))
    }
}

impl CalendarTable {
    /// Builds a deterministic 22-day anchor table (`-7..=+14` civil days around `reference_at`)
    /// in `tz`, with exact local `00:00` day boundaries (including across DST transitions).
    pub fn from_reference_in_tz(reference_at: EpochMs, tz: &TimeZone) -> Result<Self> {
        if reference_at == EpochMs::OPEN_END {
            return Err(KpError::invalid("reference_at", "timestamp overflow"));
        }
        let ref_ts = jiff::Timestamp::from_millisecond(reference_at.0)
            .map_err(|_| KpError::invalid("reference_at", "timestamp overflow"))?;
        let ref_zoned = ref_ts.to_zoned(tz.clone());
        let ref_date = ref_zoned.date();
        let utc_offset_minutes = ref_zoned.offset().seconds() / 60;
        let reference_date = civil_date_from_jiff(ref_date);
        let weekday_names = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
        let mut anchors = Vec::with_capacity(22);
        for delta in -7_i64..=14_i64 {
            let day_date = ref_date
                .checked_add(jiff::ToSpan::days(delta))
                .map_err(|_| KpError::invalid("reference_at", "day overflow"))?;
            let civil = civil_date_from_jiff(day_date);
            let day_window = local_day_window(day_date, tz, "reference_at")?;
            let wd = weekday_names[usize::from(civil.weekday_iso.saturating_sub(1)) % 7];
            let rel = match delta {
                -1 => "昨天",
                0 => "今天",
                1 => "明天",
                2 => "后天",
                _ => "",
            };
            let label = if rel.is_empty() {
                format!("{delta:+03}d ({wd})")
            } else {
                format!("{rel} ({wd})")
            };
            anchors.push(CalendarAnchor {
                label,
                date_iso: format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day),
                weekday_iso: civil.weekday_iso,
                day_window,
            });
        }
        Ok(Self {
            reference_at,
            utc_offset_minutes,
            time_zone_iana: tz.iana_name().map(str::to_owned),
            reference_date,
            anchors,
            time_zone: Some(tz.clone()),
        })
    }

    /// Looks up the precomputed day window for an exact `YYYY-MM-DD` date inside the anchor table.
    pub fn window_for_date(&self, date_iso: &str) -> Option<ValidityWindow> {
        let trimmed = date_iso.trim();
        self.anchors
            .iter()
            .find(|anchor| anchor.date_iso == trimmed)
            .map(|anchor| anchor.day_window)
    }

    /// Resolves one model-emitted calendar token to its local-day window. Accepts an anchor label
    /// from this table (`"明天 (周五)"` or `"明天"`), a Chinese weekday/weekend/relative day token,
    /// a multi-day `<start_day><connector><end_day>` token, or any ISO `YYYY-MM-DD` date.
    pub fn day_window_for_token(&self, token: &str) -> Result<ValidityWindow> {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return Err(KpError::invalid("date_iso", "date token must be nonempty"));
        }
        if let Some(anchor) = self.anchors.iter().find(|a| {
            a.date_iso == trimmed
                || a.label == trimmed
                || a.label
                    .split(" (")
                    .next()
                    .is_some_and(|head| head == trimmed)
        }) {
            return Ok(anchor.day_window);
        }

        let tz = self.effective_time_zone()?;
        if let Some(ep) =
            match_day_endpoint_at(trimmed, 0).filter(|ep| ep.byte_end == trimmed.len())
        {
            return self
                .window_for_single_endpoint(ep.kind, &tz)
                .ok_or_else(|| KpError::invalid("date_iso", "date out of supported range"));
        }

        let scanned = scan_day_endpoints(trimmed);
        if scanned.len() == 2 && scanned[1].byte_end == trimmed.len() {
            let leading = trimmed[..scanned[0].byte_start].trim();
            if (leading.is_empty() || leading == "从")
                && is_range_connector(
                    &trimmed[..scanned[0].byte_start],
                    &trimmed[scanned[0].byte_end..scanned[1].byte_start],
                )
            {
                return self
                    .window_for_endpoint_pair(scanned[0].kind, scanned[1].kind, &tz)
                    .ok_or_else(|| {
                        KpError::invalid("date_range", "end date must not precede start date")
                    });
            }
        }

        let bytes = trimmed.as_bytes();
        if bytes.len() != 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || !bytes[0..4].iter().all(u8::is_ascii_digit)
            || !bytes[5..7].iter().all(u8::is_ascii_digit)
            || !bytes[8..10].iter().all(u8::is_ascii_digit)
        {
            return Err(KpError::invalid(
                "date_iso",
                "expected YYYY-MM-DD or a CalendarTable anchor/weekday label",
            ));
        }
        let date: jiff::civil::Date = trimmed
            .parse()
            .map_err(|_| KpError::invalid("date_iso", "invalid civil date"))?;
        if !(1970..=2200).contains(&date.year()) {
            return Err(KpError::invalid("date_iso", "date out of supported range"));
        }
        local_day_window(date, &tz, "date_iso")
    }

    /// Returns the effective `TimeZone` for this table (`self.time_zone` -> `self.time_zone_iana`
    /// -> fixed `self.utc_offset_minutes`).
    pub fn effective_time_zone(&self) -> Result<TimeZone> {
        if let Some(tz) = &self.time_zone {
            return Ok(tz.clone());
        }
        if let Some(iana) = &self.time_zone_iana {
            if let Ok(tz) = TimeZone::get(iana) {
                return Ok(tz);
            }
        }
        fixed_offset_tz(self.utc_offset_minutes)
            .ok_or_else(|| KpError::invalid("utc_offset_minutes", "invalid UTC offset"))
    }

    /// Deterministically converts a model-emitted calendar range into `[valid_from, valid_until)`:
    /// starts at local midnight of `start`, ends at the end of `end` (or of `start` when `end` is
    /// absent), and is open-ended (`EpochMs::OPEN_END`) when `open_ended` is set.
    pub fn resolve_date_range(
        &self,
        start: &str,
        end: Option<&str>,
        open_ended: bool,
    ) -> Result<ValidityWindow> {
        let start_trimmed = start.trim();
        let start_win = self.day_window_for_token(start_trimmed)?;
        if open_ended {
            return Ok(ValidityWindow {
                valid_from: start_win.valid_from,
                valid_until: EpochMs::OPEN_END,
            });
        }
        let Some(end_token) = end.map(str::trim).filter(|e| !e.is_empty()) else {
            return Ok(start_win);
        };
        if let (Some(s_ep), Some(e_ep)) = (
            match_day_endpoint_at(start_trimmed, 0).filter(|ep| ep.byte_end == start_trimmed.len()),
            match_day_endpoint_at(end_token, 0).filter(|ep| ep.byte_end == end_token.len()),
        ) {
            if matches!(
                e_ep.kind,
                DayEndpointKind::Weekday {
                    qualifier: WeekQualifier::Unqualified | WeekQualifier::Next,
                    ..
                } | DayEndpointKind::Weekend(WeekQualifier::Unqualified)
            ) {
                let tz = self.effective_time_zone()?;
                if let Some(pair_win) = self.window_for_endpoint_pair(s_ep.kind, e_ep.kind, &tz) {
                    return Ok(pair_win);
                }
            }
        }
        let valid_until = self.day_window_for_token(end_token)?.valid_until;
        if valid_until <= start_win.valid_from {
            return Err(KpError::invalid(
                "date_range",
                "end date must not precede start date",
            ));
        }
        Ok(ValidityWindow {
            valid_from: start_win.valid_from,
            valid_until,
        })
    }

    /// Counts distinct ISO date, Chinese weekday/weekend, and relative-day tokens appearing in `text`.
    pub fn count_date_or_weekday_tokens(&self, text: &str) -> usize {
        scan_day_endpoints(text).len()
    }

    /// Returns `true` if `text` contains a reschedule transition (`从周一改到周三`, `推迟到周五`, etc.)
    /// between two date/weekday endpoints.
    pub fn has_reschedule_pair(&self, text: &str) -> bool {
        let tokens = scan_day_endpoints(text);
        tokens.windows(2).any(|pair| {
            let gap = &text[pair[0].byte_end..pair[1].byte_start];
            is_reschedule_gap(gap)
        })
    }

    /// Counts independent date/weekday mentions in `text` after collapsing range pairs (`周一到周三`)
    /// and reschedule pairs (`从周一改到周三`) into a single mention.
    pub fn count_independent_date_mentions(&self, text: &str) -> usize {
        let tokens = scan_day_endpoints(text);
        if tokens.is_empty() {
            return 0;
        }
        let mut count = 1usize;
        for pair in tokens.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let gap = &text[left.byte_end..right.byte_start];
            let linked =
                is_range_connector(&text[..left.byte_start], gap) || is_reschedule_gap(gap);
            if !linked {
                count += 1;
            }
        }
        count
    }

    /// Collects all candidate single-day, onset-open, and multi-day range `ValidityWindow`s
    /// referenced by weekday, relative-day, or ISO date tokens in `text`.
    pub fn candidate_weekday_windows(&self, text: &str) -> Vec<ValidityWindow> {
        let Ok(tz) = self.effective_time_zone() else {
            return Vec::new();
        };
        let tokens = scan_day_endpoints(text);
        let mut out: Vec<ValidityWindow> = Vec::new();
        let mut push_unique = |w: ValidityWindow| {
            if !out.contains(&w) {
                out.push(w);
            }
        };

        for (idx, tok) in tokens.iter().enumerate() {
            if let Some(single) = self.window_for_single_endpoint(tok.kind, &tz) {
                push_unique(single);
                let onset = apply_onset_open_end(single, &text[tok.byte_end..]);
                push_unique(onset);
            }
            if let DayEndpointKind::Weekday {
                qualifier: WeekQualifier::Unqualified,
                weekday_iso,
            } = tok.kind
            {
                for prev in &tokens[..idx] {
                    if let DayEndpointKind::Weekday {
                        qualifier: qual @ (WeekQualifier::Next | WeekQualifier::Prev),
                        ..
                    } = prev.kind
                    {
                        if let Some(inherited) = self.window_for_single_endpoint(
                            DayEndpointKind::Weekday {
                                qualifier: qual,
                                weekday_iso,
                            },
                            &tz,
                        ) {
                            push_unique(inherited);
                        }
                    }
                }
            }
        }

        for pair in tokens.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            let gap = &text[left.byte_end..right.byte_start];
            if is_range_connector(&text[..left.byte_start], gap) {
                if let Some(win) = self.window_for_endpoint_pair(left.kind, right.kind, &tz) {
                    push_unique(win);
                }
            }
            if is_reschedule_gap(gap) {
                if let Some(win) =
                    self.window_for_reschedule_target(left.kind, right.kind, gap, &tz)
                {
                    push_unique(win);
                    push_unique(apply_onset_open_end(win, &text[right.byte_end..]));
                }
            }
        }
        out
    }

    fn reference_civil_date_in_tz(&self, tz: &TimeZone) -> Option<jiff::civil::Date> {
        let ref_ts = jiff::Timestamp::from_millisecond(self.reference_at.0).ok()?;
        Some(ref_ts.to_zoned(tz.clone()).date())
    }

    fn resolve_standalone_dates(
        &self,
        kind: DayEndpointKind,
        ref_date: jiff::civil::Date,
    ) -> Option<(jiff::civil::Date, jiff::civil::Date)> {
        let ref_wd = i64::from(self.reference_date.weekday_iso);
        match kind {
            DayEndpointKind::IsoDate(d) => Some((d, d)),
            DayEndpointKind::RelativeDay(delta) => {
                let d = ref_date.checked_add(jiff::ToSpan::days(delta)).ok()?;
                Some((d, d))
            }
            DayEndpointKind::Weekend(qual) => {
                let sat_delta = match qual {
                    WeekQualifier::Next => (8 - ref_wd) + 5,
                    WeekQualifier::Prev => -((ref_wd - 1) + 2),
                    WeekQualifier::Unqualified if ref_wd <= 6 => 6 - ref_wd,
                    WeekQualifier::Unqualified => 6,
                };
                let sat = ref_date.checked_add(jiff::ToSpan::days(sat_delta)).ok()?;
                let sun = sat.checked_add(jiff::ToSpan::days(1)).ok()?;
                Some((sat, sun))
            }
            DayEndpointKind::Weekday {
                qualifier,
                weekday_iso,
            } => {
                let delta = match qualifier {
                    WeekQualifier::Next => (8 - ref_wd) + (weekday_iso - 1),
                    WeekQualifier::Prev => -((ref_wd - 1) + (8 - weekday_iso)),
                    WeekQualifier::Unqualified => (weekday_iso - ref_wd).rem_euclid(7),
                };
                let d = ref_date.checked_add(jiff::ToSpan::days(delta)).ok()?;
                Some((d, d))
            }
        }
    }

    fn window_for_single_endpoint(
        &self,
        kind: DayEndpointKind,
        tz: &TimeZone,
    ) -> Option<ValidityWindow> {
        let ref_date = self.reference_civil_date_in_tz(tz)?;
        let (start_date, end_inclusive) = self.resolve_standalone_dates(kind, ref_date)?;
        window_from_inclusive_dates(start_date, end_inclusive, tz)
    }

    fn window_for_endpoint_pair(
        &self,
        start_kind: DayEndpointKind,
        end_kind: DayEndpointKind,
        tz: &TimeZone,
    ) -> Option<ValidityWindow> {
        let ref_date = self.reference_civil_date_in_tz(tz)?;
        let (start_date, _) = self.resolve_standalone_dates(start_kind, ref_date)?;
        let start_wd = i64::from(start_date.weekday().to_monday_one_offset());
        let end_inclusive = match end_kind {
            DayEndpointKind::Weekday {
                qualifier: WeekQualifier::Unqualified,
                weekday_iso: end_wd,
            } => {
                let offset = (end_wd - start_wd).rem_euclid(7);
                start_date.checked_add(jiff::ToSpan::days(offset)).ok()?
            }
            DayEndpointKind::Weekday {
                qualifier: WeekQualifier::Next,
                weekday_iso: end_wd,
            } => {
                let ref_wd = i64::from(self.reference_date.weekday_iso);
                let delta = (8 - ref_wd) + (end_wd - 1);
                let mut d = ref_date.checked_add(jiff::ToSpan::days(delta)).ok()?;
                if d < start_date {
                    d = d.checked_add(jiff::ToSpan::days(7)).ok()?;
                }
                d
            }
            DayEndpointKind::Weekend(WeekQualifier::Unqualified) => {
                let sat_offset = (6 - start_wd).rem_euclid(7);
                let sat = start_date
                    .checked_add(jiff::ToSpan::days(sat_offset))
                    .ok()?;
                sat.checked_add(jiff::ToSpan::days(1)).ok()?
            }
            other => {
                let (_, incl_end) = self.resolve_standalone_dates(other, ref_date)?;
                if incl_end < start_date {
                    return None;
                }
                incl_end
            }
        };
        if end_inclusive < start_date {
            return None;
        }
        window_from_inclusive_dates(start_date, end_inclusive, tz)
    }

    fn window_for_reschedule_target(
        &self,
        old_kind: DayEndpointKind,
        new_kind: DayEndpointKind,
        gap: &str,
        tz: &TimeZone,
    ) -> Option<ValidityWindow> {
        let ref_date = self.reference_civil_date_in_tz(tz)?;
        if let (
            DayEndpointKind::Weekday {
                qualifier: WeekQualifier::Next,
                ..
            },
            DayEndpointKind::Weekday {
                qualifier: WeekQualifier::Unqualified,
                weekday_iso: end_wd,
            },
        ) = (old_kind, new_kind)
        {
            if !gap.contains("提前") {
                let (old_date, _) = self.resolve_standalone_dates(old_kind, ref_date)?;
                let old_wd = i64::from(old_date.weekday().to_monday_one_offset());
                let offset = (end_wd - old_wd).rem_euclid(7);
                let target_date = old_date.checked_add(jiff::ToSpan::days(offset)).ok()?;
                return window_from_inclusive_dates(target_date, target_date, tz);
            }
        }
        let (start_date, end_inclusive) = self.resolve_standalone_dates(new_kind, ref_date)?;
        window_from_inclusive_dates(start_date, end_inclusive, tz)
    }

    /// Deterministically resolves Chinese weekday/weekend tokens, reschedule targets
    /// (`例会从周一改到周三` -> `周三`), and multi-day weekday spans (`周五到周日`, `下周一到周三`,
    /// `周六至下周二`, etc.) via `jiff` in `self.effective_time_zone()`.
    pub fn resolve_chinese_weekday_expression(&self, text: &str) -> Option<ValidityWindow> {
        let tz = self.effective_time_zone().ok()?;
        let tokens = scan_day_endpoints(text);

        for i in (0..tokens.len().saturating_sub(1)).rev() {
            let (left, right) = (tokens[i], tokens[i + 1]);
            let gap = &text[left.byte_end..right.byte_start];
            if (left.kind.is_chinese_weekday_or_weekend()
                || right.kind.is_chinese_weekday_or_weekend())
                && is_reschedule_gap(gap)
            {
                if i + 2 < tokens.len()
                    && is_range_connector(
                        &text[left.byte_end..right.byte_start],
                        &text[right.byte_end..tokens[i + 2].byte_start],
                    )
                {
                    if let Some(range_win) =
                        self.window_for_endpoint_pair(right.kind, tokens[i + 2].kind, &tz)
                    {
                        return Some(range_win);
                    }
                }
                if right.kind.is_chinese_weekday_or_weekend() {
                    let win = self.window_for_reschedule_target(left.kind, right.kind, gap, &tz)?;
                    return Some(apply_onset_open_end(win, &text[right.byte_end..]));
                }
                return None;
            }
        }

        for pair in tokens.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            if (left.kind.is_chinese_weekday_or_weekend()
                || right.kind.is_chinese_weekday_or_weekend())
                && is_range_connector(
                    &text[..left.byte_start],
                    &text[left.byte_end..right.byte_start],
                )
            {
                if let Some(win) = self.window_for_endpoint_pair(left.kind, right.kind, &tz) {
                    return Some(win);
                }
            }
        }

        for tok in &tokens {
            if tok.kind.is_chinese_weekday_or_weekend() {
                let win = self.window_for_single_endpoint(tok.kind, &tz)?;
                return Some(apply_onset_open_end(win, &text[tok.byte_end..]));
            }
        }

        None
    }

    /// Deterministically resolves multi-day, rescheduled, or single-day Chinese/ISO/relative date
    /// anchors in `text`. Appending `"起"` or `"开始"` immediately after a single anchor opens the upper bound.
    pub fn resolve_relative_expression(&self, text: &str) -> Option<ValidityWindow> {
        let tz = self.effective_time_zone().ok()?;
        let tokens = scan_day_endpoints(text);

        for i in (0..tokens.len().saturating_sub(1)).rev() {
            let (left, right) = (tokens[i], tokens[i + 1]);
            let gap = &text[left.byte_end..right.byte_start];
            if is_reschedule_gap(gap) {
                if i + 2 < tokens.len()
                    && is_range_connector(
                        &text[left.byte_end..right.byte_start],
                        &text[right.byte_end..tokens[i + 2].byte_start],
                    )
                {
                    if let Some(range_win) =
                        self.window_for_endpoint_pair(right.kind, tokens[i + 2].kind, &tz)
                    {
                        return Some(range_win);
                    }
                }
                let win = self.window_for_reschedule_target(left.kind, right.kind, gap, &tz)?;
                return Some(apply_onset_open_end(win, &text[right.byte_end..]));
            }
        }

        for pair in tokens.windows(2) {
            let (left, right) = (pair[0], pair[1]);
            if is_range_connector(
                &text[..left.byte_start],
                &text[left.byte_end..right.byte_start],
            ) {
                if let Some(win) = self.window_for_endpoint_pair(left.kind, right.kind, &tz) {
                    return Some(win);
                }
            }
        }

        for tok in &tokens {
            if matches!(tok.kind, DayEndpointKind::IsoDate(_)) {
                let win = self.window_for_single_endpoint(tok.kind, &tz)?;
                return Some(apply_onset_open_end(win, &text[tok.byte_end..]));
            }
        }

        if let Some(win) = self.resolve_chinese_weekday_expression(text) {
            return Some(win);
        }

        for tok in &tokens {
            if matches!(tok.kind, DayEndpointKind::RelativeDay(_)) {
                let win = self.window_for_single_endpoint(tok.kind, &tz)?;
                return Some(apply_onset_open_end(win, &text[tok.byte_end..]));
            }
        }
        None
    }
}

fn window_from_inclusive_dates(
    start_date: jiff::civil::Date,
    end_inclusive: jiff::civil::Date,
    tz: &TimeZone,
) -> Option<ValidityWindow> {
    let start_win = local_day_window(start_date, tz, "date_range").ok()?;
    let end_win = local_day_window(end_inclusive, tz, "date_range").ok()?;
    (end_win.valid_until > start_win.valid_from).then_some(ValidityWindow {
        valid_from: start_win.valid_from,
        valid_until: end_win.valid_until,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WeekQualifier {
    Next,
    Prev,
    Unqualified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DayEndpointKind {
    IsoDate(jiff::civil::Date),
    Weekend(WeekQualifier),
    Weekday {
        qualifier: WeekQualifier,
        weekday_iso: i64,
    },
    RelativeDay(i64),
}

impl DayEndpointKind {
    fn is_chinese_weekday_or_weekend(self) -> bool {
        matches!(self, Self::Weekend(_) | Self::Weekday { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScannedDayEndpoint {
    byte_start: usize,
    byte_end: usize,
    kind: DayEndpointKind,
}

fn match_chinese_weekday_or_weekend_prefix(rest: &str) -> Option<(usize, DayEndpointKind)> {
    for (kw, qual) in [
        ("下周末", WeekQualifier::Next),
        ("上周末", WeekQualifier::Prev),
        ("这周末", WeekQualifier::Unqualified),
        ("本周末", WeekQualifier::Unqualified),
        ("周末", WeekQualifier::Unqualified),
    ] {
        if rest.starts_with(kw) {
            return Some((kw.len(), DayEndpointKind::Weekend(qual)));
        }
    }

    const WEEKDAY_SUFFIXES: [(&str, i64); 8] = [
        ("一", 1),
        ("二", 2),
        ("三", 3),
        ("四", 4),
        ("五", 5),
        ("六", 6),
        ("日", 7),
        ("天", 7),
    ];

    for (prefix, qual) in [
        ("下星期", WeekQualifier::Next),
        ("下礼拜", WeekQualifier::Next),
        ("下周", WeekQualifier::Next),
        ("上星期", WeekQualifier::Prev),
        ("上礼拜", WeekQualifier::Prev),
        ("上周", WeekQualifier::Prev),
        ("这星期", WeekQualifier::Unqualified),
        ("本星期", WeekQualifier::Unqualified),
        ("这礼拜", WeekQualifier::Unqualified),
        ("本礼拜", WeekQualifier::Unqualified),
        ("这周", WeekQualifier::Unqualified),
        ("本周", WeekQualifier::Unqualified),
        ("星期", WeekQualifier::Unqualified),
        ("礼拜", WeekQualifier::Unqualified),
        ("周", WeekQualifier::Unqualified),
    ] {
        if let Some(after_prefix) = rest.strip_prefix(prefix) {
            for (suf, wd) in WEEKDAY_SUFFIXES {
                if after_prefix.starts_with(suf) {
                    return Some((
                        prefix.len() + suf.len(),
                        DayEndpointKind::Weekday {
                            qualifier: qual,
                            weekday_iso: wd,
                        },
                    ));
                }
            }
        }
    }
    None
}

fn match_day_endpoint_at(text: &str, byte_idx: usize) -> Option<ScannedDayEndpoint> {
    let rest = text.get(byte_idx..)?;
    if rest.len() >= 10 && rest.is_char_boundary(10) {
        let prev_is_digit = text[..byte_idx]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_digit());
        let next_is_digit = rest[10..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit());
        let head = &rest.as_bytes()[..10];
        if !prev_is_digit
            && !next_is_digit
            && head[0..4].iter().all(u8::is_ascii_digit)
            && head[4] == b'-'
            && head[5..7].iter().all(u8::is_ascii_digit)
            && head[7] == b'-'
            && head[8..10].iter().all(u8::is_ascii_digit)
        {
            if let Ok(date) = rest[..10].parse::<jiff::civil::Date>() {
                if (1970..=2200).contains(&date.year()) {
                    let mut byte_end = byte_idx + 10;
                    let after_iso = &text[byte_end..];
                    let ws_len = after_iso.len() - after_iso.trim_start_matches([' ', '\t']).len();
                    let after_ws = &after_iso[ws_len..];
                    if let Some((open_len, close_ch)) = after_ws
                        .strip_prefix('(')
                        .map(|_| (1usize, ')'))
                        .or_else(|| after_ws.strip_prefix('（').map(|_| ('（'.len_utf8(), '）')))
                    {
                        let inside = &after_ws[open_len..];
                        let inside_ws = inside.len() - inside.trim_start_matches([' ', '\t']).len();
                        if let Some((wd_len, _)) =
                            match_chinese_weekday_or_weekend_prefix(&inside[inside_ws..])
                        {
                            let after_wd =
                                inside[inside_ws + wd_len..].trim_start_matches([' ', '\t']);
                            if after_wd.starts_with(close_ch) {
                                let consumed =
                                    (after_iso.len() - after_wd.len()) + close_ch.len_utf8();
                                byte_end = byte_idx + 10 + consumed;
                            }
                        }
                    }
                    return Some(ScannedDayEndpoint {
                        byte_start: byte_idx,
                        byte_end,
                        kind: DayEndpointKind::IsoDate(date),
                    });
                }
            }
        }
    }

    if let Some((len, kind)) = match_chinese_weekday_or_weekend_prefix(rest) {
        return Some(ScannedDayEndpoint {
            byte_start: byte_idx,
            byte_end: byte_idx + len,
            kind,
        });
    }

    for (kw, delta) in [
        ("大后天", 3),
        ("后天", 2),
        ("明天", 1),
        ("明日", 1),
        ("今天", 0),
        ("今日", 0),
        ("前天", -2),
        ("昨天", -1),
        ("昨日", -1),
    ] {
        if rest.starts_with(kw) {
            return Some(ScannedDayEndpoint {
                byte_start: byte_idx,
                byte_end: byte_idx + kw.len(),
                kind: DayEndpointKind::RelativeDay(delta),
            });
        }
    }
    None
}

fn scan_day_endpoints(text: &str) -> Vec<ScannedDayEndpoint> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        if let Some(tok) = match_day_endpoint_at(text, cursor) {
            cursor = tok.byte_end;
            out.push(tok);
        } else if let Some(ch) = text[cursor..].chars().next() {
            cursor += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn is_clause_break(c: char) -> bool {
    matches!(
        c,
        '，' | ',' | '。' | '；' | ';' | '！' | '!' | '？' | '?' | '\n' | '\r'
    )
}

fn is_sentence_break(c: char) -> bool {
    matches!(c, '。' | '；' | ';' | '！' | '!' | '？' | '?' | '\n' | '\r')
}

fn is_reschedule_connector(trimmed_gap: &str) -> bool {
    const RESCHEDULE_SUFFIXES: &[&str] = &[
        "改到",
        "改为",
        "改成",
        "改至",
        "推迟到",
        "推迟至",
        "延后到",
        "延后至",
        "延期到",
        "延期至",
        "顺延到",
        "顺延至",
        "提前到",
        "提前至",
        "移到",
        "移至",
        "挪到",
        "挪至",
        "调整到",
        "调整至",
        "调整为",
        "换到",
        "换至",
        "换成",
        "调到",
        "调至",
        "改期到",
        "改期至",
        "改期为",
        "变更到",
        "变更至",
        "变更为",
    ];
    let normalized = trimmed_gap
        .strip_suffix('了')
        .unwrap_or(trimmed_gap)
        .trim_end();
    RESCHEDULE_SUFFIXES
        .iter()
        .any(|suf| normalized.ends_with(suf))
}

fn is_reschedule_gap(gap: &str) -> bool {
    !gap.chars().any(is_sentence_break) && is_reschedule_connector(gap.trim())
}

fn is_range_connector(prefix_before_start: &str, gap: &str) -> bool {
    if gap.chars().any(is_clause_break) {
        return false;
    }
    let trimmed = gap.trim();
    if matches!(
        trimmed,
        "到" | "至"
            | "~"
            | "～"
            | "-"
            | "—"
            | "–"
            | "--"
            | "——"
            | "一直到"
            | "持续到"
            | "起至"
            | "起到"
            | "开始到"
            | "开始至"
    ) {
        return true;
    }
    if is_reschedule_connector(trimmed) {
        return false;
    }
    let ends_with_to = trimmed.ends_with('到') || trimmed.ends_with('至');
    if !ends_with_to || trimmed.chars().count() > 12 {
        return false;
    }
    let stem = trimmed
        .strip_suffix("一直到")
        .or_else(|| trimmed.strip_suffix("持续到"))
        .or_else(|| trimmed.strip_suffix('到'))
        .or_else(|| trimmed.strip_suffix('至'))
        .unwrap_or("")
        .trim();
    if matches!(
        stem,
        "早" | "早上"
            | "上午"
            | "中午"
            | "下午"
            | "傍晚"
            | "晚上"
            | "晚"
            | "凌晨"
            | "全天"
            | "起"
            | "开始"
            | "一直"
            | "持续"
            | "一直持续"
    ) {
        return true;
    }
    let same_clause_prefix = match prefix_before_start.rfind(is_clause_break) {
        Some(idx) => {
            let ch_len = prefix_before_start[idx..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
            &prefix_before_start[idx + ch_len..]
        }
        None => prefix_before_start,
    };
    same_clause_prefix.contains('从')
}

/// Returns true if `text` contains an explicit numeric date (`YYYY-MM-DD`, `YYYY/MM/DD`, `M月D日`,
/// `M月D号`, or `M/D`), indicating a Chinese weekday in the same text is parenthetical rather than
/// the sole date anchor.
pub fn contains_explicit_numeric_date(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    for i in 0..len {
        // 1) YYYY-MM-DD / YYYY/MM/DD / YYYY.MM.DD
        if i + 7 < len
            && (i == 0 || !chars[i - 1].is_ascii_digit())
            && chars[i..i + 4].iter().all(char::is_ascii_digit)
            && matches!(chars[i + 4], '-' | '/' | '.')
        {
            let sep = chars[i + 4];
            let mut m_end = i + 5;
            while m_end < len && m_end < i + 7 && chars[m_end].is_ascii_digit() {
                m_end += 1;
            }
            if m_end > i + 5 && m_end < len && chars[m_end] == sep {
                let mut d_end = m_end + 1;
                while d_end < len && d_end < m_end + 3 && chars[d_end].is_ascii_digit() {
                    d_end += 1;
                }
                if d_end > m_end + 1 && (d_end == len || !chars[d_end].is_ascii_digit()) {
                    return true;
                }
            }
        }

        // 2) M月D日 / M月D号
        if chars[i] == '月' && i >= 1 && chars[i - 1].is_ascii_digit() {
            let mut d_end = i + 1;
            while d_end < len && d_end <= i + 2 && chars[d_end].is_ascii_digit() {
                d_end += 1;
            }
            if d_end > i + 1 && d_end < len && matches!(chars[d_end], '日' | '号') {
                return true;
            }
        }

        // 3) M/D (bounded by non-ASCII-alphanumeric and non-slash/dash/dot)
        if chars[i] == '/'
            && i >= 1
            && i + 1 < len
            && chars[i - 1].is_ascii_digit()
            && chars[i + 1].is_ascii_digit()
        {
            let m_start = if i >= 2 && chars[i - 2].is_ascii_digit() {
                i - 2
            } else {
                i - 1
            };
            let d_end = if i + 2 < len && chars[i + 2].is_ascii_digit() {
                i + 3
            } else {
                i + 2
            };
            let left_ok = m_start == 0
                || (!chars[m_start - 1].is_ascii_alphanumeric()
                    && !matches!(chars[m_start - 1], '/' | '-' | '.'));
            let right_ok = d_end == len
                || (!chars[d_end].is_ascii_alphanumeric()
                    && !matches!(chars[d_end], '/' | '-' | '.'));
            if left_ok && right_ok {
                let m_str: String = chars[m_start..i].iter().collect();
                let d_str: String = chars[i + 1..d_end].iter().collect();
                if let (Ok(m), Ok(d)) = (m_str.parse::<u8>(), d_str.parse::<u8>()) {
                    if (1..=12).contains(&m) && (1..=31).contains(&d) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn apply_onset_open_end(window: ValidityWindow, tail: &str) -> ValidityWindow {
    let trimmed = tail.trim_start();
    if trimmed.starts_with('起') || trimmed.starts_with("开始") {
        ValidityWindow {
            valid_from: window.valid_from,
            valid_until: EpochMs::OPEN_END,
        }
    } else {
        window
    }
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
