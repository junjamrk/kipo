use proptest::prelude::*;
use proptest::test_runner::RngSeed;

use super::*;

#[test]
fn validity_is_half_open() {
    let window = ValidityWindow {
        valid_from: EpochMs(3),
        valid_until: EpochMs(5),
    };
    assert!(!window.contains(EpochMs(2)));
    assert!(window.contains(EpochMs(3)));
    assert!(window.contains(EpochMs(4)));
    assert!(!window.contains(EpochMs(5)));
    assert_eq!(
        window.intersect(&ValidityWindow {
            valid_from: EpochMs(5),
            valid_until: EpochMs::OPEN_END
        }),
        None
    );
}

#[test]
fn empty_and_reversed_intervals_never_contain_time() {
    assert!(!ValidityWindow {
        valid_from: EpochMs(1),
        valid_until: EpochMs(1)
    }
    .contains(EpochMs(1)));
    assert!(!ValidityWindow {
        valid_from: EpochMs(2),
        valid_until: EpochMs(1)
    }
    .contains(EpochMs(1)));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, rng_seed: RngSeed::Fixed(102), ..ProptestConfig::default() })]
    #[test]
    fn intersection_is_commutative_and_exact(a in any::<i64>(), b in any::<i64>(), c in any::<i64>(), d in any::<i64>(), t in any::<i64>()) {
        let left = ValidityWindow { valid_from: EpochMs(a.min(b)), valid_until: EpochMs(a.max(b)) };
        let right = ValidityWindow { valid_from: EpochMs(c.min(d)), valid_until: EpochMs(c.max(d)) };
        prop_assert_eq!(left.intersect(&right), right.intersect(&left));
        prop_assert_eq!(left.intersect(&right).is_some_and(|w| w.contains(EpochMs(t))), left.contains(EpochMs(t)) && right.contains(EpochMs(t)));
    }
}

#[test]
fn calendar_table_resolves_relative_weekdays_and_onset_ranges() -> crate::Result<()> {
    // 2026-11-19T12:00:00Z is Thursday (weekday_iso == 4) in UTC+8 (2026-11-19 20:00).
    let shanghai = TimeZone::get("Asia/Shanghai")
        .map_err(|_| crate::KpError::invalid("test", "Asia/Shanghai"))?;
    let table = CalendarTable::from_reference_in_tz(EpochMs(1_795_089_600_000), &shanghai)?;
    assert_eq!(table.reference_date.year, 2026);
    assert_eq!(table.reference_date.month, 11);
    assert_eq!(table.reference_date.day, 19);
    assert_eq!(table.reference_date.weekday_iso, 4);

    let fri = table
        .window_for_date("2026-11-20")
        .ok_or_else(|| crate::KpError::invalid("test", "missing friday"))?;
    assert_eq!(
        table.resolve_relative_expression("听说周五健身房闭馆"),
        Some(fri)
    );
    assert_eq!(table.resolve_relative_expression("明天有读书会"), Some(fri));

    let next_wed = table
        .window_for_date("2026-11-25")
        .ok_or_else(|| crate::KpError::invalid("test", "missing next wednesday"))?;
    assert_eq!(
        table.resolve_relative_expression("下周三起暂停报销审批"),
        Some(ValidityWindow {
            valid_from: next_wed.valid_from,
            valid_until: EpochMs::OPEN_END,
        })
    );
    assert_eq!(table.resolve_relative_expression("出差只坐高铁"), None);
    Ok(())
}

