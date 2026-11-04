//! What keeps a message from being read as a program.
//!
//! `plugin.json` asks for markup, so a row's HTML-named tags are live on top of the
//! older tags (`<input>`, `<link>`, `<image>`, `<button>`), and nearly everything in
//! a row here was written by a stranger. A subject reading
//! `<img src="/home/user/secret.png">` must stay text.
//!
//! So what is kept is kept plain (headers, a text body, draft fields), and it goes
//! through `tags::escape` where a row is built. Coming back, `commit_edit` gets its
//! `new` plain ([`typed_value`] has the exception), a path segment is plain, and
//! anything read out of a row with `tags::extract_input` has to go through
//! [`unescape`].
//!
//! Two kinds of body need more than that:
//!
//! - **A structured (FFON) body** is a tree of row strings, and the sender chooses
//!   them, so it could hold a live `<button>send</button>`. On the wire a leaf a person
//!   wrote is `<input>text</input>` with plain text inside (what this plugin has
//!   always sent), and anything else is just text. [`received_rows`] and
//!   [`compose_rows`] read a tree from the wire, and [`wire_rows`] writes one.
//! - **An HTML body** is flattened to text by [`marked_text`], which keeps only what
//!   it can vouch for: links to `http` and `https`, with their URL still visible.

use sicompass_sdk::ffon::{FfonElement, FfonObject};
use sicompass_sdk::placeholders::is_i_placeholder;
use sicompass_sdk::tags;

const INPUT_OPEN: &str = "<input>";
const INPUT_CLOSE: &str = "</input>";
const LINK_OPEN: &str = "<link>";
const LINK_CLOSE: &str = "</link>";

/// Around a URL in a text body, for a link the HTML conversion found. Unicode
/// noncharacters, set aside for uses like this one. A text body is otherwise plain,
/// and nothing else can say which of its URLs were links. `net::parse_body_part`
/// strips them from everything it reads, so a sender cannot write one.
const LINK_MARK_OPEN: char = '\u{FDD0}';
const LINK_MARK_CLOSE: char = '\u{FDD1}';

/// Undo `tags::escape`.
///
/// Safe on text that was never escaped: only a backslash right before a bracket is
/// dropped, and `escape` never leaves a bracket without one.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(bracket) = chars.next_if(|next| matches!(next, '<' | '>'))
        {
            out.push(bracket);
            continue;
        }
        out.push(c);
    }
    out
}

/// What the user means by the `new` of a `commit_edit`.
///
/// `new` is what they typed, plain, and `old` the field as it was written, escaped.
/// The one exception is Enter on a field outside Insert mode, which only commits the
/// field: the app reads the value out of the row and passes the same raw slice as
/// both, so equal values are still escaped. Text with no bracket is the same either
/// way. Retyping a field to hold, literally, the escaped form of what it already
/// held looks identical and loses its backslash, and that is the price.
pub fn typed_value(old: &str, new: &str) -> String {
    if old == new {
        unescape(new)
    } else {
        new.to_owned()
    }
}

/// The text of a wire leaf the compose form wrote: `<input>text</input>`, whole.
fn wire_leaf_text(s: &str) -> Option<&str> {
    s.strip_prefix(INPUT_OPEN)?.strip_suffix(INPUT_CLOSE)
}

fn map_tree(elems: &[FfonElement], row: &impl Fn(&str) -> String) -> Vec<FfonElement> {
    elems
        .iter()
        .map(|elem| match elem {
            FfonElement::Str(s) => FfonElement::Str(row(s)),
            FfonElement::Obj(o) => FfonElement::Obj(FfonObject {
                key: row(&o.key),
                children: map_tree(&o.children, row),
            }),
        })
        .collect()
}

/// A tree from the wire as rows of a message being read: all text, nothing to edit.
pub fn received_rows(elems: &[FfonElement]) -> Vec<FfonElement> {
    map_tree(elems, &|s| {
        if is_i_placeholder(s) {
            return s.to_owned();
        }
        tags::escape(wire_leaf_text(s).unwrap_or(s))
    })
}

/// A tree from the wire (or typed as JSON) as rows of a draft: a person's leaf stays
/// editable, and anything else is text.
pub fn compose_rows(elems: &[FfonElement]) -> Vec<FfonElement> {
    map_tree(elems, &|s| {
        if is_i_placeholder(s) {
            return s.to_owned();
        }
        match wire_leaf_text(s) {
            Some(text) => tags::format_input(&tags::escape(text)),
            None => tags::escape(s),
        }
    })
}

/// The rows of a draft as they go on the wire, the inverse of [`compose_rows`]. The
/// escapes are how a row is shown, and a recipient must never see one.
pub fn wire_rows(elems: &[FfonElement]) -> Vec<FfonElement> {
    map_tree(elems, &|s| match wire_leaf_text(s) {
        Some(text) => tags::format_input(&unescape(text)),
        None => unescape(s),
    })
}

/// Whether a link in a stranger's HTML may stay a link. Following one fetches it, so
/// not a local path, and only what can sit between the tags as it is.
fn is_followable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '\\' | '"'))
}

