/// Discord counts UTF-16 code units. Reserve space for closing/reopening fences.
const BODY_LIMIT: usize = 1800;

pub fn split_message(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut fence: Option<(String, String)> = None;
    for line in text.split_inclusive('\n') {
        // A fence marker is recognized only at the beginning of a Markdown line.
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        let mut run = marker.map(|m| trimmed.chars().take_while(|c| *c == m).collect::<String>());
        let closing = match (&fence, &run) {
            (Some((delimiter, _)), Some(run)) => {
                run.starts_with(delimiter) && trimmed[run.len()..].trim().is_empty()
            }
            _ => false,
        };
        // Keep ordinary fence lines atomic, and cap pathological language labels.
        let line = if let Some(delimiter) = &run
            && line.encode_utf16().count() > 100
        {
            let shortened: String = delimiter.chars().take(3).collect();
            run = Some(shortened.clone());
            format!("{shortened}\n")
        } else {
            line.to_owned()
        };
        if current.encode_utf16().count() + line.encode_utf16().count() > BODY_LIMIT
            && !current.is_empty()
        {
            flush(&mut result, &mut current, &fence);
        }
        for ch in line.chars() {
            if current.encode_utf16().count() + ch.len_utf16() > BODY_LIMIT {
                flush(&mut result, &mut current, &fence);
            }
            current.push(ch);
        }
        if closing {
            fence = None;
        } else if fence.is_none()
            && let Some(run) = run
        {
            let delimiter: String = run.chars().take(100).collect();
            fence = Some((delimiter, line.trim_end().to_owned()));
        }
    }
    if !current.is_empty() {
        if let Some((delimiter, _)) = &fence {
            current.push_str(&format!("\n{delimiter}"));
        }
        result.push(current);
    }
    result
}

fn flush(result: &mut Vec<String>, current: &mut String, fence: &Option<(String, String)>) {
    if let Some((delimiter, opening)) = fence {
        current.push_str(&format!("\n{delimiter}"));
        result.push(std::mem::take(current));
        current.push_str(opening);
        current.push('\n');
    } else {
        result.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_and_code_fences() {
        for text in [
            "あ😀".repeat(2500),
            format!("説明\n```rust\n{}\n```\n終わり", "let x = 1;\n".repeat(500)),
            format!("~~~text\n{}", "長文😀".repeat(2000)),
        ] {
            let parts = split_message(&text);
            assert!(parts.len() > 1);
            for part in &parts {
                assert!(part.encode_utf16().count() <= 2000);
                for marker in ["```", "~~~"] {
                    assert_eq!(
                        part.lines().filter(|l| l.starts_with(marker)).count() % 2,
                        0
                    );
                }
            }
        }
        assert_eq!(split_message("短い回答"), vec!["短い回答"]);
    }
}
