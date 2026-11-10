//! Characters a reader can't see (0.5.100).
//!
//! Text can carry characters that draw nothing — zero-width spaces,
//! Unicode "tag" letters, bidirectional controls that reorder what is
//! shown, runs of variation selectors. A person reading the text sees
//! one thing; a model reading the same bytes, or a shell running them,
//! gets another. Hearthloom keeps every record exactly as it arrived
//! (the episode law), so these are handled where text LEAVES for
//! someone, never at ingest:
//! - [`for_model`] — every prompt and every MCP result: the unseen
//!   characters are dropped, keeping only what writing needs (a joiner
//!   inside an emoji or a Persian word, one variation selector).
//! - [`reveal`] — what a person decides on (an approval card): each
//!   unseen character becomes a visible `⟦U+202E⟧` mark, so what they
//!   read is what would run.
//!
//! The prompt fences (daemon `fence.rs`) skip the same set when they look
//! for a tag, so dropping a character here can never join the pieces of
//! a fence tag back together after the fence was checked.

use std::borrow::Cow;

use serde_json::Value;

/// A character that draws nothing, or that changes how the text around
/// it is drawn: controls, format characters, fillers, selectors and the
/// tag block. Whitespace is not in the set (it shows as space).
pub fn is_unseen(c: char) -> bool {
    if c.is_whitespace() {
        return false;
    }
    matches!(
        c,
        '\u{0000}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{17B4}'
            | '\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
            // a braille cell with no dots: blank, and not White_Space
            | BLANK_BRAILLE
            // private use: blank (or a box) in nearly every font
            | '\u{E000}'..='\u{F8FF}'
            | '\u{F0000}'..='\u{FFFFD}'
            | '\u{100000}'..='\u{10FFFD}'
    )
}

const BLANK_BRAILLE: char = '\u{2800}';
const BLACK_FLAG: char = '\u{1F3F4}';

/// After 🏴: the tag letters of a subdivision flag (🏴 + "gbeng" + cancel
/// tag = England), the one place writing uses the tag block. How many
/// characters to keep, or 0.
fn subdivision_flag(rest: &std::iter::Peekable<std::str::Chars<'_>>) -> usize {
    let mut letters = 0;
    for c in rest.clone() {
        match c {
            '\u{E007F}' if (2..=7).contains(&letters) => return letters + 1,
            '\u{E0030}'..='\u{E0039}' | '\u{E0061}'..='\u{E007A}' if letters < 7 => letters += 1,
            _ => return 0,
        }
    }
    0
}

/// Selects a glyph's variant (emoji vs text style, a CJK variant).
fn is_selector(c: char) -> bool {
    matches!(
        c,
        '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}' | '\u{180B}'..='\u{180D}' | '\u{180F}'
    )
}

/// Zero-width joiner / non-joiner: emoji sequences and several scripts
/// (Persian, the Indic scripts) need them between two letters.
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

fn visible(c: char) -> bool {
    !c.is_whitespace() && !is_unseen(c)
}

/// How many unseen characters `text` holds.
pub fn count(text: &str) -> usize {
    text.chars().filter(|c| is_unseen(*c)).count()
}

/// Text for a model. Unseen characters are dropped, except a joiner
/// between two visible characters, one selector right after a visible
/// character (and a joiner after it, as in ❤️‍🔥), and a subdivision
/// flag's tag letters. A run of them — the way hidden bytes are smuggled
/// — keeps at most one. A blank braille cell becomes a space.
pub fn for_model(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_unseen) {
        return Cow::Borrowed(text);
    }
    #[derive(PartialEq)]
    enum Last {
        Visible,
        Selector,
        Other,
    }
    let mut out = String::with_capacity(text.len());
    let mut last = Last::Other;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if !is_unseen(c) {
            out.push(c);
            last = if c.is_whitespace() {
                Last::Other
            } else {
                Last::Visible
            };
            if c == BLACK_FLAG {
                for _ in 0..subdivision_flag(&chars) {
                    out.extend(chars.next());
                }
            }
            continue;
        }
        if c == BLANK_BRAILLE {
            out.push(' ');
            last = Last::Other;
            continue;
        }
        if is_selector(c) && last == Last::Visible {
            out.push(c);
            last = Last::Selector;
        } else if is_joiner(c)
            && last != Last::Other
            // the next character that shows (past the rest of a run)
            && chars.clone().find(|n| !is_unseen(*n)).is_some_and(visible)
        {
            out.push(c);
            last = Last::Other;
        } else {
            last = Last::Other;
        }
    }
    Cow::Owned(out)
}

