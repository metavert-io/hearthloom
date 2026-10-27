//! The machine-address rulebook: senders that are never people —
//! noreply@, bounce handlers, mailer daemons, ESP relay tokens, and
//! addresses whose local part is a long random sequence. These mint as
//! type `Service` instead of `Person` (and a consolidation sweep
//! reclassifies old ones), which removes them from every person
//! surface at once: Relationships, the galaxy, @-mention pickers, the
//! relationship predictor, and the merge scan. Episodes and provenance
//! are untouched — the mail is still the mail; the sender is just not
//! a connection.

/// Local-part markers that mean "a machine sent this".
const MACHINE_LOCAL_MARKERS: &[&str] = &[
    "noreply",
    "no-reply",
    "no_reply",
    "donotreply",
    "do-not-reply",
    "do_not_reply",
    "mailer-daemon",
    "mailerdaemon",
    "postmaster",
    "bounce",
    "bounces",
    "notification",
    "notifications",
    "notify",
    "alerts",
    "newsletter",
    "newsletters",
    "digest",
    "marketing",
    "mailings",
    "email-updates",
    "emailupdates",
    "auto-confirm",
    "autoconfirm",
    "unsubscribe",
    "undisclosed-recipients",
    // transactional order-tracking senders (orders@shop, order-status@…)
    "order",
    "orders",
    // hotel/travel reservation desks — a different guest each time
    "reservation",
    "reservations",
];

/// Domains that only ever relay machine mail (ESPs, gateways,
/// comment/notification bridges).
const MACHINE_DOMAIN_MARKERS: &[&str] = &[
    "amazonses.com",
    "sendgrid.net",
    "mailgun",
    "mandrillapp.com",
    "sparkpostmail",
    "pm-bounces",
    "bounces.google.com",
    "docs.google.com",
    "calendar-server.bounces.google.com",
    "txt.voice.google.com",
    "mail-relay",
    "email.mg",
    // Google Calendar machinery: opaque resource/room/group attendee
    // addresses (group./resource./import.calendar.google.com). Catches
    // the ones whose local part is too short for the entropy rule.
    "calendar.google.com",
    // LinkedIn's reply-relay: every message threads through a per-thread
    // token @reply.linkedin.com — never a person (Jon, 2026-07-29; these
    // were flooding Tend). bounce.linkedin.com likewise.
    "reply.linkedin.com",
    "bounce.linkedin.com",
];

/// True when an email address is machine-originated by rule.
pub fn is_machine_email(address: &str) -> bool {
    let address = address.trim().to_lowercase();
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    if MACHINE_DOMAIN_MARKERS.iter().any(|m| domain.contains(m)) {
        return true;
    }
    // marker must be a whole segment or prefix/suffix of one, not an
    // accidental substring of a real name ("cannotreplytoday" stays)
    let segments: Vec<&str> = local.split(['.', '+', '-', '_', '=']).collect();
    let joined = segments.join("-"); // normalized for two-word markers
    for marker in MACHINE_LOCAL_MARKERS {
        if segments.contains(marker) || joined.contains(marker) {
            return true;
        }
    }
    // reply-token forms: reply+<token>@, <list>+bounces@, SRS rewrites
    if local.starts_with("reply+") || local.starts_with("srs0=") || local.starts_with("srs1=") {
        return true;
    }
    // The entropy rule (Jon: "long alphanumeric random sequence"):
    // a hex run of 16+ (uuid-ish) or a 20+ alnum run carrying 5+ digits
    // is a generated token, not a mailbox a human reads.
    let mut hex_run = 0usize;
    let mut alnum_run = 0usize;
    let mut digits_in_run = 0usize;
    for c in local.chars() {
        if c.is_ascii_hexdigit() {
            hex_run += 1;
            if hex_run >= 16 {
                return true;
            }
        } else {
            hex_run = 0;
        }
        if c.is_ascii_alphanumeric() {
            alnum_run += 1;
            if c.is_ascii_digit() {
                digits_in_run += 1;
            }
            if alnum_run >= 20 && digits_in_run >= 5 {
                return true;
            }
        } else {
            alnum_run = 0;
            digits_in_run = 0;
        }
    }
    false
}

/// True when a would-be Person is actually a machine: every email it
/// carries is machine-ruled, or (with no email attributes) its NAME is
/// itself a machine address (mail adapters name entities by handle).
pub fn is_machine_person(name: &str, attributes: &[(String, String)]) -> bool {
    let emails: Vec<&String> = attributes
        .iter()
        .filter(|(kind, _)| kind == "email")
        .map(|(_, value)| value)
        .collect();
    if !emails.is_empty() {
        return emails.iter().all(|e| is_machine_email(e));
    }
    name.contains('@') && is_machine_email(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_machine_rules() {
        for addr in [
            "noreply@github.com",
            "no-reply@linkedin.com",
            "DoNotReply@stripe.com",
            "mailer-daemon@googlemail.com",
            "bounces+12345@sendgrid.net",
            "reply+ABCDEF123456@reply.github.com",
            "69badb5400b24800071e86d4@pm-bounces.example.com",
            "d41d8cd98f00b204e9800998@mail.example.com", // hex run
            "a1b2c3d4e5f6g7h8i9j0k1l2@weird.example.com", // long alnum+digits
            "notifications@slack.com",
            "comments-noreply@docs.google.com",
            "15555550100.15555550123.x7k2m9q4ab@txt.voice.google.com",
            // Google Calendar machinery (resource/group/opaque attendee ids)
            "room-7@resource.calendar.google.com",
            "abcdef@group.calendar.google.com",
            "q3ffz8m2tu9xk4wq7npl1a0bcd@google.com", // opaque id via entropy rule
            "orders@shopify.com",
            "order@somestore.com",
            "order-status@retailer.com",
            "reservations@hotel.com",
            "reservation@airline.com",
        ] {
            assert!(is_machine_email(addr), "{addr} should be machine");
        }
        for addr in [
            "jon@harbor.example",
            "priya.nair@ledger.example",
            "ada.lovelace+lists@gmail.com",
            "cannotreplytoday@example.com", // marker only as a substring of a word
            "abc123@gmail.com",             // short token — below the entropy bar
        ] {
            assert!(!is_machine_email(addr), "{addr} should be human");
        }
        assert!(is_machine_person(
            "noreply@github.com",
            &[("email".into(), "noreply@github.com".into())]
        ));
        assert!(!is_machine_person(
            "Ada Lovelace",
            &[("email".into(), "ada@example.com".into())]
        ));
        // mixed identities: one human email keeps them human
        assert!(!is_machine_person(
            "GitHub",
            &[
                ("email".into(), "noreply@github.com".into()),
                ("email".into(), "founders@github.com".into()),
            ]
        ));
    }
}