#[test]
fn model_date_ranges_resolve_deterministically_without_epoch_copying() -> crate::Result<()> {
    // Thursday 2026-11-19 20:00 in UTC+8.
    let shanghai = TimeZone::get("Asia/Shanghai")
        .map_err(|_| crate::KpError::invalid("test", "Asia/Shanghai"))?;
    let table = CalendarTable::from_reference_in_tz(EpochMs(1_795_089_600_000), &shanghai)?;
    let fri = table
        .window_for_date("2026-11-20")
        .ok_or_else(|| crate::KpError::invalid("test", "missing friday"))?;
    let sun = table
        .window_for_date("2026-11-22")
        .ok_or_else(|| crate::KpError::invalid("test", "missing sunday"))?;

    // ISO date, full anchor label, and short anchor label all resolve to the same local day.
    assert_eq!(table.resolve_date_range("2026-11-20", None, false)?, fri);
    assert_eq!(table.resolve_date_range("明天 (周五)", None, false)?, fri);
    assert_eq!(table.resolve_date_range("明天", None, false)?, fri);
    // Multi-day range ends at the end of the last day.
    assert_eq!(
        table.resolve_date_range("2026-11-20", Some("2026-11-22"), false)?,
        ValidityWindow {
            valid_from: fri.valid_from,
            valid_until: sun.valid_until,
        }
    );
    // Open-ended ranges never expire.
    assert_eq!(
        table
            .resolve_date_range("2026-11-25", None, true)?
            .valid_until,
        EpochMs::OPEN_END
    );
    // Dates outside the anchor table still resolve via civil-day arithmetic (2027-01-01 UTC+8).
    let ny = table.resolve_date_range("2027-01-01", None, false)?;
    assert_eq!(ny.valid_from, EpochMs(1_798_761_600_000 - 8 * 3_600_000));
    assert_eq!(ny.valid_until.0 - ny.valid_from.0, 86_400_000);
    // Malformed, impossible, and reversed ranges are rejected.
    assert!(table.resolve_date_range("2026-02-30", None, false).is_err());
    assert!(table
        .resolve_date_range("1795089600000", None, false)
        .is_err());
    assert!(table
        .resolve_date_range("2026-11-22", Some("2026-11-20"), false)
        .is_err());
    Ok(())
}

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn shanghai_early_morning_uses_local_date_and_local_midnight_windows() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    // 2026-11-22 07:00:00 +08:00 == 2026-11-21 23:00:00Z (UTC is still the previous day).
    let morning = EpochMs(1_795_302_000_000);
    assert_eq!(format_date_iso_in_tz(morning, &TimeZone::UTC), "2026-11-21");
    assert_eq!(format_date_iso_in_tz(morning, &shanghai), "2026-11-22");

    let table = CalendarTable::from_reference_in_tz(morning, &shanghai)?;
    assert_eq!(table.utc_offset_minutes, 480);
    assert_eq!(table.reference_date.year, 2026);
    assert_eq!(table.reference_date.month, 11);
    assert_eq!(table.reference_date.day, 22);
    assert_eq!(table.reference_date.weekday_iso, 7);

    let today_anchor = table
        .anchors
        .iter()
        .find(|a| a.label.starts_with("今天"))
        .ok_or_else(|| crate::KpError::invalid("test", "missing today anchor"))?;
    assert_eq!(today_anchor.date_iso, "2026-11-22");

    let tomorrow_anchor = table
        .anchors
        .iter()
        .find(|a| a.label.starts_with("明天"))
        .ok_or_else(|| crate::KpError::invalid("test", "missing tomorrow anchor"))?;
    assert_eq!(tomorrow_anchor.date_iso, "2026-11-23");

    // Exact local 00:00 boundaries in Asia/Shanghai: [2026-11-23T00:00:00+08:00, 2026-11-24T00:00:00+08:00).
    let expected_start = parse_timestamp_ms_in_tz("2026-11-23T00:00:00+08:00", &shanghai)
        .ok_or_else(|| crate::KpError::invalid("test", "parse start"))?;
    let expected_end = parse_timestamp_ms_in_tz("2026-11-24T00:00:00+08:00", &shanghai)
        .ok_or_else(|| crate::KpError::invalid("test", "parse end"))?;
    assert_eq!(expected_start, EpochMs(1_795_363_200_000));
    assert_eq!(expected_end, EpochMs(1_795_449_600_000));
    assert_eq!(
        tomorrow_anchor.day_window,
        ValidityWindow {
            valid_from: expected_start,
            valid_until: expected_end,
        }
    );
    assert_eq!(
        table.resolve_date_range("明天", None, false)?,
        tomorrow_anchor.day_window
    );

    // JSON roundtrip of CalendarTable skips TimeZone cleanly and preserves equality.
    let encoded = serde_json::to_string(&table)?;
    let decoded: CalendarTable = serde_json::from_str(&encoded)?;
    assert_eq!(decoded, table);
    Ok(())
}

