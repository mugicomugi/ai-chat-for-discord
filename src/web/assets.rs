//! The browser app (`static/`, compiled into the binary) and the policy pages (`docs/*.md`,
//! rendered once at startup).

use std::sync::LazyLock;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use ring::digest;

use super::AppState;

struct Asset {
    path: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

const ASSETS: &[Asset] = &[
    Asset {
        path: "index.html",
        content_type: "text/html; charset=utf-8",
        body: include_bytes!("../../static/index.html"),
    },
    Asset {
        path: "app.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/app.js"),
    },
    Asset {
        path: "app.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../../static/app.css"),
    },
    Asset {
        path: "favicon.svg",
        content_type: "image/svg+xml",
        body: include_bytes!("../../static/favicon.svg"),
    },
    Asset {
        path: "vendor/marked.min.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/vendor/marked.min.js"),
    },
    Asset {
        path: "vendor/purify.min.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../../static/vendor/purify.min.js"),
    },
    Asset {
        path: "vendor/LICENSES.txt",
        content_type: "text/plain; charset=utf-8",
        body: include_bytes!("../../static/vendor/LICENSES.txt"),
    },
];

/// Content hashes, so browsers revalidate cheaply after a deploy (`Cache-Control: no-cache`).
static ETAGS: LazyLock<Vec<String>> = LazyLock::new(|| {
    ASSETS
        .iter()
        .map(|asset| {
            let digest = digest::digest(&digest::SHA256, asset.body);
            let hex: String = digest.as_ref()[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("\"{hex}\"")
        })
        .collect()
});

pub async fn index(headers: HeaderMap) -> Response {
    asset("index.html", &headers)
}

pub async fn file(Path(path): Path<String>, headers: HeaderMap) -> Response {
    asset(&path, &headers)
}

fn asset(path: &str, headers: &HeaderMap) -> Response {
    let Some(index) = ASSETS.iter().position(|asset| asset.path == path) else {
        return (StatusCode::NOT_FOUND, "見つかりません。").into_response();
    };
    let (asset, etag) = (&ASSETS[index], ETAGS[index].as_str());
    let cache = [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache")];
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|tag| tag.trim() == etag));
    if matches {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (
        cache,
        [(header::CONTENT_TYPE, asset.content_type)],
        Bytes::from_static(asset.body),
    )
        .into_response()
}

/// The privacy policy and terms of service as HTML.
pub struct Pages {
    privacy: Bytes,
    terms: Bytes,
}

impl Pages {
    pub fn render() -> Self {
        Self {
            privacy: render_markdown(
                "プライバシーポリシー",
                include_str!("../../docs/privacy.md"),
            ),
            terms: render_markdown("利用規約", include_str!("../../docs/terms.md")),
        }
    }
}

pub async fn privacy(State(state): State<AppState>) -> Response {
    page(state.pages.privacy.clone())
}

pub async fn terms(State(state): State<AppState>) -> Response {
    page(state.pages.terms.clone())
}

fn page(body: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// Raw HTML in the Markdown is shown as text, links between the documents point at their routes
/// and only web, mail and same-site links are kept.
pub fn render_markdown(title: &str, markdown: &str) -> Bytes {
    let events = Parser::new_ext(markdown, Options::ENABLE_TABLES).map(|event| match event {
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: link(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: link(dest_url),
            title,
            id,
        }),
        event => event,
    });
    let mut body = String::new();
    html::push_html(&mut body, events);
    Bytes::from(layout(title, &body))
}

fn link(url: CowStr<'_>) -> CowStr<'_> {
    match url.as_ref() {
        "privacy.md" => "/privacy".into(),
        "terms.md" => "/terms".into(),
        value
            if ["https://", "http://", "mailto:", "/", "#"]
                .iter()
                .any(|prefix| value.starts_with(prefix)) =>
        {
            url
        }
        _ => "#".into(),
    }
}

fn layout(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<link rel=\"icon\" href=\"/static/favicon.svg\" type=\"image/svg+xml\">\n\
         <link rel=\"stylesheet\" href=\"/static/app.css\">\n</head>\n\
         <body>\n<main class=\"document\">\n{body}<p class=\"back\"><a href=\"/\">トップへ戻る</a></p>\n</main>\n\
         </body>\n</html>\n"
    )
}

/// A short page for browser navigations that failed (the login callback). `title` and
/// `message` are fixed texts, never user input.
pub fn message_page(title: &'static str, message: &'static str) -> Html<String> {
    Html(layout(
        title,
        &format!("<h1>{title}</h1>\n<p>{message}</p>\n"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_rendered_without_raw_html() {
        let html = render_markdown(
            "テスト",
            "# 見出し\n\n<script>alert(1)</script>\n\n本文 <b>太字</b> [規約](terms.md) \
             [外部](https://example.com/) [危険](javascript:alert(1))\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        );
        let html = std::str::from_utf8(&html).unwrap();
        assert!(html.contains("<h1>見出し</h1>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;b&gt;太字&lt;/b&gt;"));
        assert!(!html.contains("<script"));
        assert!(html.contains("<a href=\"/terms\">規約</a>"));
        assert!(html.contains("<a href=\"https://example.com/\">外部</a>"));
        assert!(html.contains("<a href=\"#\">危険</a>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<title>テスト</title>"));
    }

    #[test]
    fn bundled_documents_render() {
        let pages = Pages::render();
        let privacy = std::str::from_utf8(&pages.privacy).unwrap();
        assert!(privacy.contains("<h1>プライバシーポリシー</h1>"));
        let terms = std::str::from_utf8(&pages.terms).unwrap();
        assert!(terms.contains("href=\"/privacy\""));
    }

    #[test]
    fn every_asset_has_an_etag() {
        assert_eq!(ETAGS.len(), ASSETS.len());
        assert!(ETAGS.iter().all(|tag| tag.len() == 34));
    }
}
