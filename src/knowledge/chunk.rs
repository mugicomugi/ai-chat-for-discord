//! Splits a document's text into overlapping pieces for embedding. Pure functions only: the
//! upload counts chunks with the same code the worker later stores them with.

/// Longest chunk, in characters (Unicode scalar values).
pub const TARGET_CHARS: usize = 600;
/// How much of the previous chunk a chunk repeats, at most.
pub const OVERLAP_CHARS: usize = 100;
/// A chunk is never cut shorter than this, so boundaries cannot produce tiny pieces.
const MIN_CHARS: usize = 200;
const MAX_HEADING_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Markdown only: the headings above the text, outermost first ("章 > 節").
    pub heading: Option<String>,
    pub content: String,
}

/// Cuts at the end of a paragraph that ends a sentence first, then at a sentence end
/// (`。！？．.!?`), then at any other paragraph or line break, then anywhere. Line breaks rank
/// low because PDF text breaks lines (often with blank lines between them) wherever the page
/// did. Markdown is split at headings first and every chunk carries the heading path above
/// it. Never returns empty chunks.
pub fn chunk(text: &str, markdown: bool) -> Vec<Chunk> {
    let sections = if markdown {
        sections(text)
    } else {
        vec![(None, text)]
    };
    sections
        .into_iter()
        .flat_map(|(heading, body)| {
            split(body).into_iter().map(move |content| Chunk {
                heading: heading.clone(),
                content,
            })
        })
        .collect()
}

/// Markdown bodies with the heading path above each. Headings inside fenced code are text.
fn sections(text: &str) -> Vec<(Option<String>, &str)> {
    let mut result = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut body_start = 0;
    let mut offset = 0;
    let current = |path: &[(usize, String)]| {
        let joined = path
            .iter()
            .map(|(_, text)| text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" > ");
        (!joined.is_empty()).then(|| joined.chars().take(MAX_HEADING_CHARS).collect())
    };
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        if let Some((c, run, bare)) = fence_marker(line) {
            fence = match fence {
                None => Some((c, run)),
                // A closing fence uses the same character, at least as many times, and nothing
                // else.
                Some((open, length)) if c == open && run >= length && bare => None,
                open => open,
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some((level, title)) = heading(line) {
            result.push((current(&path), &text[body_start..line_start]));
            while path.last().is_some_and(|(l, _)| *l >= level) {
                path.pop();
            }
            path.push((level, title));
            body_start = offset;
        }
    }
    result.push((current(&path), &text[body_start..]));
    result
}

/// `(character, run length, only the run on the line)` of a ``` or ~~~ fence line.
fn fence_marker(line: &str) -> Option<(char, usize, bool)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let c = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = trimmed.chars().take_while(|x| *x == c).count();
    (run >= 3).then(|| (c, run, trimmed[run..].trim().is_empty()))
}

/// An ATX heading (`#` to `######`) and its text.
fn heading(line: &str) -> Option<(usize, String)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let rest = rest.trim();
    // A closing run of #s is dropped only when separated by a space (`# C#` keeps its #).
    let stripped = rest.trim_end_matches('#');
    let text = if stripped.is_empty() || stripped.ends_with([' ', '\t']) {
        stripped.trim_end()
    } else {
        rest
    };
    Some((level, text.to_owned()))
}

/// Overlapping pieces of one section's text, each at most `TARGET_CHARS` characters.
fn split(body: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let n = chars.len();
    let byte = |i: usize| if i == n { body.len() } else { chars[i].0 };
    let paragraph = paragraph_starts(&chars);
    let mut pieces = Vec::new();
    let mut start = 0;
    loop {
        while start < n && chars[start].1.is_whitespace() {
            start += 1;
        }
        if start >= n {
            break;
        }
        if n - start <= TARGET_CHARS {
            pieces.push(body[byte(start)..].trim().to_owned());
            break;
        }
        let limit = start + TARGET_CHARS;
        let lowest = start + MIN_CHARS;
        let last = |boundary: &dyn Fn(usize) -> bool| (lowest..=limit).rev().find(|&i| boundary(i));
        let cut = last(&|i| paragraph[i] && closes_sentence(&chars, i))
            .or_else(|| last(&|i| sentence_end(&chars, i)))
            .or_else(|| last(&|i| paragraph[i] || chars[i - 1].1 == '\n'))
            .unwrap_or_else(|| hard_cut(&chars, lowest, limit));
        pieces.push(body[byte(start)..byte(cut)].trim().to_owned());
        // Repeat the end of this chunk, starting at a sentence, line or word if there is one.
        let back = cut - OVERLAP_CHARS;
        let first = |boundary: &dyn Fn(usize) -> bool| (back..cut).find(|&i| boundary(i));
        start = first(&|i| sentence_end(&chars, i))
            .or_else(|| first(&|i| chars[i - 1].1 == '\n'))
            .or_else(|| first(&|i| chars[i - 1].1.is_whitespace()))
            .unwrap_or_else(|| {
                let mut i = back;
                while i < cut && (extends(chars[i].1) || chars[i - 1].1 == '\u{200D}') {
                    i += 1;
                }
                i
            });
    }
    pieces.retain(|piece| !piece.is_empty());
    pieces
}