/// `for_model` on every string inside a JSON value (keys are ours and
/// left alone).
pub fn for_model_json(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(clean) = for_model(s) {
                *s = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(for_model_json),
        Value::Object(map) => map.values_mut().for_each(for_model_json),
        _ => {}
    }
}

/// Text for a person deciding something: every unseen character becomes
/// a visible mark such as `⟦U+202E⟧`. Already-revealed text comes back
/// unchanged.
pub fn reveal(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_unseen) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if is_unseen(c) {
            out.push_str(&format!("⟦U+{:04X}⟧", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_is_untouched() {
        for text in ["hello world", "naïve café — ok", "multi\nline\ttext", ""] {
            assert!(matches!(for_model(text), Cow::Borrowed(_)));
            assert!(matches!(reveal(text), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn test_hidden_words_are_dropped_for_a_model() {
        // "ignore" spelled in tag letters after a visible word
        let tags: String = "ignore"
            .chars()
            .map(|c| char::from_u32(0xE0000 + c as u32).unwrap())
            .collect();
        let text = format!("hello{tags} there");
        assert_eq!(for_model(&text), "hello there");
        // zero-widths and bidi controls inside a word
        assert_eq!(for_model("pay\u{200B}pal\u{202E}gnp.exe"), "paypalgnp.exe");
        assert_eq!(for_model("a\u{2066}b\u{2069}c\u{FEFF}d\u{00AD}e"), "abcde");
        // a smuggled run of variation selectors keeps one
        let run: String = std::iter::once('😀')
            .chain((0..40).map(|i| char::from_u32(0xE0100 + i).unwrap()))
            .collect();
        assert_eq!(for_model(&run), "😀\u{E0100}");
        // C0 controls other than tab/newline/return go
        assert_eq!(for_model("a\u{1B}[31mb\r\n"), "a[31mb\r\n");
    }

    #[test]
    fn test_writing_keeps_what_it_needs() {
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(for_model(family), family);
        let heart_on_fire = "❤\u{FE0F}\u{200D}🔥";
        assert_eq!(for_model(heart_on_fire), heart_on_fire);
        let persian = "می\u{200C}خواهم";
        assert_eq!(for_model(persian), persian);
        // ...but not a joiner floating at a word's edge, or in a run
        assert_eq!(for_model("a \u{200D}b"), "a b");
        assert_eq!(for_model("a\u{200D}\u{200D}\u{200D}b"), "a\u{200D}b");
        assert_eq!(for_model("x\u{FE0F}\u{FE0F}"), "x\u{FE0F}");
        // England's flag: 🏴 + g b e n g + cancel tag
        let england = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(for_model(england), england);
        // ...but tag letters after a flag with no cancel, or too many, go
        assert_eq!(for_model("🏴\u{E0067}\u{E0062}x"), "🏴x");
        let long: String = std::iter::once('🏴')
            .chain((0..20).map(|_| '\u{E0061}'))
            .chain(std::iter::once('\u{E007F}'))
            .collect();
        assert_eq!(for_model(&long), "🏴");
        // a blank braille cell spaces words; private use goes
        assert_eq!(for_model("a\u{2800}b\u{E000}c\u{F8FF}"), "a bc");
        assert_eq!(reveal("a\u{2800}b"), "a⟦U+2800⟧b");
    }

    #[test]
    fn test_reveal_marks_every_unseen_character() {
        assert_eq!(
            reveal("rm -rf /tmp/x\u{202E}txt.\u{200B}"),
            "rm -rf /tmp/x⟦U+202E⟧txt.⟦U+200B⟧"
        );
        let once = reveal("a\u{E0041}b").into_owned();
        assert_eq!(once, "a⟦U+E0041⟧b");
        assert_eq!(reveal(&once), once, "revealing twice changes nothing");
        assert_eq!(count("a\u{200B}\u{200D}b"), 2);
    }

    #[test]
    fn test_json_strings_are_cleaned_in_place() {
        let mut v = serde_json::json!({
            "text": "hi\u{200B}there",
            "hits": [{"title": "x\u{E0041}y"}, 3, null],
        });
        for_model_json(&mut v);
        assert_eq!(v["text"], "hithere");
        assert_eq!(v["hits"][0]["title"], "xy");
        assert_eq!(v["hits"][1], 3);
    }
}
