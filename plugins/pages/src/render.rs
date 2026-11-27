//! Turning a page's folder into HTML, and finding what's wrong with it.
//!
//! Markdown is GitHub-flavoured (`pulldown-cmark`), with raw HTML turned into text and links limited to safe schemes.
//! A paragraph made only of images becomes a figure (one image) or a gallery (several), the alt text as each caption.
//! An `index.html` is never rendered here: it's checked, and shown in a sandboxed frame.

use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::Serialize;

/// Images bigger than this are flagged: a page should open quickly on a phone.
pub const MAX_IMAGE: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub message: String,
}

impl Problem {
    pub(crate) fn new(m: impl Into<String>) -> Self {
        Problem { message: m.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Markdown,
    Html,
    Empty,
}

/// A page as the list and the page view need it.
#[derive(Debug, Clone, Serialize)]
pub struct Rendered {
    pub kind: Kind,
    pub title: Option<String>,
    pub summary: Option<String>,
    /// The body's HTML, without the title and summary (Markdown pages only).
    pub body: String,
    pub problems: Vec<Problem>,
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
}

/// What a link or image destination is.
#[derive(Debug, PartialEq, Eq)]
enum Dest {
    /// http, https, mailto, or a fragment: fine as it is.
    External,
    /// A file relative to the page's folder (the path inside the space, normalised, without query or fragment).
    Local(PathBuf),
    /// A relative path that leaves the space.
    Outside,
    /// A scheme we don't allow (`javascript:`, `data:`, `file:`), or an absolute path.
    Refused,
}

/// Where `dest` points, from a page whose folder is `page` (relative to the space).
fn classify(dest: &str, page: &Path) -> Dest {
    let d = dest.trim();
    if d.is_empty() || d.starts_with('#') {
        return Dest::External;
    }
    let lower = d.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
        return Dest::External;
    }
    // Any other scheme (`javascript:`, `data:`, `file:`), a scheme-relative URL or an absolute path.
    if d.starts_with('/') || d.starts_with('\\') {
        return Dest::Refused;
    }
    if let Some(colon) = d.find(':') {
        let before = &d[..colon];
        if !before.contains('/') && !before.contains('?') && !before.contains('#') {
            return Dest::Refused;
        }
    }
    let path = d.split(['#', '?']).next().unwrap_or_default();
    let path = percent_decode(path);
    let mut out = PathBuf::new();
    for c in page.join(&path).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Dest::Outside;
                }
            }
            _ => return Dest::Refused,
        }
    }
    Dest::Local(out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path is served only if no part of it is hidden (`.git`, `.fleetlayer-space`) or private (`_drafts`).
pub fn servable(rel: &Path) -> bool {
    rel.components().all(|c| match c {
        Component::Normal(p) => {
            let s = p.to_string_lossy();
            !s.starts_with('.') && !s.starts_with('_')
        }
        _ => false,
    })
}

/// Checks a local destination against the space's folder: a problem when it isn't there.
fn check_local(space: &Path, rel: &Path, what: &str, dest: &str, image: bool, problems: &mut Vec<Problem>) {
    if !servable(rel) {
        problems.push(Problem::new(format!("The {what} {dest} points at a hidden file, which isn't served.")));
        return;
    }
    match std::fs::metadata(space.join(rel)) {
        Ok(m) => {
            if image && m.len() > MAX_IMAGE {
                problems.push(Problem::new(format!(
                    "The image {dest} is {:.1} MB: keep images under 5 MB (JPEG at quality 82 for screenshots).",
                    m.len() as f64 / 1_048_576.0
                )));
            }
        }
        Err(_) => problems.push(Problem::new(format!("The {what} {dest} isn't in the page's folder."))),
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// One image in a paragraph made only of images.
struct Img {
    src: String,
    alt: String,
    title: String,
}

/// Collects the paragraph's images when it holds nothing else (blank text and line breaks aside).
fn only_images(events: &[Event]) -> Option<Vec<Img>> {
    let mut imgs = vec![];
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Image { dest_url, title, .. }) => {
                let mut alt = String::new();
                i += 1;
                while i < events.len() {
                    match &events[i] {
                        Event::End(TagEnd::Image) => break,
                        Event::Text(t) | Event::Code(t) => alt.push_str(t),
                        _ => {}
                    }
                    i += 1;
                }
                imgs.push(Img { src: dest_url.to_string(), alt, title: title.to_string() });
            }
            Event::SoftBreak | Event::HardBreak => {}
            Event::Text(t) if t.trim().is_empty() => {}
            _ => return None,
        }
        i += 1;
    }
    (!imgs.is_empty()).then_some(imgs)
}