/// A row out of `ffon::html_to_ffon` as text. Not all of what the conversion writes
/// is escaped (an image's `alt`, a table cell, a form control's name and value, a
/// link's `href`) and it writes live `<input>` and `<button>` for a form, so nothing
/// here is taken on trust: every tag goes, a form control is left as its text, and a
/// link as its URL, marked if [`is_followable`].
pub fn marked_text(row: &str) -> String {
    let row = strip_link_marks(row);
    let mut out = String::with_capacity(row.len());
    let mut rest = row.as_str();
    while let Some(open) = rest
        .match_indices(LINK_OPEN)
        .map(|(at, _)| at)
        .find(|&at| !rest[..at].ends_with('\\'))
    {
        let after = &rest[open + LINK_OPEN.len()..];
        let Some(close) = after.find(LINK_CLOSE) else {
            break;
        };
        out.push_str(&tags::strip_display(&rest[..open]));
        let url = &after[..close];
        if is_followable(url) {
            out.push(LINK_MARK_OPEN);
            out.push_str(url);
            out.push(LINK_MARK_CLOSE);
        } else {
            out.push_str(&tags::strip_display(url));
        }
        rest = &after[close + LINK_CLOSE.len()..];
    }
    out.push_str(&tags::strip_display(rest));
    out
}

/// `text` without the marks [`marked_text`] left in it.
pub fn strip_link_marks(text: &str) -> String {
    text.replace([LINK_MARK_OPEN, LINK_MARK_CLOSE], "")
}

/// The row for a text body: all of it escaped, with a marked URL a live `<link>`.
pub fn text_row(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once(LINK_MARK_OPEN) {
        let Some((url, tail)) = after.split_once(LINK_MARK_CLOSE) else {
            break;
        };
        out.push_str(&tags::escape(&strip_link_marks(before)));
        if is_followable(url) {
            out.push_str(LINK_OPEN);
            out.push_str(&tags::escape(url));
            out.push_str(LINK_CLOSE);
        } else {
            out.push_str(&tags::escape(url));
        }
        rest = tail;
    }
    out.push_str(&tags::escape(&strip_link_marks(rest)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sicompass_sdk::markup;

    #[test]
    fn unescape_undoes_escape_whatever_the_text_holds() {
        for text in [
            "plain",
            "<b>x</b>",
            r"a\<b",
            r"a\\<b\>",
            r"ends in a backslash\",
            r"C:\notes\<new>",
            "",
        ] {
            assert_eq!(unescape(&tags::escape(text)), text, "{text:?}");
        }
    }

    #[test]
    fn unescape_leaves_text_that_was_never_escaped() {
        assert_eq!(unescape(r"C:\notes"), r"C:\notes");
        assert_eq!(unescape("a < b"), "a < b");
    }

    #[test]
    fn a_typed_value_is_taken_as_typed() {
        assert_eq!(typed_value(r"\<b\>", "<b>"), "<b>");
        assert_eq!(typed_value("", r"\<b\>"), r"\<b\>");
    }

    #[test]
    fn enter_on_a_field_commits_its_value_not_its_escapes() {
        assert_eq!(typed_value(r"\<b\>", r"\<b\>"), "<b>");
    }

    #[test]
    fn a_marked_link_is_live_and_the_rest_of_the_row_is_not() {
        let text = marked_text(r"\<b\>see <link>https://example.com/a?b=1</link>");
        let row = text_row(&text);
        assert!(!markup::has_markup(&row), "{row}");
        let targets = markup::targets(&row);
        assert_eq!(targets.len(), 1, "{row}");
        assert_eq!(targets[0].value, "https://example.com/a?b=1");
        assert_eq!(
            tags::strip_display(&row),
            "<b>see https://example.com/a?b=1"
        );
        assert_eq!(strip_link_marks(&text), "<b>see https://example.com/a?b=1");
    }

    #[test]
    fn a_link_to_anywhere_but_the_web_stays_text() {
        for url in [
            "/home/user/secret.json",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b.c",
            "https://x y",
            r"https://x\",
        ] {
            let row = text_row(&marked_text(&format!("t <link>{url}</link>")));
            assert!(markup::targets(&row).is_empty(), "{url}: {row}");
            assert_eq!(tags::strip_display(&row), format!("t {url}"));
        }
    }

    #[test]
    fn a_mark_with_no_end_or_a_bad_url_makes_nothing() {
        for text in [
            "a\u{FDD0}https://x",
            "a\u{FDD0}/etc/passwd\u{FDD1}",
            "a\u{FDD1}https://x\u{FDD0}",
        ] {
            let row = text_row(text);
            assert!(markup::targets(&row).is_empty(), "{row}");
            assert!(
                !row.contains('\u{FDD0}') && !row.contains('\u{FDD1}'),
                "{row}"
            );
        }
    }

    #[test]
    fn a_leaf_goes_to_the_compose_form_and_back_to_the_wire_unchanged() {
        for s in [
            "<input>a <b> c</input>",
            r"<input>ends in a backslash\</input>",
            "<input>a</input>b</input>",
            "<button>send</button>Send",
            r#"<img src="/etc/passwd">"#,
            r"a\<b",
            "i <input></input>",
            "",
        ] {
            let wire = vec![FfonElement::new_str(s.to_owned())];
            assert_eq!(wire_rows(&compose_rows(&wire)), wire, "{s:?}");
        }
    }
}