/// `result[i]`: position `i` starts a paragraph (it follows a blank line). One entry per
/// position, end of text included.
fn paragraph_starts(chars: &[(usize, char)]) -> Vec<bool> {
    let mut result = vec![false; chars.len() + 1];
    let mut line_blank = true;
    let mut previous_blank = false;
    for (i, (_, c)) in chars.iter().enumerate() {
        if *c == '\n' {
            if line_blank && i > 0 {
                previous_blank = true;
            }
            line_blank = true;
            continue;
        }
        if line_blank && previous_blank && !c.is_whitespace() {
            result[i] = true;
        }
        if !c.is_whitespace() {
            line_blank = false;
            previous_blank = false;
        }
    }
    result
}

/// The text before position `i`, blank lines and spaces aside, ends a sentence (or there is
/// none).
fn closes_sentence(chars: &[(usize, char)], i: usize) -> bool {
    match (0..i).rev().find(|&j| !chars[j].1.is_whitespace()) {
        Some(j) => sentence_end(chars, j + 1),
        None => true,
    }
}

/// Position `i` (0 < i < len) directly follows the end of a sentence. ASCII `.!?` count only
/// before whitespace, so `3.14` and `example.com` stay whole; closing brackets and quotes after
/// the mark stay with the sentence.
fn sentence_end(chars: &[(usize, char)], i: usize) -> bool {
    if i == 0 || i >= chars.len() {
        return false;
    }
    let next = chars[i].1;
    if closing(next) {
        return false;
    }
    let mut j = i - 1;
    while j > 0 && closing(chars[j].1) {
        j -= 1;
    }
    match chars[j].1 {
        '。' | '！' | '？' | '．' => true,
        '.' | '!' | '?' => next.is_whitespace(),
        _ => false,
    }
}

fn closing(c: char) -> bool {
    matches!(
        c,
        '」' | '』' | '）' | ')' | '"' | '\'' | '”' | '’' | '】' | '］' | ']'
    )
}

/// The last position at most `limit` that does not split a character from what modifies it
/// (combining marks, variation selectors, skin tones, zero-width joiners).
fn hard_cut(chars: &[(usize, char)], lowest: usize, limit: usize) -> usize {
    let mut cut = limit;
    while cut > lowest
        && cut < chars.len()
        && (extends(chars[cut].1) || chars[cut - 1].1 == '\u{200D}')
    {
        cut -= 1;
    }
    cut
}

fn extends(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{3099}'..='\u{309A}'
        | '\u{200D}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{1F3FB}'..='\u{1F3FF}'
        | '\u{E0020}'..='\u{E007F}'
        | '\u{E0100}'..='\u{E01EF}')
}

