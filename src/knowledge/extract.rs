//! Text from uploaded files. The kind comes from the file name's extension and the first bytes
//! (the browser's Content-Type is not trusted). PDFs are parsed by a child process of the same
//! binary (`extract-pdf`), so a parser crash, hang or memory blow-up cannot take the bot down.

use std::{path::PathBuf, process::Stdio, sync::LazyLock, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
};

/// Longest text a document may have after extraction.
pub const MAX_TEXT_CHARS: usize = 500_000;
pub const MAX_PDF_PAGES: usize = 300;
/// Largest PDF parsed, whatever KB_MAX_UPLOAD_BYTES allows: the parser's memory counts against
/// the bot's container.
pub const MAX_PDF_BYTES: usize = 5 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an upload waits for another PDF to finish before giving up.
const PDF_QUEUE_WAIT: Duration = Duration::from_secs(15);
/// The child's output limit: the longest text in UTF-8, plus room for whitespace the
/// normalization removes.
const MAX_PDF_OUTPUT_BYTES: usize = MAX_TEXT_CHARS * 8;
/// The child reads at most this much from stdin (the parent checks it before starting one).
const MAX_PDF_INPUT_BYTES: u64 = MAX_PDF_BYTES as u64;

/// Exit codes of `extract-pdf`; anything else (a panic exits with 101) means unreadable.
const EXIT_INVALID: i32 = 10;
const EXIT_ENCRYPTED: i32 = 11;
const EXIT_TOO_MANY_PAGES: i32 = 12;
const EXIT_TOO_LONG: i32 = 13;
const EXIT_UNAVAILABLE: i32 = 14;

/// One PDF at a time: the parser's memory counts against the bot's container limit.
static PDF_SLOT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Text,
    Markdown,
    Pdf,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text/plain",
            Self::Markdown => "text/markdown",
            Self::Pdf => "application/pdf",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Text, Self::Markdown, Self::Pdf]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }

    pub fn is_markdown(self) -> bool {
        self == Self::Markdown
    }
}