#[test]
fn dst_transitions_preserve_exact_local_midnight_boundaries() -> TestResult {
    let ny = TimeZone::get("America/New_York")?;

    // 2026-03-07T17:00:00Z == 2026-03-07 12:00:00-05:00; tomorrow (2026-03-08) springs forward (23h).
    let march_ref = parse_timestamp_ms_in_tz("2026-03-07T12:00:00-05:00", &ny)
        .ok_or_else(|| crate::KpError::invalid("test", "parse march_ref"))?;
    let spring_table = CalendarTable::from_reference_in_tz(march_ref, &ny)?;
    assert_eq!(spring_table.utc_offset_minutes, -300);
    let spring_day = spring_table
        .window_for_date("2026-03-08")
        .ok_or_else(|| crate::KpError::invalid("test", "missing 2026-03-08"))?;
    let spring_start = parse_timestamp_ms_in_tz("2026-03-08T00:00:00-05:00", &ny)
        .ok_or_else(|| crate::KpError::invalid("test", "spring start"))?;
    let spring_end = parse_timestamp_ms_in_tz("2026-03-09T00:00:00-04:00", &ny)
        .ok_or_else(|| crate::KpError::invalid("test", "spring end"))?;
    assert_eq!(spring_day.valid_from, spring_start);
    assert_eq!(spring_day.valid_until, spring_end);
    assert_eq!(
        spring_day.valid_until.0 - spring_day.valid_from.0,
        82_800_000
    );

    // Fall-back day outside the March anchor table (2026-11-01, 25h = 90_000_000 ms) also uses IANA TZ.
    let fall_day = spring_table.day_window_for_token("2026-11-01")?;
    assert_eq!(fall_day.valid_until.0 - fall_day.valid_from.0, 90_000_000);
    Ok(())
}

#[test]
fn format_date_iso_is_infallible_across_sentinels_and_offsets() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    assert_eq!(
        format_date_iso_in_tz(EpochMs(i64::MAX - 1), &shanghai),
        "日期未知"
    );
    assert_eq!(
        format_date_iso_in_tz(EpochMs::OPEN_END, &shanghai),
        "日期未知"
    );
    assert_eq!(
        format_date_iso_in_tz(EpochMs(i64::MIN), &shanghai),
        "日期未知"
    );

    let minus_one = TimeZone::fixed(jiff::tz::offset(-1));
    assert_eq!(
        format_date_iso_in_tz(EpochMs(0), &TimeZone::UTC),
        "1970-01-01"
    );
    assert_eq!(format_date_iso_in_tz(EpochMs(0), &minus_one), "1969-12-31");
    // 2026-11-19T20:00:00Z is 2026-11-19 in UTC and 2026-11-20 in Asia/Shanghai (+08:00).
    let utc_20h = EpochMs(1_795_118_400_000);
    assert_eq!(format_date_iso_in_tz(utc_20h, &TimeZone::UTC), "2026-11-19");
    assert_eq!(format_date_iso_in_tz(utc_20h, &shanghai), "2026-11-20");
    Ok(())
}

