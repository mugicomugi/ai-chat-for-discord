use chrono::{Duration, Utc};
use discord_discussion_bot::{
    history::{Entry, merge},
    output::split_message,
};

#[test]
fn plain_text_split_is_lossless_and_respects_utf16_limit() {
    let text = format!(
        "{}\n{}\n{}",
        "あ😀".repeat(2300),
        "続き\n".repeat(300),
        "end"
    );
    let parts = split_message(&text);
    assert_eq!(parts.concat(), text);
    assert!(parts.iter().all(|part| part.encode_utf16().count() <= 2000));
}

#[test]
fn history_uses_latest_entries_without_filling_from_outside_window() {
    let end = Utc::now();
    let start = end - Duration::minutes(15);
    let entry = |id, at, content: &str| Entry {
        id,
        at,
        author: "user".into(),
        content: content.into(),
    };
    let result = merge(
        vec![
            entry(1, start - Duration::milliseconds(1), "excluded"),
            entry(2, start, "included start"),
            entry(3, end - Duration::milliseconds(1), "included end"),
            entry(4, end, "excluded current"),
            entry(5, end + Duration::seconds(1), "excluded future"),
        ],
        start,
        end,
        false,
    );
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [2, 3]
    );
}