/// The file name extensions an upload may have (lowercase, without the dot) and their kinds.
/// A build without PDF support leaves PDF out.
pub fn extensions() -> impl Iterator<Item = (&'static str, MediaType)> {
    [
        ("txt", MediaType::Text),
        ("text", MediaType::Text),
        ("md", MediaType::Markdown),
        ("markdown", MediaType::Markdown),
        ("pdf", MediaType::Pdf),
    ]
    .into_iter()
    .filter(|(_, kind)| cfg!(feature = "pdf") || *kind != MediaType::Pdf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    #[error("unsupported_type")]
    Unsupported,
    /// The extension and the content disagree (a renamed file).
    #[error("type_mismatch")]
    Mismatch,
    #[error("not_utf8")]
    NotUtf8,
    #[error("empty")]
    Empty,
    #[error("garbled")]
    Garbled,
    #[error("too_long")]
    TooLong,
    #[error("pdf_invalid")]
    PdfInvalid,
    #[error("pdf_encrypted")]
    PdfEncrypted,
    #[error("pdf_too_many_pages")]
    PdfTooManyPages,
    #[error("pdf_too_large")]
    PdfTooLarge,
    /// The parser was killed (most likely it ran out of memory).
    #[error("pdf_too_complex")]
    PdfTooComplex,
    #[error("pdf_timeout")]
    PdfTimeout,
    /// Another PDF is being read.
    #[error("pdf_busy")]
    PdfBusy,
    /// This build has no PDF support, or the child process could not be started.
    #[error("pdf_unavailable")]
    PdfUnavailable,
}

impl ExtractError {
    /// The kind code (the same as `Display`), as a static string for API errors.
    pub fn code(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported_type",
            Self::Mismatch => "type_mismatch",
            Self::NotUtf8 => "not_utf8",
            Self::Empty => "empty",
            Self::Garbled => "garbled",
            Self::TooLong => "too_long",
            Self::PdfInvalid => "pdf_invalid",
            Self::PdfEncrypted => "pdf_encrypted",
            Self::PdfTooManyPages => "pdf_too_many_pages",
            Self::PdfTooLarge => "pdf_too_large",
            Self::PdfTooComplex => "pdf_too_complex",
            Self::PdfTimeout => "pdf_timeout",
            Self::PdfBusy => "pdf_busy",
            Self::PdfUnavailable => "pdf_unavailable",
        }
    }

    pub fn user_message(self) -> String {
        match self {
            Self::Unsupported if cfg!(feature = "pdf") => {
                "登録できるのはテキスト（.txt）、Markdown（.md）、PDF（.pdf）のファイルだけです。"
                    .into()
            }
            Self::Unsupported => {
                "登録できるのはテキスト（.txt）と Markdown（.md）のファイルだけです。".into()
            }
            Self::Mismatch => {
                "ファイルの内容が拡張子と一致しません。正しい形式のファイルを選んでください。"
                    .into()
            }
            Self::NotUtf8 => {
                "テキストファイルは UTF-8 で保存してください（Shift_JIS などには対応していません）。"
                    .into()
            }
            Self::Empty => "ファイルから本文を取り出せませんでした。".into(),
            Self::Garbled => "取り出した本文が文字化けしているため登録できません。PDF の場合は、テキストを選択・コピーできるものか確認してください（画像だけの PDF には対応していません）。".into(),
            Self::TooLong => format!(
                "本文が長すぎます（{MAX_TEXT_CHARS}文字まで）。ファイルを分割してください。"
            ),
            Self::PdfInvalid => "PDF を読み取れませんでした。壊れているか、対応していない形式です。".into(),
            Self::PdfEncrypted => "パスワードで保護された PDF には対応していません。".into(),
            Self::PdfTooManyPages => {
                format!("PDF は{MAX_PDF_PAGES}ページまでです。ファイルを分割してください。")
            }
            Self::PdfTooLarge => format!(
                "PDF は{} MiBまでです。ファイルを分割してください。",
                MAX_PDF_BYTES / (1024 * 1024)
            ),
            Self::PdfTooComplex => "PDF の読み取り中にメモリが足りなくなりました。ページ数を減らすか、ファイルを分割してください。".into(),
            Self::PdfTimeout => {
                "PDF の読み取りに時間がかかりすぎたため中止しました。ファイルを分割してください。"
                    .into()
            }
            Self::PdfBusy => {
                "ほかの PDF を処理中です。しばらくしてから再試行してください。".into()
            }
            Self::PdfUnavailable => "現在 PDF は登録できません。管理者に確認を依頼してください。".into(),
        }
    }
}

/// The kind of an upload from its name and first bytes.
pub fn detect(file_name: &str, bytes: &[u8]) -> Result<MediaType, ExtractError> {
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let kind = extensions()
        .find(|(known, _)| *known == extension)
        .map(|(_, kind)| kind)
        .ok_or(ExtractError::Unsupported)?;
    let pdf = if kind == MediaType::Pdf {
        // Readers accept the header anywhere in the first kilobyte.
        let head = &bytes[..bytes.len().min(1024)];
        head.windows(5).any(|window| window == b"%PDF-")
    } else {
        // A text may mention the header; a renamed PDF starts with it.
        let text = bytes.strip_prefix("\u{FEFF}".as_bytes()).unwrap_or(bytes);
        text.trim_ascii_start().starts_with(b"%PDF-")
    };
    if (kind == MediaType::Pdf) != pdf {
        return Err(ExtractError::Mismatch);
    }
    Ok(kind)
}

/// The text of an upload, normalized and checked. `pdf_program` is the binary that runs
/// `extract-pdf` (the running executable unless a test passes the built bot binary).
pub async fn extract(
    kind: MediaType,
    bytes: &[u8],
    pdf_program: Option<PathBuf>,
) -> Result<String, ExtractError> {
    let raw = match kind {
        MediaType::Text | MediaType::Markdown => {
            // UTF-16 and other encodings are full of NUL bytes or invalid sequences.
            if bytes.contains(&0) {
                return Err(ExtractError::NotUtf8);
            }
            std::str::from_utf8(bytes)
                .map_err(|_| ExtractError::NotUtf8)?
                .to_owned()
        }
        MediaType::Pdf => extract_pdf(bytes, pdf_program).await?,
    };
    finish(&raw)
}