#[test]
fn rfc3339_formatting_and_timestamp_parsing_handle_offsets_fractions_and_epochs() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    assert_eq!(format_epoch_sec_rfc3339_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_epoch_sec_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
    assert_eq!(
        format_epoch_sec_rfc3339_utc(i64::MAX),
        "1970-01-01T00:00:00Z"
    );
    assert_eq!(
        format_epoch_sec_rfc3339_utc(i64::MIN),
        "1970-01-01T00:00:00Z"
    );
    let base_utc = parse_timestamp_ms_in_tz("2026-11-21T07:16:20Z", &TimeZone::UTC);
    assert_eq!(
        base_utc.map(format_epoch_ms_rfc3339_utc).as_deref(),
        Some("2026-11-21T07:16:20Z")
    );

    // Explicit timezone suffixes: +08:00, +0800, +08, and negative offsets.
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T15:16:20+08:00", &TimeZone::UTC),
        base_utc
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21 15:16:20+0800", &TimeZone::UTC),
        base_utc
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21 15:16:20+08", &TimeZone::UTC),
        base_utc
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T02:16:20-05:00", &TimeZone::UTC),
        base_utc
    );

    // Date-only (`YYYY-MM-DD` and `YYYY/MM/DD`) and naive datetime with Asia/Shanghai.
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21", &shanghai),
        parse_timestamp_ms_in_tz("2026/11/21", &shanghai)
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21 15:16:20", &shanghai),
        base_utc
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T15:16+08:00", &TimeZone::UTC),
        parse_timestamp_ms_in_tz("2026-11-21T07:16:00Z", &TimeZone::UTC)
    );

    // Fractional milliseconds (1, 3, and 6 digits).
    let base_ms = base_utc.map_or(0, |t| t.0);
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T07:16:20.345Z", &TimeZone::UTC),
        Some(EpochMs(base_ms + 345))
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T07:16:20.5Z", &TimeZone::UTC),
        Some(EpochMs(base_ms + 500))
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T07:16:20.123987+00:00", &TimeZone::UTC),
        Some(EpochMs(base_ms + 123))
    );

    // Numeric epoch seconds vs milliseconds normalization.
    assert_eq!(
        normalize_numeric_epoch(1_795_000_000),
        EpochMs(1_795_000_000_000)
    );
    assert_eq!(
        normalize_numeric_epoch(1_795_000_000_123),
        EpochMs(1_795_000_000_123)
    );
    assert_eq!(normalize_numeric_epoch(i64::MIN), EpochMs(i64::MIN));
    assert_eq!(
        parse_timestamp_ms_in_tz("1795000000", &TimeZone::UTC),
        Some(EpochMs(1_795_000_000_000))
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("1795000000123", &TimeZone::UTC),
        Some(EpochMs(1_795_000_000_123))
    );

    // Invalid inputs return `None` cleanly.
    assert_eq!(parse_timestamp_ms_in_tz("", &TimeZone::UTC), None);
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-02-30T00:00:00Z", &TimeZone::UTC),
        None
    );
    assert_eq!(
        parse_timestamp_ms_in_tz("2026-11-21T15:16:20+99:00", &TimeZone::UTC),
        None
    );
    Ok(())
}

#[test]
fn sunday_chinese_weekdays_resolve_to_upcoming_days_and_last_week_via_jiff() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;
    // Sunday 2026-11-22 10:00:00 +08:00 (`ref_wd = 7`).
    let sunday = parse_timestamp_ms_in_tz("2026-11-22T10:00:00+08:00", &shanghai)
        .ok_or_else(|| crate::KpError::invalid("test", "parse sunday"))?;
    let table = CalendarTable::from_reference_in_tz(sunday, &shanghai)?;
    assert_eq!(table.reference_date.weekday_iso, 7);

    let upcoming_fri = table.day_window_for_token("2026-11-27")?;
    for phrase in [
        "周五园区电路检修",
        "本周五园区电路检修",
        "这周五园区电路检修",
    ] {
        assert_eq!(
            table.resolve_chinese_weekday_expression(phrase),
            Some(upcoming_fri),
            "{phrase} on Sunday must land on upcoming Friday 2026-11-27 (+5d), never -2d"
        );
    }
    assert_eq!(
        table.resolve_chinese_weekday_expression("下周五园区电路检修"),
        Some(upcoming_fri)
    );

    // Previous ISO week (`上周五` -> 2026-11-13, `-9d`; `上周一` -> 2026-11-09, `-13d` beyond `-7`).
    assert_eq!(
        table.resolve_chinese_weekday_expression("上周五归档已完成"),
        Some(table.day_window_for_token("2026-11-13")?)
    );
    assert_eq!(
        table.resolve_chinese_weekday_expression("上周一会议纪要"),
        Some(table.day_window_for_token("2026-11-09")?)
    );

    // Weekend tokens (`周末` / `本周末` -> upcoming Sat..Mon `2026-11-28..2026-11-30`).
    let upcoming_sat = table.day_window_for_token("2026-11-28")?;
    let upcoming_sun = table.day_window_for_token("2026-11-29")?;
    let expected_weekend = ValidityWindow {
        valid_from: upcoming_sat.valid_from,
        valid_until: upcoming_sun.valid_until,
    };
    assert_eq!(
        table.resolve_chinese_weekday_expression("本周末机房维护"),
        Some(expected_weekend)
    );

    assert!(contains_explicit_numeric_date("2026-11-20 周五"));
    assert!(contains_explicit_numeric_date("11月20日（周五）"));
    assert!(contains_explicit_numeric_date("周五(11/20)检修"));
    assert!(!contains_explicit_numeric_date(
        "本周五下午 15:00 在 3F 阅览室"
    ));
    Ok(())
}