/// Joins two consecutive chunks of one document, dropping the text the second repeats (at
/// least 8 bytes, at most what an overlap can be).
pub fn join_overlapping(first: &str, second: &str) -> String {
    let longest = first.len().min(second.len()).min((OVERLAP_CHARS + 20) * 4);
    let overlap = (1..=longest)
        .rev()
        .filter(|&k| second.is_char_boundary(k))
        .find(|&k| k >= 8 && first.ends_with(&second[..k]));
    match overlap {
        Some(k) => format!("{first}{}", &second[k..]),
        None => format!("{first}\n{second}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lengths(chunks: &[Chunk]) -> Vec<usize> {
        chunks.iter().map(|c| c.content.chars().count()).collect()
    }

    #[test]
    fn short_text_is_one_chunk_and_blank_text_none() {
        assert_eq!(
            chunk("  短い文章です。\n", false),
            [Chunk {
                heading: None,
                content: "短い文章です。".into()
            }]
        );
        assert!(chunk(" \n\n\t ", false).is_empty());
        assert!(chunk("", true).is_empty());
    }

    #[test]
    fn japanese_sentences_are_kept_whole_with_overlap() {
        let text: String = (0..60)
            .map(|i| format!("これは{i}番目の文で、ナレッジベースの分割を確かめるためのものです。"))
            .collect();
        let chunks = chunk(&text, false);
        assert!(chunks.len() > 3);
        for (i, c) in chunks.iter().enumerate() {
            let count = c.content.chars().count();
            assert!(count <= TARGET_CHARS, "{count}");
            assert!(c.content.ends_with('。'), "chunk {i} ends mid-sentence");
            assert!(
                c.content.starts_with("これは"),
                "chunk {i} starts mid-sentence"
            );
        }
        // Consecutive chunks share text, and joining them restores the original.
        let mut joined = chunks[0].content.clone();
        for c in &chunks[1..] {
            let overlap_start: String = c.content.chars().take(20).collect();
            assert!(joined.contains(&overlap_start));
            joined = join_overlapping(&joined, &c.content);
        }
        assert_eq!(joined, text);
    }

    #[test]
    fn paragraphs_are_preferred_over_sentences() {
        let first = "あ。".repeat(150);
        let second = "い。".repeat(150);
        let third = "う。".repeat(150);
        let text = format!("{first}\n\n{second}\n\n{third}");
        let chunks = chunk(&text, false);
        assert_eq!(chunks[0].content, first);
        assert!(chunks[1].content.ends_with(&second));
        assert!(chunks.last().unwrap().content.ends_with(&third));
    }

    /// PDF text: lines wrapped where the page ended them, with or without blank lines between
    /// them (pdf-extract adds one when the line spacing is wide).
    #[test]
    fn wrapped_lines_do_not_end_chunks() {
        let text: String = (0..80)
            .map(|i| {
                let filler = "資料の分割を確かめる文章".chars().cycle();
                format!(
                    "第{i}条{}。",
                    filler.take(15 + (i * 37) % 60).collect::<String>()
                )
            })
            .collect();
        for separator in ["\n", "\n\n"] {
            let chars: Vec<char> = text.chars().collect();
            let wrapped = chars
                .chunks(40)
                .map(|line| line.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join(separator);
            let chunks = chunk(&wrapped, false);
            assert!(chunks.len() > 5);
            for (i, c) in chunks.iter().enumerate() {
                let count = c.content.chars().count();
                assert!(count <= TARGET_CHARS, "{count}");
                assert!(c.content.ends_with('。'), "chunk {i} ends mid-sentence");
                assert!(c.content.starts_with('第'), "chunk {i} starts mid-sentence");
            }
            let mut joined = chunks[0].content.clone();
            for c in &chunks[1..] {
                joined = join_overlapping(&joined, &c.content);
            }
            assert_eq!(joined, wrapped);
        }
    }

    #[test]
    fn long_runs_are_cut_hard_without_splitting_emoji() {
        // No sentence ends or spaces at all; the family emoji is a ZWJ sequence.
        let family = "👨\u{200D}👩\u{200D}👧";
        let text = format!("{}{}", "字".repeat(598), family.repeat(200));
        let chunks = chunk(&text, false);
        assert!(lengths(&chunks).iter().all(|n| *n <= TARGET_CHARS));
        for c in &chunks {
            assert!(!c.content.starts_with('\u{200D}'));
            assert!(!c.content.ends_with('\u{200D}'));
        }
        assert!(chunks.iter().all(|c| !c.content.is_empty()));
        // Hard cuts overlap by exactly OVERLAP_CHARS.
        let chunks = chunk(&"😀".repeat(1500), false);
        assert_eq!(lengths(&chunks), [600, 600, 500]);
    }

    #[test]
    fn ascii_periods_inside_words_do_not_end_sentences() {
        let text = format!(
            "{} See example.com and 3.14 here. {}",
            "x".repeat(300),
            "y ".repeat(300)
        );
        for c in chunk(&text, false) {
            assert!(!c.content.starts_with("com"));
            assert!(!c.content.starts_with("14"));
        }
    }

    #[test]
    fn markdown_chunks_carry_the_heading_path() {
        let text = "前書き\n\n# 第1章\n\n本文1\n\n## 節A ##\n\n本文A\n\n```\n# コード内の見出しではない行\n```\n\n## 節B\n\n### 小節\n\n本文B\n\n# 第2章\n\n# 空の章\n\n# C#\n本文C\n";
        let chunks = chunk(text, true);
        let pairs: Vec<(Option<&str>, &str)> = chunks
            .iter()
            .map(|c| (c.heading.as_deref(), c.content.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                (None, "前書き"),
                (Some("第1章"), "本文1"),
                (
                    Some("第1章 > 節A"),
                    "本文A\n\n```\n# コード内の見出しではない行\n```"
                ),
                (Some("第1章 > 節B > 小節"), "本文B"),
                (Some("C#"), "本文C"),
            ]
        );
        // Plain text keeps lines starting with # as text.
        assert_eq!(chunk("# 見出し\n本文", false)[0].heading, None);
    }

    #[test]
    fn chunking_is_deterministic_for_counting() {
        let text = "段落。".repeat(500) + "\n\n" + &"次の段落。".repeat(300);
        assert_eq!(chunk(&text, true), chunk(&text, true));
        assert_eq!(chunk(&text, false).len(), chunk(&text, true).len());
    }

    #[test]
    fn joining_without_overlap_keeps_both() {
        assert_eq!(
            join_overlapping("前の部分", "後の部分"),
            "前の部分\n後の部分"
        );
        assert_eq!(
            join_overlapping("abcdefghij klm", "efghij klm nop"),
            "abcdefghij klm nop"
        );
    }
}
