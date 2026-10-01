//! Whether and how a question consults the guild's knowledge base. Shared by /talk and the web
//! chat, so both search with the same time limit and say the same when they could not.

use std::time::Duration;

use super::{Knowledge, SearchOutcome, store::KbSource};
use crate::agent::Excerpt;

/// The knowledge search gets this long; then the answer goes ahead without it.
const KNOWLEDGE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the knowledge base contributes to one answer.
#[derive(Debug, Default)]
pub struct Consulted {
    pub excerpts: Vec<Excerpt>,
    /// The documents given to the AI; `None` when the knowledge base was not searched.
    pub sources: Option<Vec<KbSource>>,
    /// Appended to the answer when the knowledge base could not be used.
    pub notice: Option<&'static str>,
}

pub const KNOWLEDGE_DISABLED: &str =
    "\n\n※このBotではナレッジベースが有効になっていないため、資料を参照せずに回答しました。";
pub const KNOWLEDGE_EMPTY: &str =
    "\n\n※参照できるナレッジ資料が見つからなかったため、資料を参照せずに回答しました。";
pub const KNOWLEDGE_FAILED: &str =
    "\n\n※ナレッジ資料を検索できなかったため、資料を参照せずに回答しました。";

/// `false` keeps the question away from the knowledge base and so from the embedding providers;
/// otherwise it is searched when the knowledge base is enabled.
pub fn searches_knowledge(requested: Option<bool>) -> bool {
    requested != Some(false)
}

/// Searches the guild's knowledge base for the question unless the caller turned it off
/// (`requested` is `Some(false)`). On failure the answer goes ahead without it, with a notice.
pub async fn consult(
    knowledge: Option<&Knowledge>,
    guild: u64,
    question: &str,
    requested: Option<bool>,
) -> Consulted {
    let search = match knowledge {
        Some(knowledge) if searches_knowledge(requested) => Some(
            match tokio::time::timeout(KNOWLEDGE_TIMEOUT, knowledge.search(guild, question)).await {
                Ok(Ok(outcome)) => Ok(outcome),
                result => {
                    let error_code = match result {
                        Ok(Err(error)) => error.to_string(),
                        _ => "timeout".into(),
                    };
                    tracing::warn!(guild_id = guild, error_code, "knowledge_search_failed");
                    Err(())
                }
            },
        ),
        _ => None,
    };
    outcome(requested, knowledge.is_some(), search)
}

/// What the knowledge base contributes, from the requested option, whether the knowledge base
/// is enabled and the search: `None` if there was none, `Err` if it failed or timed out.
/// `sources` is what gets recorded (`None`: the knowledge base was not used).
pub fn outcome(
    requested: Option<bool>,
    enabled: bool,
    search: Option<Result<SearchOutcome, ()>>,
) -> Consulted {
    let mut consulted = Consulted::default();
    let asked = requested == Some(true);
    match search {
        None => {
            if asked && !enabled {
                consulted.notice = Some(KNOWLEDGE_DISABLED);
            }
        }
        Some(Ok(SearchOutcome::NoDocuments)) => {
            if asked {
                consulted.notice = Some(KNOWLEDGE_EMPTY);
            }
        }
        Some(Ok(SearchOutcome::Found(excerpts))) => {
            if excerpts.is_empty() && asked {
                consulted.notice = Some(KNOWLEDGE_EMPTY);
            }
            consulted.sources = Some(super::sources(&excerpts));
            consulted.excerpts = excerpts;
        }
        Some(Err(())) => {
            consulted.sources = Some(Vec::new());
            consulted.notice = Some(KNOWLEDGE_FAILED);
        }
    }
    consulted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowledge_options_decide_search_notice_and_record() {
        // knowledge:false never sends the question to the embedding providers.
        assert!(!searches_knowledge(Some(false)));
        assert!(searches_knowledge(None) && searches_knowledge(Some(true)));
        let excerpt = Excerpt {
            document_id: 7,
            title: "手順書".into(),
            text: "抜粋".into(),
        };
        let found = || Some(Ok(SearchOutcome::Found(vec![excerpt.clone()])));
        let source = Some(vec![KbSource {
            id: 7,
            title: "手順書".into(),
        }]);
        // (option, enabled, search) → (notice, recorded sources, excerpts given to the AI)
        let cases = [
            (Some(false), true, None, None, None, 0),
            (None, false, None, None, None, 0),
            (Some(true), false, None, Some(KNOWLEDGE_DISABLED), None, 0),
            (
                None,
                true,
                Some(Ok(SearchOutcome::NoDocuments)),
                None,
                None,
                0,
            ),
            (
                Some(true),
                true,
                Some(Ok(SearchOutcome::NoDocuments)),
                Some(KNOWLEDGE_EMPTY),
                None,
                0,
            ),
            (
                Some(true),
                true,
                Some(Ok(SearchOutcome::Found(Vec::new()))),
                Some(KNOWLEDGE_EMPTY),
                Some(Vec::new()),
                0,
            ),
            (None, true, found(), None, source.clone(), 1),
            (Some(true), true, found(), None, source, 1),
            // A failure or timeout: the answer says so, and the run records an empty list.
            (
                None,
                true,
                Some(Err(())),
                Some(KNOWLEDGE_FAILED),
                Some(Vec::new()),
                0,
            ),
        ];
        for (requested, enabled, search, notice, sources, excerpts) in cases {
            let label = format!("{requested:?} {enabled} {search:?}");
            let consulted = outcome(requested, enabled, search);
            assert_eq!(consulted.notice, notice, "{label}");
            assert_eq!(consulted.sources, sources, "{label}");
            assert_eq!(consulted.excerpts.len(), excerpts, "{label}");
        }
    }
}