#[test]
fn calendar_table_json_roundtrip_preserves_iana_dst_23h_and_25h_windows() -> TestResult {
    let ny = TimeZone::get("America/New_York")?;
    let march_ref = parse_timestamp_ms_in_tz("2026-03-07T12:00:00-05:00", &ny)
        .ok_or_else(|| crate::KpError::invalid("test", "parse march_ref"))?;
    let spring_table = CalendarTable::from_reference_in_tz(march_ref, &ny)?;
    assert_eq!(
        spring_table.time_zone_iana.as_deref(),
        Some("America/New_York")
    );

    let json_text = serde_json::to_string(&spring_table)?;
    let decoded: CalendarTable = serde_json::from_str(&json_text)?;
    assert_eq!(decoded, spring_table);

    // After JSON deserialization (`time_zone` skipped), `time_zone_iana` still resolves DST windows!
    let spring_23h = decoded.day_window_for_token("2026-03-08")?;
    assert_eq!(
        spring_23h.valid_until.0 - spring_23h.valid_from.0,
        23 * 3_600_000
    );

    let summer_edt = decoded.day_window_for_token("2026-06-15")?;
    let expected_summer_start = parse_timestamp_ms_in_tz("2026-06-15T00:00:00-04:00", &ny)
        .ok_or_else(|| crate::KpError::invalid("test", "parse summer_edt"))?;
    assert_eq!(summer_edt.valid_from, expected_summer_start);
    assert_eq!(
        summer_edt.valid_until.0 - summer_edt.valid_from.0,
        24 * 3_600_000
    );

    let fall_25h = decoded.day_window_for_token("2026-11-01")?;
    assert_eq!(
        fall_25h.valid_until.0 - fall_25h.valid_from.0,
        25 * 3_600_000
    );
    Ok(())
}

