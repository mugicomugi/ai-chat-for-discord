use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::Serialize;

pub const MAX_MESSAGES: usize = 500;
pub const MAX_CHARS: usize = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("history は 0m〜1440m または 0h〜24h の整数で指定してください（例: 15m、2h）。")]
pub struct InvalidHistory;

pub fn parse_history(value: Option<&str>) -> Result<u32, InvalidHistory> {
    let value = value.unwrap_or("15m");
    let (digits, multiplier) = if let Some(v) = value.strip_suffix('m') {
        (v, 60)
    } else if let Some(v) = value.strip_suffix('h') {
        (v, 3600)
    } else {
        return Err(InvalidHistory);
    };
    if digits.is_empty() || !digits.bytes().all(|v| v.is_ascii_digit()) {
        return Err(InvalidHistory);
    }
    digits
        .parse::<u32>()
        .ok()
        .and_then(|v| v.checked_mul(multiplier))
        .filter(|v| *v <= 86_400)
        .ok_or(InvalidHistory)
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// Discord message IDs and interaction IDs share the Snowflake namespace.
    pub id: u64,
    pub at: DateTime<Utc>,
    pub author: String,
    pub content: String,
}

pub struct History {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

pub fn merge(
    entries: Vec<Entry>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    already_truncated: bool,
) -> History {
    let mut entries: Vec<_> = entries
        .into_iter()
        .filter(|e| e.at >= start && e.at < end && !e.content.is_empty())
        .collect();
    entries.sort_by_key(|e| (e.at, e.id));
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.id));
    let mut chars = 0;
    let mut selected = Vec::new();
    let mut truncated = already_truncated;
    for mut entry in entries.into_iter().rev() {
        if selected.len() == MAX_MESSAGES || chars == MAX_CHARS {
            truncated = true;
            break;
        }
        let size = entry.content.chars().count();
        if size > MAX_CHARS - chars {
            entry.content = entry.content.chars().take(MAX_CHARS - chars).collect();
            truncated = true;
        }
        chars += entry.content.chars().count();
        selected.push(entry);
    }
    selected.reverse();
    History {
        entries: selected,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn durations() {
        for (input, expected) in [
            (None, 900),
            (Some("0m"), 0),
            (Some("30m"), 1800),
            (Some("2h"), 7200),
            (Some("24h"), 86400),
        ] {
            assert_eq!(parse_history(input).unwrap(), expected);
        }
        for invalid in [
            "",
            "15",
            "-1m",
            "1.5h",
            "25h",
            "1441m",
            "999999999999h",
            "+1m",
            " 2h",
            "2d",
        ] {
            assert!(parse_history(Some(invalid)).is_err(), "{invalid}");
        }
    }

    #[test]
    fn boundaries_dedup_order_and_channel_independent_input() {
        let end = Utc::now();
        let start = end - Duration::minutes(15);
        let make = |id, at| Entry {
            id,
            at,
            author: "A".into(),
            content: "発言".into(),
        };
        let result = merge(
            vec![
                make(2, end),
                make(1, start),
                make(1, start),
                make(3, start - Duration::seconds(1)),
            ],
            start,
            end,
            false,
        );
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].id, 1);
        assert!(!result.truncated);
    }

    #[test]
    fn newest_count_and_unicode_budget() {
        let end = Utc::now();
        let at = end - Duration::seconds(1);
        let items = (0..501)
            .map(|id| Entry {
                id,
                at,
                author: "A".into(),
                content: "😀".into(),
            })
            .collect();
        let result = merge(items, at, end, false);
        assert_eq!(result.entries.len(), 500);
        assert_eq!(result.entries[0].id, 1);
        assert!(result.truncated);
        let result = merge(
            vec![Entry {
                id: 1,
                at,
                author: "A".into(),
                content: "日".repeat(60_001),
            }],
            at,
            end,
            false,
        );
        assert_eq!(result.entries[0].content.chars().count(), 60_000);
        assert!(result.truncated);
    }
}
