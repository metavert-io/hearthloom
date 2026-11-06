//! A reply as an RFC 5322 message (0.5.94) — what a draft is saved as.
//!
//! The message is 7-bit only: header values with anything beyond ASCII
//! become RFC 2047 encoded words and the body is base64, so an IMAP
//! APPEND needs no UTF-8 literal and every server takes it. Whatever comes
//! from other people's mail (their name, the subject, message ids) loses
//! its line breaks and control characters first, so it can't add a
//! header of its own.
use base64::Engine;

#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// the owner: (display name, address)
    pub from: (String, String),
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    /// the subject of the message answered ("Re: " is added once)
    pub subject: String,
    /// the Message-ID answered, without angle brackets
    pub in_reply_to: Option<String>,
    /// its References, oldest first, without angle brackets
    pub references: Vec<String>,
    /// this draft's own Message-ID, without angle brackets
    pub message_id: String,
    /// RFC 2822 date
    pub date: String,
    /// the reply text
    pub body: String,
    /// "On … wrote:" and the quoted original, appended below the reply
    pub quoted: Option<String>,
}

/// One line, printable, no header tricks.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A message id as `<…>`: printable ASCII only, no brackets or spaces.
fn msg_id(id: &str) -> Option<String> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    let ok = !id.is_empty()
        && id.len() <= 250
        && id.contains('@')
        && id
            .chars()
            .all(|c| c.is_ascii_graphic() && !"<>()[]\\\",;".contains(c));
    ok.then(|| format!("<{id}>"))
}

/// An address the header can carry: plain ASCII, one `@`, no specials.
fn address(addr: &str) -> Option<String> {
    let addr = addr.trim();
    let ok = addr.len() <= 254
        && addr.matches('@').count() == 1
        && !addr.starts_with('@')
        && !addr.ends_with('@')
        && addr
            .chars()
            .all(|c| c.is_ascii_graphic() && !"<>()[]\\\",;:".contains(c));
    ok.then(|| addr.to_string())
}

/// RFC 2047 encoded words for text beyond ASCII, folded so no line runs
/// past 76 characters.
fn encoded(text: &str) -> String {
    let text = clean(text);
    if text.is_ascii() {
        return text;
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        // 45 bytes encode to 60 characters: with the wrapper, under 76
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", engine.encode(&chunk)));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", engine.encode(&chunk)));
    }
    words.join("\r\n ")
}