#[test]
fn multi_day_chinese_weekday_relative_and_iso_spans_resolve_accurately() -> TestResult {
    let shanghai = TimeZone::get("Asia/Shanghai")?;

    // 1) Spoken on Sunday 2026-11-22 (`ref_wd = 7`):
    let sunday = parse_timestamp_ms_in_tz("2026-11-22T10:00:00+08:00", &shanghai)
        .ok_or_else(|| crate::KpError::invalid("test", "parse sunday"))?;
    let sun_table = CalendarTable::from_reference_in_tz(sunday, &shanghai)?;

    let fri_27 = sun_table.day_window_for_token("2026-11-27")?;
    let sun_29 = sun_table.day_window_for_token("2026-11-29")?;
    let expected_fri_to_sun = ValidityWindow {
        valid_from: fri_27.valid_from,
        valid_until: sun_29.valid_until,
    };
    for expr in [
        "周五到周日阅览室闭馆",
        "本周五至周日阅览室闭馆",
        "从周五下午到周日闭馆",
        "周五一直到周日闭馆",
        "周五持续到周日闭馆",
        "星期五~星期天闭馆",
        "礼拜五—礼拜日闭馆",
    ] {
        assert_eq!(
            sun_table.resolve_chinese_weekday_expression(expr),
            Some(expected_fri_to_sun),
            "failed on Sunday expression: {expr}"
        );
    }
    assert_eq!(
        sun_table.resolve_date_range("周五", Some("周日"), false)?,
        expected_fri_to_sun
    );

    let mon_23 = sun_table.day_window_for_token("2026-11-23")?;
    let wed_25 = sun_table.day_window_for_token("2026-11-25")?;
    let expected_next_mon_to_wed = ValidityWindow {
        valid_from: mon_23.valid_from,
        valid_until: wed_25.valid_until,
    };
    assert_eq!(
        sun_table.resolve_chinese_weekday_expression("下周一到周三封闭培训"),
        Some(expected_next_mon_to_wed)
    );
    assert_eq!(
        sun_table.resolve_date_range("下周一", Some("周三"), false)?,
        expected_next_mon_to_wed
    );

    // 2) Spoken on Thursday 2026-11-19 (`ref_wd = 4`):
    let thursday = parse_timestamp_ms_in_tz("2026-11-19T20:00:00+08:00", &shanghai)
        .ok_or_else(|| crate::KpError::invalid("test", "parse thursday"))?;
    let thu_table = CalendarTable::from_reference_in_tz(thursday, &shanghai)?;

    assert_eq!(
        thu_table.resolve_chinese_weekday_expression("下周一到周三封闭培训"),
        Some(expected_next_mon_to_wed)
    );

    let sat_21 = thu_table.day_window_for_token("2026-11-21")?;
    let next_tue_24 = thu_table.day_window_for_token("2026-11-24")?;
    let expected_sat_to_next_tue = ValidityWindow {
        valid_from: sat_21.valid_from,
        valid_until: next_tue_24.valid_until,
    };
    for expr in ["周六至下周二机房维护", "周六到下周二机房维护"] {
        assert_eq!(
            thu_table.resolve_chinese_weekday_expression(expr),
            Some(expected_sat_to_next_tue),
            "failed on Thursday expression: {expr}"
        );
    }

    let fri_20 = thu_table.day_window_for_token("2026-11-20")?;
    let sun_22 = thu_table.day_window_for_token("2026-11-22")?;
    let expected_20_to_22 = ValidityWindow {
        valid_from: fri_20.valid_from,
        valid_until: sun_22.valid_until,
    };
    assert_eq!(
        thu_table.resolve_relative_expression("明天到周日书展"),
        Some(expected_20_to_22)
    );
    assert_eq!(
        thu_table.resolve_relative_expression("2026-11-20 - 2026-11-22 书展"),
        Some(expected_20_to_22)
    );
    assert_eq!(
        thu_table.resolve_relative_expression("2026-11-20（周五）至2026-11-22（周日）书展"),
        Some(expected_20_to_22)
    );

    let today_19 = thu_table.day_window_for_token("今天")?;
    let day_after_tomorrow_21 = thu_table.day_window_for_token("后天")?;
    assert_eq!(
        thu_table.resolve_relative_expression("今天到后天仓库盘点"),
        Some(ValidityWindow {
            valid_from: today_19.valid_from,
            valid_until: day_after_tomorrow_21.valid_until,
        })
    );

    // Reschedule verbs (`改到` / `改为` / `推迟到` / `提前到` / `移至` / `调整到` / `换到` / `调到`)
    // resolve to `<new_day>` rather than `<old_day>` or a multi-day range!
    let mon_single = thu_table.day_window_for_token("周一")?;
    let wed_single = thu_table.day_window_for_token("周三")?;
    for expr in [
        "例会从周一改到周三",
        "周一的例会改为周三",
        "原定周一分享改成周三",
        "周一评审推迟到周三",
        "周一评审延期至周三",
        "周五巡检提前到周三",
        "周一值班调整到周三",
        "周一排班换到周三",
        "周一排班移至周三",
        "周一排班调到周三",
    ] {
        assert_eq!(
            thu_table.resolve_chinese_weekday_expression(expr),
            Some(wed_single),
            "expected Wednesday target for reschedule expression: {expr}"
        );
        assert_eq!(
            thu_table.resolve_relative_expression(expr),
            Some(wed_single),
            "expected Wednesday target in resolve_relative_expression: {expr}"
        );
    }
    assert_eq!(
        thu_table.resolve_chinese_weekday_expression("例会从周一改到周三起每周举行"),
        Some(ValidityWindow {
            valid_from: wed_single.valid_from,
            valid_until: EpochMs::OPEN_END,
        })
    );

    // Multi-clause independent mentions (`周一交提纲，周三做分享`) expose both candidate windows!
    let cands = thu_table.candidate_weekday_windows("周一交提纲，周三做分享");
    assert!(cands.contains(&mon_single));
    assert!(cands.contains(&wed_single));
    assert_eq!(
        thu_table.count_independent_date_mentions("周一交提纲，周三做分享"),
        2
    );
    assert_eq!(
        thu_table.count_independent_date_mentions("例会从周一改到周三"),
        1
    );
    assert_eq!(
        thu_table.count_independent_date_mentions("周一到周三封闭培训"),
        1
    );
    Ok(())
}