/// Checks extracted text and normalizes it.
pub fn finish(raw: &str) -> Result<String, ExtractError> {
    if raw.chars().all(char::is_whitespace) {
        return Err(ExtractError::Empty);
    }
    if garbled(raw) {
        return Err(ExtractError::Garbled);
    }
    let text = normalize(raw);
    if text.is_empty() {
        return Err(ExtractError::Empty);
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(ExtractError::TooLong);
    }
    Ok(text)
}

/// No BOM, `\n` line ends, no control characters except tab and newline, no trailing spaces,
/// at most one blank line in a row.
pub fn normalize(text: &str) -> String {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut result = String::with_capacity(text.len());
    let mut blank_lines = 0;
    for line in text.split('\n') {
        let line: String = line
            .chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect();
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_lines += 1;
            continue;
        }
        if !result.is_empty() {
            result.push_str(if blank_lines > 0 { "\n\n" } else { "\n" });
        }
        blank_lines = 0;
        result.push_str(line);
    }
    result
}

/// Empty, or more than 10% of the visible characters are replacement characters, private-use
/// characters or control characters: what PDFs without a text layer or usable font mapping and
/// mis-decoded files tend to produce.
pub fn garbled(text: &str) -> bool {
    let (mut visible, mut bad) = (0_usize, 0_usize);
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        visible += 1;
        if c == '\u{FFFD}'
            || matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
            || c.is_control()
        {
            bad += 1;
        }
    }
    visible == 0 || bad * 10 > visible
}

async fn extract_pdf(bytes: &[u8], program: Option<PathBuf>) -> Result<String, ExtractError> {
    if !cfg!(feature = "pdf") {
        return Err(ExtractError::PdfUnavailable);
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err(ExtractError::PdfTooLarge);
    }
    let _slot = tokio::time::timeout(PDF_QUEUE_WAIT, PDF_SLOT.acquire())
        .await
        .map_err(|_| ExtractError::PdfBusy)?
        .expect("semaphore never closed");
    let program = match program {
        Some(program) => program,
        None => std::env::current_exe().map_err(|_| ExtractError::PdfUnavailable)?,
    };
    let mut child = Command::new(program)
        .arg("extract-pdf")
        // The parser needs none of the bot's settings, secrets included.
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            tracing::warn!("pdf_extractor_spawn_failed");
            ExtractError::PdfUnavailable
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let run = async {
        let write = async {
            // The child may exit early (too many pages); a broken pipe is not an error here.
            let _ = stdin.write_all(bytes).await;
            drop(stdin);
        };
        let read = async {
            let mut output = Vec::new();
            let mut limited = stdout.take(MAX_PDF_OUTPUT_BYTES as u64 + 1);
            limited.read_to_end(&mut output).await.map(|_| output)
        };
        let ((), output) = tokio::join!(write, read);
        let status = child.wait().await;
        (status, output)
    };
    // On timeout the child is dropped, which kills it.
    let (status, output) = tokio::time::timeout(PDF_TIMEOUT, run)
        .await
        .map_err(|_| ExtractError::PdfTimeout)?;
    let status = status.map_err(|_| ExtractError::PdfInvalid)?;
    let output = output.map_err(|_| ExtractError::PdfInvalid)?;
    match status.code() {
        Some(0) if output.len() > MAX_PDF_OUTPUT_BYTES => Err(ExtractError::TooLong),
        Some(0) => String::from_utf8(output).map_err(|_| ExtractError::PdfInvalid),
        Some(EXIT_ENCRYPTED) => Err(ExtractError::PdfEncrypted),
        Some(EXIT_TOO_MANY_PAGES) => Err(ExtractError::PdfTooManyPages),
        Some(EXIT_TOO_LONG) => Err(ExtractError::TooLong),
        Some(EXIT_UNAVAILABLE) => Err(ExtractError::PdfUnavailable),
        Some(EXIT_INVALID) => Err(ExtractError::PdfInvalid),
        code => {
            // A panic in the parser (101), or a kill: the child asks the kernel to pick it first
            // when the container runs out of memory.
            #[cfg(unix)]
            let signal = std::os::unix::process::ExitStatusExt::signal(&status);
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            tracing::warn!(exit_code = ?code, signal = ?signal, "pdf_extractor_failed");
            Err(if signal.is_some() {
                ExtractError::PdfTooComplex
            } else {
                ExtractError::PdfInvalid
            })
        }
    }
}