fn mailbox((name, addr): &(String, String)) -> Option<String> {
    let addr = address(addr)?;
    let name = clean(name);
    Some(if name.is_empty() {
        addr
    } else if name.is_ascii() {
        format!(
            "\"{}\" <{addr}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )
    } else {
        format!("{} <{addr}>", encoded(&name))
    })
}

/// "Re: " once, whatever the subject already starts with.
pub fn reply_subject(subject: &str) -> String {
    let s = clean(subject);
    let lower = s.to_lowercase();
    if lower.starts_with("re:") || lower.starts_with("aw:") || lower.starts_with("sv:") {
        s
    } else if s.is_empty() {
        "Re:".into()
    } else {
        format!("Re: {s}")
    }
}

/// The original, quoted below the reply ("On …, … wrote:" then "> " lines).
pub fn quote(date: &str, from: &str, text: &str, max_chars: usize) -> String {
    let text: String = text.chars().take(max_chars).collect();
    let mut out = format!("On {}, {} wrote:\n", clean(date), clean(from));
    for line in text.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The whole message, CRLF line ends. `Err` when there's nobody valid to
/// send it to or the owner's address isn't usable.
pub fn rfc822(r: &Reply) -> Result<Vec<u8>, String> {
    let from = mailbox(&r.from).ok_or("the owner's address isn't usable")?;
    let to: Vec<String> = r.to.iter().filter_map(mailbox).collect();
    if to.is_empty() {
        return Err("nobody to reply to".into());
    }
    let cc: Vec<String> = r.cc.iter().filter_map(mailbox).collect();
    let own_id = msg_id(&r.message_id).ok_or("a draft needs its own Message-ID")?;
    let mut h = vec![
        format!("From: {from}"),
        format!("To: {}", to.join(",\r\n ")),
    ];
    if !cc.is_empty() {
        h.push(format!("Cc: {}", cc.join(",\r\n ")));
    }
    h.push(format!("Subject: {}", encoded(&reply_subject(&r.subject))));
    h.push(format!("Date: {}", clean(&r.date)));
    h.push(format!("Message-ID: {own_id}"));
    if let Some(parent) = r.in_reply_to.as_deref().and_then(msg_id) {
        h.push(format!("In-Reply-To: {parent}"));
        let mut refs: Vec<String> = r.references.iter().filter_map(|x| msg_id(x)).collect();
        if !refs.contains(&parent) {
            refs.push(parent);
        }
        // the root and the latest few are what threading needs
        if refs.len() > 10 {
            let keep: Vec<String> = std::iter::once(refs[0].clone())
                .chain(refs[refs.len() - 9..].iter().cloned())
                .collect();
            refs = keep;
        }
        h.push(format!("References: {}", refs.join("\r\n ")));
    }
    h.push("MIME-Version: 1.0".into());
    h.push("Content-Type: text/plain; charset=utf-8".into());
    h.push("Content-Transfer-Encoding: base64".into());
    let mut text = r.body.trim_end().replace("\r\n", "\n");
    if let Some(q) = &r.quoted {
        text.push_str("\n\n");
        text.push_str(q.trim_end());
    }
    let text = text.replace('\n', "\r\n") + "\r\n";
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = h.join("\r\n");
    out.push_str("\r\n\r\n");
    for chunk in b64.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Reply {
        Reply {
            from: ("Sam Owner".into(), "sam@owner.example".into()),
            to: vec![("Dana Müller".into(), "dana@fund.example".into())],
            subject: "Seed round".into(),
            in_reply_to: Some("<m2@fund.example>".into()),
            references: vec!["m1@fund.example".into()],
            message_id: "hl-draft-1@owner.example".into(),
            date: "Thu, 06 Nov 2026 10:00:00 +0000".into(),
            body: "Happy to talk — Tuesday works.".into(),
            quoted: Some(quote("Wed, 5 Nov", "Dana", "Can we talk?\nThanks", 1000)),
            ..Default::default()
        }
    }

    #[test]
    fn test_a_reply_threads_and_stays_seven_bit() {
        let raw = rfc822(&reply()).unwrap();
        assert!(raw.is_ascii(), "7-bit only");
        let text = String::from_utf8(raw.clone()).unwrap();
        assert!(text.contains("Subject: Re: Seed round\r\n"));
        assert!(text.contains("In-Reply-To: <m2@fund.example>\r\n"));
        assert!(text.contains("References: <m1@fund.example>\r\n <m2@fund.example>\r\n"));
        assert!(text.contains("To: =?UTF-8?B?"), "{text}");
        // it reads back as the same message
        let m = mail_parser::MessageParser::default().parse(&raw).unwrap();
        assert_eq!(m.subject(), Some("Re: Seed round"));
        assert_eq!(
            m.to().and_then(|t| t.first()).and_then(|a| a.name()),
            Some("Dana Müller")
        );
        let body = m.body_text(0).unwrap();
        assert!(body.contains("Tuesday works."));
        assert!(body.contains("> Can we talk?"));
        assert_eq!(
            m.in_reply_to()
                .and_then(|v| v.first())
                .map(|s| s.to_string()),
            Some("m2@fund.example".to_string())
        );
    }

    #[test]
    fn test_other_peoples_words_cannot_add_headers() {
        let mut r = reply();
        r.subject = "Hi\r\nBcc: thief@evil.example".into();
        r.to = vec![(
            "Dana\r\nBcc: thief@evil.example".into(),
            "dana@fund.example".into(),
        )];
        r.in_reply_to = Some("x@y>\r\nBcc: thief@evil.example".into());
        let text = String::from_utf8(rfc822(&r).unwrap()).unwrap();
        let headers = text.split("\r\n\r\n").next().unwrap();
        assert!(!headers.contains("\r\nBcc:"), "{headers}");
        assert!(!headers.contains("In-Reply-To"), "a bad id is dropped");
        // a bad address is dropped; nobody left is an error
        r.to = vec![("x".into(), "a@b@c".into())];
        assert!(rfc822(&r).is_err());
        assert_eq!(reply_subject("RE: x"), "RE: x");
        assert_eq!(reply_subject("x"), "Re: x");
    }
}