fn figure(img: &Img, class: &str) -> String {
    let title = if img.title.is_empty() { String::new() } else { format!(r#" title="{}""#, esc(&img.title)) };
    let caption =
        if img.alt.is_empty() { String::new() } else { format!("<figcaption>{}</figcaption>", esc(&img.alt)) };
    format!(
        r#"<figure class="{class}"><a class="lb" href="{src}"><img src="{src}" alt="{alt}" loading="lazy" decoding="async"{title}></a>{caption}</figure>"#,
        src = esc(&img.src),
        alt = esc(&img.alt),
    )
}

/// Renders a Markdown page. `space` is the space's folder on disk, `page` the page's folder relative to it.
pub fn markdown(src: &str, space: &Path, page: &Path) -> Rendered {
    let mut problems = vec![];
    let mut raw_html = false;

    // First pass: neutralise raw HTML and unsafe links, check local files.
    let mut events: Vec<Event> = vec![];
    for ev in Parser::new_ext(src, options()) {
        let ev = match ev {
            Event::Html(h) | Event::InlineHtml(h) => {
                raw_html = true;
                Event::Text(h)
            }
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                match classify(&dest_url, page) {
                    Dest::External => {}
                    Dest::Local(rel) => {
                        // A link to a folder (another page) needs no file check beyond existing.
                        check_local(space, &rel, "link", &dest_url, false, &mut problems);
                    }
                    Dest::Outside => {
                        problems.push(Problem::new(format!("The link {dest_url} points outside your pages folder.")))
                    }
                    Dest::Refused => {
                        problems.push(Problem::new(format!(
                            "The link {dest_url} isn't allowed (http, https, mailto or a relative path only)."
                        )));
                        events.push(Event::Start(Tag::Link { link_type, dest_url: CowStr::Borrowed("#"), title, id }));
                        continue;
                    }
                }
                Event::Start(Tag::Link { link_type, dest_url, title, id })
            }
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                match classify(&dest_url, page) {
                    Dest::Local(rel) => check_local(space, &rel, "image", &dest_url, true, &mut problems),
                    Dest::External => problems.push(Problem::new(format!(
                        "The image {dest_url} is on another site, so it isn't loaded: put it in the page's folder."
                    ))),
                    Dest::Outside => {
                        problems.push(Problem::new(format!("The image {dest_url} is outside your pages folder.")))
                    }
                    Dest::Refused => problems.push(Problem::new(format!("The image {dest_url} isn't allowed."))),
                }
                Event::Start(Tag::Image { link_type, dest_url, title, id })
            }
            e => e,
        };
        events.push(ev);
    }
    if raw_html {
        problems.push(Problem::new("The page has raw HTML, which is shown as text: use Markdown only."));
    }

    // The title: a level-one heading as the first block.
    let mut title = None;
    let mut start = 0;
    if let Some(Event::Start(Tag::Heading { level: HeadingLevel::H1, .. })) = events.first() {
        let mut t = String::new();
        let mut i = 1;
        while i < events.len() {
            match &events[i] {
                Event::End(TagEnd::Heading(_)) => break,
                Event::Text(s) | Event::Code(s) => t.push_str(s),
                _ => {}
            }
            i += 1;
        }
        title = Some(t.trim().to_string()).filter(|t| !t.is_empty());
        start = i + 1;
    }
    if title.is_none() {
        problems.insert(0, Problem::new("The first line should be the page's title: `# Title`."));
    }

    // The summary: the first block after the title, when it's a paragraph of text (not only images).
    let mut summary = None;
    if title.is_some() {
        if let Some(Event::Start(Tag::Paragraph)) = events.get(start) {
            let end =
                events[start..].iter().position(|e| matches!(e, Event::End(TagEnd::Paragraph))).map(|p| start + p);
            if let Some(end) = end
                && only_images(&events[start + 1..end]).is_none()
            {
                let mut s = String::new();
                for e in &events[start + 1..end] {
                    match e {
                        Event::Text(t) | Event::Code(t) => s.push_str(t),
                        Event::SoftBreak | Event::HardBreak => s.push(' '),
                        _ => {}
                    }
                }
                summary = Some(s.trim().to_string());
                start = end + 1;
            }
        }
        if summary.is_none() {
            problems.insert(
                1.min(problems.len()),
                Problem::new(
                    "There's no summary: the paragraph after the title says what the page shows, from when and where.",
                ),
            );
        }
    }

    // Second pass: emit HTML, with image-only paragraphs as figures and galleries.
    let mut body = String::new();
    let rest = &events[start..];
    let mut i = 0;
    let mut pending: Vec<Event> = vec![];
    while i < rest.len() {
        if let Event::Start(Tag::Paragraph) = &rest[i]
            && let Some(len) = rest[i..].iter().position(|e| matches!(e, Event::End(TagEnd::Paragraph)))
            && let Some(imgs) = only_images(&rest[i + 1..i + len])
        {
            pulldown_cmark::html::push_html(&mut body, pending.drain(..));
            if imgs.len() == 1 {
                body.push_str(&figure(&imgs[0], "single"));
            } else {
                body.push_str(r#"<div class="gallery">"#);
                for img in &imgs {
                    body.push_str(&figure(img, "shot"));
                }
                body.push_str("</div>");
            }
            body.push('\n');
            i += len + 1;
            continue;
        }
        // An image inside a sentence: a lightbox link around it too.
        match &rest[i] {
            Event::Start(Tag::Image { dest_url, .. }) => {
                pending.push(Event::InlineHtml(CowStr::from(format!(r#"<a class="lb" href="{}">"#, esc(dest_url)))));
                pending.push(rest[i].clone());
            }
            Event::End(TagEnd::Image) => {
                pending.push(rest[i].clone());
                pending.push(Event::InlineHtml(CowStr::Borrowed("</a>")));
            }
            e => pending.push(e.clone()),
        }
        i += 1;
    }
    pulldown_cmark::html::push_html(&mut body, pending.into_iter());

    Rendered { kind: Kind::Markdown, title, summary, body, problems }
}

/// Finds the text of the first `<tag ...>…</tag>`, case-insensitively.
fn element_text(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find(&format!("<{tag}"))?;
    let gt = open + lower[open..].find('>')? + 1;
    let close = gt + lower[gt..].find(&format!("</{tag}"))?;
    Some(decode_entities(html[gt..close].trim())).filter(|s| !s.is_empty())
}

fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// The `content` of `<meta name="description">`.
fn meta_description(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("<meta") {
        let at = from + p;
        let end = at + lower[at..].find('>')?;
        let tag = &lower[at..end];
        if tag.contains("name=\"description\"") || tag.contains("name='description'") {
            let raw = &html[at..end];
            let lraw = &lower[at..end];
            let c = lraw.find("content=")? + "content=".len();
            let q = raw[c..].chars().next()?;
            if q == '"' || q == '\'' {
                let rest = &raw[c + 1..];
                let close = rest.find(q)?;
                return Some(decode_entities(rest[..close].trim())).filter(|s| !s.is_empty());
            }
            return None;
        }
        from = end;
    }
    None
}

/// The values of `src=` and `href=` attributes (and `url(...)` in styles), for checking local files.
pub(crate) fn references(html: &str) -> Vec<String> {
    let mut out = vec![];
    let lower = html.to_ascii_lowercase();
    for attr in ["src=", "href="] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(attr) {
            let at = from + p;
            // Only a whole attribute name: `data-src=` and `xhref=` don't count.
            let before = lower[..at].chars().last();
            from = at + attr.len();
            if !matches!(before, Some(c) if c.is_whitespace()) {
                continue;
            }
            let rest = &html[from..];
            let value = match rest.chars().next() {
                Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or_default(),
                _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or_default(),
            };
            out.push(decode_entities(value));
        }
    }
    let mut from = 0;
    while let Some(p) = lower[from..].find("url(") {
        let at = from + p + 4;
        from = at;
        if let Some(close) = html[at..].find(')') {
            out.push(html[at..at + close].trim().trim_matches(['"', '\'']).to_string());
        }
    }
    out
}

/// Checks an `index.html` mock-up: its title, summary and anything that would need a script or another site.
pub fn html(src: &str, space: &Path, page: &Path) -> Rendered {
    let mut problems = vec![];
    let lower = src.to_ascii_lowercase();
    let title = element_text(src, "title");
    if title.is_none() {
        problems.push(Problem::new("The mock-up has no <title>: it's the page's name in the list."));
    }
    if lower.contains("<script") {
        problems.push(Problem::new("The mock-up has a <script>, which never runs here: use HTML and CSS only."));
    }
    if has_event_handler(&lower) {
        problems.push(Problem::new("The mock-up has an on…= attribute (a script), which never runs here."));
    }
    let mut seen = std::collections::BTreeSet::new();
    for r in references(src) {
        if !seen.insert(r.clone()) {
            continue;
        }
        let l = r.trim().to_ascii_lowercase();
        if l.starts_with("javascript:") {
            problems.push(Problem::new("The mock-up has a javascript: link, which never runs here."));
            continue;
        }
        if l.starts_with("data:") {
            continue;
        }
        match classify(&r, page) {
            Dest::External if l.starts_with("http") && !l.starts_with("#") => {
                // Links to other sites are fine; files loaded from them aren't. We can't tell which an href is for
                // a <link rel=stylesheet>, so flag stylesheets, fonts and images by extension.
                if is_asset(&l) {
                    problems.push(Problem::new(format!(
                        "The mock-up loads {r} from another site, which is blocked: put the file in the page's folder."
                    )));
                }
            }
            Dest::External => {}
            Dest::Local(rel) => check_local(space, &rel, "file", &r, is_image(&l), &mut problems),
            Dest::Outside => {
                problems.push(Problem::new(format!("The mock-up refers to {r}, outside your pages folder.")))
            }
            Dest::Refused => problems.push(Problem::new(format!("The mock-up refers to {r}, which isn't allowed."))),
        }
    }
    Rendered { kind: Kind::Html, title, summary: meta_description(src), body: String::new(), problems }
}

pub(crate) fn has_event_handler(lower: &str) -> bool {
    let b = lower.as_bytes();
    let mut i = 0;
    while let Some(p) = lower[i..].find(" on") {
        let at = i + p + 3;
        let mut j = at;
        while j < b.len() && b[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j > at && j < b.len() {
            let mut k = j;
            while k < b.len() && b[k] == b' ' {
                k += 1;
            }
            if k < b.len() && b[k] == b'=' {
                // Only inside a tag: the last '<' before it comes after the last '>'.
                let lt = lower[..at].rfind('<');
                let gt = lower[..at].rfind('>');
                if lt.is_some() && (gt.is_none() || lt > gt) {
                    return true;
                }
            }
        }
        i = at;
    }
    false
}

fn ext(l: &str) -> &str {
    let path = l.split(['#', '?']).next().unwrap_or_default();
    path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default()
}

fn is_image(l: &str) -> bool {
    matches!(ext(l), "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "svg")
}

pub(crate) fn is_asset(l: &str) -> bool {
    is_image(l) || matches!(ext(l), "css" | "woff" | "woff2" | "ttf" | "otf" | "js")
}

/// Renders whatever is in a page's folder.
pub fn page(space: &Path, page: &Path) -> Rendered {
    let dir = space.join(page);
    if let Ok(src) = std::fs::read_to_string(dir.join("index.md")) {
        return markdown(&src, space, page);
    }
    if let Ok(src) = std::fs::read_to_string(dir.join("index.html")) {
        return html(&src, space, page);
    }
    Rendered {
        kind: Kind::Empty,
        title: None,
        summary: None,
        body: String::new(),
        problems: vec![Problem::new("The folder has neither index.md nor index.html.")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("p/img")).unwrap();
        std::fs::write(d.path().join("p/img/a.jpg"), b"x").unwrap();
        std::fs::write(d.path().join("p/img/b.jpg"), b"x").unwrap();
        std::fs::create_dir_all(d.path().join("other")).unwrap();
        d
    }

    fn md(src: &str) -> Rendered {
        let d = space();
        markdown(src, d.path(), Path::new("p"))
    }

    #[test]
    fn the_title_and_summary_come_from_the_first_heading_and_paragraph() {
        let r = md("# Signing QA\n\nWhat this shows,\nfrom today.\n\n## Desktop\n\nText.\n");
        assert_eq!(r.title.as_deref(), Some("Signing QA"));
        assert_eq!(r.summary.as_deref(), Some("What this shows, from today."));
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert!(!r.body.contains("Signing QA"));
        assert!(!r.body.contains("What this shows"));
        assert!(r.body.contains("<h2>Desktop</h2>"));
    }

    #[test]
    fn a_missing_title_and_summary_are_problems() {
        let r = md("Just text.\n");
        assert!(r.title.is_none());
        assert!(r.problems[0].message.contains("title"));
        let r = md("# T\n\n## Straight to a heading\n");
        assert!(r.summary.is_none());
        assert!(r.problems.iter().any(|p| p.message.contains("summary")));
    }

    #[test]
    fn consecutive_images_make_a_gallery_with_captions() {
        let r = md("# T\n\nS.\n\n![Light](img/a.jpg)\n![Dark](img/b.jpg)\n");
        assert!(r.body.contains(r#"<div class="gallery">"#), "{}", r.body);
        assert_eq!(r.body.matches(r#"<figure class="shot">"#).count(), 2);
        assert!(r.body.contains("<figcaption>Light</figcaption>"));
        assert!(r.body.contains(r#"<a class="lb" href="img/a.jpg">"#));
        assert!(r.problems.is_empty(), "{:?}", r.problems);
    }

    #[test]
    fn an_image_on_its_own_is_a_single_figure_and_inline_stays_inline() {
        let r = md("# T\n\nS.\n\n![One](img/a.jpg)\n\nSee ![tiny](img/b.jpg) here.\n");
        assert!(r.body.contains(r#"<figure class="single">"#));
        assert!(
            r.body.contains(r#"<p>See <a class="lb" href="img/b.jpg"><img src="img/b.jpg" alt="tiny" />"#),
            "{}",
            r.body
        );
    }

    #[test]
    fn raw_html_is_shown_as_text_and_flagged() {
        let r = md("# T\n\nS.\n\n<script>alert(1)</script>\n\nHi <b>there</b>.\n");
        assert!(!r.body.contains("<script>"));
        assert!(r.body.contains("&lt;script&gt;"));
        assert!(!r.body.contains("<b>"));
        assert!(r.problems.iter().any(|p| p.message.contains("raw HTML")));
    }

    #[test]
    fn unsafe_links_are_neutralised() {
        let r = md("# T\n\nS.\n\n[x](javascript:alert(1)) [y](data:text/html,hi) [z](//evil.example/)\n");
        assert!(!r.body.contains("javascript:"));
        assert!(!r.body.contains("data:text"));
        assert!(!r.body.contains("//evil"));
        assert_eq!(r.problems.len(), 3, "{:?}", r.problems);
    }

    #[test]
    fn broken_and_escaping_paths_are_problems() {
        let r = md("# T\n\nS.\n\n![gone](img/nope.jpg)\n\n[up](../../etc/passwd) [ok](../other/) [hidden](.secret)\n");
        let m: Vec<_> = r.problems.iter().map(|p| p.message.as_str()).collect();
        assert!(m.iter().any(|p| p.contains("img/nope.jpg") && p.contains("isn't in")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("outside your pages folder")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("hidden")), "{m:?}");
        assert!(!m.iter().any(|p| p.contains("../other/")), "{m:?}");
    }

    #[test]
    fn external_images_are_flagged_and_safe_links_pass() {
        let r = md(
            "# T\n\nS.\n\n![x](https://example.com/a.png)\n\n[site](https://example.com) [mail](mailto:a@b.c) [top](#t)\n",
        );
        assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
        assert!(r.body.contains(r#"href="https://example.com""#));
    }

    #[test]
    fn callouts_tables_and_task_lists_render() {
        let r = md("# T\n\nS.\n\n> [!WARNING]\n> Careful.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n");
        assert!(r.body.contains("markdown-alert-warning"), "{}", r.body);
        assert!(r.body.contains("<table>"));
        assert!(r.body.contains("checkbox"));
    }

    #[test]
    fn mock_ups_are_checked_for_scripts_titles_and_files() {
        let d = space();
        let r = html(
            r#"<html><head><title>Editor &amp; mock</title><meta name="description" content="Two layouts.">
            <link rel="stylesheet" href="style.css"><link rel="stylesheet" href="https://cdn.example/x.css"></head>
            <body onload="x()"><script>1</script><img src="img/a.jpg"><a href="javascript:x()">x</a>
            <div data-src="nothing"></div></body></html>"#,
            d.path(),
            Path::new("p"),
        );
        assert_eq!(r.title.as_deref(), Some("Editor & mock"));
        assert_eq!(r.summary.as_deref(), Some("Two layouts."));
        let m: Vec<_> = r.problems.iter().map(|p| p.message.as_str()).collect();
        assert!(m.iter().any(|p| p.contains("<script>")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("on…=")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("javascript:")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("style.css") && p.contains("isn't in")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("cdn.example")), "{m:?}");
        assert!(!m.iter().any(|p| p.contains("img/a.jpg")), "{m:?}");
        assert!(!m.iter().any(|p| p.contains("nothing")), "{m:?}");
    }

    #[test]
    fn a_clean_mock_up_has_no_problems() {
        let d = space();
        let r = html("<title>M</title><p style=\"color:red\">Online now</p>", d.path(), Path::new("p"));
        assert!(r.problems.is_empty(), "{:?}", r.problems);
    }

    #[test]
    fn hidden_and_private_paths_are_not_servable() {
        assert!(servable(Path::new("p/img/a.jpg")));
        assert!(!servable(Path::new(".fleetlayer-space")));
        assert!(!servable(Path::new("p/.git/config")));
        assert!(!servable(Path::new("_drafts/x")));
        assert!(!servable(Path::new("../x")));
    }

    #[test]
    fn percent_encoded_paths_resolve() {
        assert_eq!(classify("img/my%20shot.jpg", Path::new("p")), Dest::Local(PathBuf::from("p/img/my shot.jpg")));
        assert_eq!(classify("a%2", Path::new("p")), Dest::Local(PathBuf::from("p/a%2")));
    }
}