/// `bot extract-pdf`: PDF bytes on stdin, UTF-8 text on stdout, the result as the exit code.
/// Runs before logging is set up, so nothing but the text reaches stdout.
pub fn pdf_child() -> i32 {
    // Make the kernel pick this process first if the container runs out of memory. Raising
    // one's own score needs no privileges.
    let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
    let mut input = Vec::new();
    use std::io::{Read, Write};
    if std::io::stdin()
        .take(MAX_PDF_INPUT_BYTES)
        .read_to_end(&mut input)
        .is_err()
    {
        return EXIT_INVALID;
    }
    match pdf_text(&input) {
        Ok(text) => {
            let mut stdout = std::io::stdout().lock();
            if stdout
                .write_all(text.as_bytes())
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return EXIT_INVALID;
            }
            0
        }
        Err(code) => code,
    }
}

#[cfg(feature = "pdf")]
fn pdf_text(input: &[u8]) -> Result<String, i32> {
    use pdf_extract::{ConvertToFmt, Document, PlainTextOutput, output_doc};

    /// Stops the parser once the text is longer than any document may be.
    struct Capped<'a>(&'a mut String);

    impl std::fmt::Write for Capped<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            if self.0.len() + s.len() > MAX_PDF_OUTPUT_BYTES {
                return Err(std::fmt::Error);
            }
            self.0.push_str(s);
            Ok(())
        }
    }

    impl ConvertToFmt for Capped<'_> {
        type Writer = Self;
        fn convert(self) -> Self {
            self
        }
    }

    let mut document = Document::load_mem(input).map_err(|_| EXIT_INVALID)?;
    if document.is_encrypted() && document.decrypt("").is_err() {
        return Err(EXIT_ENCRYPTED);
    }
    if document.get_pages().len() > MAX_PDF_PAGES {
        return Err(EXIT_TOO_MANY_PAGES);
    }
    let mut text = String::new();
    let result = output_doc(&document, &mut PlainTextOutput::new(Capped(&mut text)));
    match result {
        Ok(()) => Ok(text),
        Err(pdf_extract::OutputError::FormatError(_)) => Err(EXIT_TOO_LONG),
        Err(_) => Err(EXIT_INVALID),
    }
}

#[cfg(not(feature = "pdf"))]
fn pdf_text(_input: &[u8]) -> Result<String, i32> {
    Err(EXIT_UNAVAILABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_come_from_extension_and_content() {
        assert_eq!(detect("メモ.TXT", b"hello"), Ok(MediaType::Text));
        assert_eq!(detect("a.text", b"hello"), Ok(MediaType::Text));
        assert_eq!(detect("a.md", b"# x"), Ok(MediaType::Markdown));
        assert_eq!(detect("a.markdown", b"x"), Ok(MediaType::Markdown));
        assert_eq!(detect("a.txt", b"%PDF-1.4"), Err(ExtractError::Mismatch));
        assert_eq!(
            detect("a.md", "\u{FEFF}\n  %PDF-1.4".as_bytes()),
            Err(ExtractError::Mismatch)
        );
        // A note about PDFs is still a note.
        assert_eq!(
            detect("a.md", b"# PDF\nPDF files start with `%PDF-1.7`."),
            Ok(MediaType::Markdown)
        );
        for name in ["a.docx", "a", "a.exe", "pdf", "a.pdf.zip"] {
            assert_eq!(detect(name, b"x"), Err(ExtractError::Unsupported), "{name}");
        }
        if cfg!(feature = "pdf") {
            assert_eq!(detect("a.pdf", b"%PDF-1.7\n..."), Ok(MediaType::Pdf));
            assert_eq!(detect("a.pdf", b"\r\n%PDF-1.4"), Ok(MediaType::Pdf));
            assert_eq!(detect("a.pdf", b"plain text"), Err(ExtractError::Mismatch));
        } else {
            assert_eq!(detect("a.pdf", b"%PDF-1.7"), Err(ExtractError::Unsupported));
        }
        let listed: Vec<&str> = extensions().map(|(extension, _)| extension).collect();
        assert_eq!(listed.contains(&"pdf"), cfg!(feature = "pdf"));
    }

    #[cfg(feature = "pdf")]
    #[tokio::test]
    async fn large_pdfs_are_refused_before_parsing() {
        let mut bytes = b"%PDF-1.4\n".to_vec();
        bytes.resize(MAX_PDF_BYTES + 1, b' ');
        // The program does not exist: the size is checked before a child is started.
        let program = PathBuf::from("/nonexistent/bot");
        assert_eq!(
            extract(MediaType::Pdf, &bytes, Some(program)).await,
            Err(ExtractError::PdfTooLarge)
        );
    }

    /// A child killed by a signal (as the kernel's OOM killer would) is not a broken PDF.
    #[cfg(all(unix, feature = "pdf"))]
    #[tokio::test]
    async fn a_killed_parser_is_reported_as_too_complex() {
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join(format!("kill-pdf-{}.sh", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\nkill -KILL $$\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = extract(MediaType::Pdf, b"%PDF-1.4\n", Some(script.clone())).await;
        std::fs::remove_file(&script).unwrap();
        assert_eq!(result, Err(ExtractError::PdfTooComplex));
    }

    #[tokio::test]
    async fn text_must_be_utf8_and_is_normalized() {
        let text = extract(
            MediaType::Text,
            "\u{FEFF}一行目\r\n二行目  \r\n\r\n\r\n\r\n三行目\u{7}\tタブ\r最後\u{85}".as_bytes(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(text, "一行目\n二行目\n\n三行目\tタブ\n最後");
        // Shift_JIS for "日本語".
        assert_eq!(
            extract(MediaType::Text, &[0x93, 0xfa, 0x96, 0x7b, 0x8c, 0xea], None).await,
            Err(ExtractError::NotUtf8)
        );
        // UTF-16LE with BOM.
        assert_eq!(
            extract(MediaType::Markdown, &[0xff, 0xfe, 0x41, 0x00], None).await,
            Err(ExtractError::NotUtf8)
        );
        assert_eq!(
            extract(MediaType::Text, b" \r\n\t ", None).await,
            Err(ExtractError::Empty)
        );
        let long = "あ".repeat(MAX_TEXT_CHARS + 1);
        assert_eq!(
            extract(MediaType::Text, long.as_bytes(), None).await,
            Err(ExtractError::TooLong)
        );
        let exact = "😀".repeat(MAX_TEXT_CHARS);
        assert!(
            extract(MediaType::Text, exact.as_bytes(), None)
                .await
                .is_ok()
        );
    }

    #[test]
    fn garbled_text_is_detected() {
        assert!(garbled(""));
        assert!(garbled(" \n\t"));
        assert!(!garbled("普通の日本語の文章です。English too. 😀"));
        // 1 bad character in 10 visible ones is still acceptable; 2 in 10 is not.
        assert!(!garbled("abcdefghi\u{FFFD}"));
        assert!(garbled("abcdefgh\u{FFFD}\u{FFFD}"));
        assert!(garbled(&format!(
            "{}{}",
            "\u{E000}".repeat(3),
            "本文".repeat(5)
        )));
        assert!(garbled(&format!(
            "{}{}",
            "\u{1}".repeat(3),
            "本文".repeat(5)
        )));
        assert_eq!(finish("\u{FFFD}\u{FFFD}あ"), Err(ExtractError::Garbled));
        assert_eq!(finish("本文\r\n"), Ok("本文".into()));
    }

    #[test]
    fn codes_match_display() {
        for error in [
            ExtractError::Unsupported,
            ExtractError::Mismatch,
            ExtractError::NotUtf8,
            ExtractError::Empty,
            ExtractError::Garbled,
            ExtractError::TooLong,
            ExtractError::PdfInvalid,
            ExtractError::PdfEncrypted,
            ExtractError::PdfTooManyPages,
            ExtractError::PdfTooLarge,
            ExtractError::PdfTooComplex,
            ExtractError::PdfTimeout,
            ExtractError::PdfBusy,
            ExtractError::PdfUnavailable,
        ] {
            assert_eq!(error.code(), error.to_string());
            assert!(!error.user_message().is_empty());
        }
    }

    #[test]
    fn media_types_round_trip() {
        for kind in [MediaType::Text, MediaType::Markdown, MediaType::Pdf] {
            assert_eq!(MediaType::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MediaType::parse("image/png"), None);
    }
}
