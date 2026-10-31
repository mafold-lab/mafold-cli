//! Which address is whose — the ONE mapping between a Mafold handle and its
//! mail address at mafold.com, both ways. The api's rcpt hook, its mail
//! methods, @mail and the cli all go through these two functions, so an
//! address can't mean one account to the door and another to the reader.
//!
//! - `opsdu` ⇄ `opsdu@mafold.com`
//! - `opsdu:claude-code` ⇄ `opsdu.claude-code@mafold.com` — the `:` of an
//!   agent's handle is a `.` in its address, because `:` can't stand in one.
//! - `opsdu+github@mafold.com` reaches `opsdu`, tagged `github` (kept for
//!   display and filtering, never part of who it's for).
//!
//! That is unambiguous only because no handle contains `.` or `+` and no
//! handle has a second `:` (usernames are `[a-z0-9_]`, bot labels
//! `[a-z0-9_-]`), so `.` can mean "agent" here. There is no Gmail-style dot
//! folding: `o.psdu@` is the agent `o:psdu`, not `opsdu`. Case never matters.
//!
//! Who HAS a mailbox — a person, or an agent a person owns — is the api's to
//! decide from the accounts; this file only says which handle an address
//! names.

/// The domain every Mafold address is at.
pub const DOMAIN: &str = "mafold.com";

/// RFC 5321's limit on a local part. The longest handle a mailbox can have —
/// a 20-character username and a 32-character label — is 53 with its dot.
pub const MAX_LOCAL: usize = 64;

/// The local part of `handle`'s address: lowercased, `:` → `.`.
pub fn local_part(handle: &str) -> String {
    handle.trim().to_lowercase().replace(':', ".")
}

/// `handle`'s address at [`DOMAIN`].
pub fn address(handle: &str) -> String {
    format!("{}@{DOMAIN}", local_part(handle))
}

/// What an envelope recipient names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rcpt {
    /// The handle it is for, lowercase: `opsdu` or `opsdu:claude-code`.
    pub handle: String,
    /// The local part without the tag: `opsdu.claude-code`.
    pub local: String,
    /// What followed the first `+`, if anything did.
    pub tag: Option<String>,
}

impl Rcpt {
    /// The human whose namespace the address is in: the handle itself, or the
    /// part before the `:` of an agent's.
    pub fn owner(&self) -> &str {
        self.handle.split_once(':').map_or(self.handle.as_str(), |(o, _)| o)
    }

    /// The agent's label, for an agent's address.
    pub fn label(&self) -> Option<&str> {
        self.handle.split_once(':').map(|(_, l)| l)
    }
}

/// Why an envelope recipient names no handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcptError {
    /// Not at [`DOMAIN`] (or no `@` at all).
    NotOurDomain,
    /// At [`DOMAIN`], but no handle could be spelled that way: two dots, an
    /// empty part, a character no handle has, too long.
    Malformed,
}

/// The handle (and tag) an envelope recipient names. Case-insensitive; the
/// address may come wrapped in `<>` the way SMTP writes it.
pub fn parse_rcpt(rcpt: &str) -> Result<Rcpt, RcptError> {
    let lower = rcpt.trim().to_lowercase();
    let bare = lower.strip_prefix('<').and_then(|s| s.strip_suffix('>')).unwrap_or(&lower);
    let (local_full, domain) = bare.rsplit_once('@').ok_or(RcptError::NotOurDomain)?;
    if domain != DOMAIN {
        return Err(RcptError::NotOurDomain);
    }
    if local_full.is_empty() || local_full.chars().count() > MAX_LOCAL {
        return Err(RcptError::Malformed);
    }
    let (base, tag) = match local_full.split_once('+') {
        Some((b, t)) => (b, Some(t)),
        None => (local_full, None),
    };
    let tag = match tag {
        Some(t) if !t.chars().all(tag_char) => return Err(RcptError::Malformed),
        Some("") | None => None,
        Some(t) => Some(t.to_string()),
    };
    let handle = match base.split_once('.') {
        None if !base.is_empty() && base.chars().all(owner_char) => base.to_string(),
        Some((owner, label))
            if !owner.is_empty()
                && owner.chars().all(owner_char)
                && !label.is_empty()
                && label.chars().count() <= MAX_LABEL
                && label.chars().all(label_char) =>
        {
            format!("{owner}:{label}")
        }
        _ => return Err(RcptError::Malformed),
    };
    Ok(Rcpt { handle, local: base.to_string(), tag })
}

/// A bot label's longest (`bots_api::sanitize_label`).
const MAX_LABEL: usize = 32;

fn owner_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
}

fn label_char(c: char) -> bool {
    owner_char(c) || c == '-'
}

/// What a tag may hold: RFC 5322's atom characters and `.`, lowercase —
/// anything a sender's server would send unquoted. Never a space, a quote,
/// `@`, `<` or `>`, so a tag is safe to show next to the address.
fn tag_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || "!#$%&'*+-/=?^_`{|}~.".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(rcpt: &str) -> Result<String, RcptError> {
        parse_rcpt(rcpt).map(|r| r.handle)
    }

    #[test]
    fn a_handle_has_one_address_and_it_reads_back() {
        assert_eq!(address("opsdu"), "opsdu@mafold.com");
        assert_eq!(address("OpsDu:Claude-Code"), "opsdu.claude-code@mafold.com");
        for h in ["opsdu", "opsdu:claude-code", "ana_b", "x1:a_b-c"] {
            assert_eq!(handle(&address(h)).as_deref(), Ok(h), "{h}");
        }
    }

    #[test]
    fn case_never_matters() {
        assert_eq!(handle("OpsDu@Mafold.COM").as_deref(), Ok("opsdu"));
        assert_eq!(handle("OPSDU.CLAUDE-CODE@MAFOLD.COM").as_deref(), Ok("opsdu:claude-code"));
        assert_eq!(handle(" <opsdu@mafold.com> ").as_deref(), Ok("opsdu"), "SMTP's brackets and stray space");
    }

    #[test]
    fn a_tag_rides_along_and_names_nobody() {
        let r = parse_rcpt("opsdu+GitHub@mafold.com").unwrap();
        assert_eq!((r.handle.as_str(), r.local.as_str(), r.tag.as_deref()), ("opsdu", "opsdu", Some("github")));
        let r = parse_rcpt("opsdu.claude-code+signup.x@mafold.com").unwrap();
        assert_eq!((r.handle.as_str(), r.tag.as_deref()), ("opsdu:claude-code", Some("signup.x")));
        assert_eq!((r.owner(), r.label()), ("opsdu", Some("claude-code")));
        // Split at the FIRST `+`; an empty tag is no tag.
        assert_eq!(parse_rcpt("opsdu+a+b@mafold.com").unwrap().tag.as_deref(), Some("a+b"));
        assert_eq!(parse_rcpt("opsdu+@mafold.com").unwrap().tag, None);
        // A tag no server sends unquoted is refused, not shown.
        assert_eq!(handle("opsdu+a b@mafold.com"), Err(RcptError::Malformed));
        assert_eq!(handle("opsdu+<x>@mafold.com"), Err(RcptError::Malformed));
    }

    #[test]
    fn one_dot_is_an_agent_and_anything_else_is_nobody() {
        assert_eq!(handle("opsdu.claude-code@mafold.com").as_deref(), Ok("opsdu:claude-code"));
        let r = parse_rcpt("opsdu@mafold.com").unwrap();
        assert_eq!((r.owner(), r.label()), ("opsdu", None));
        for bad in [
            "a.b.c@mafold.com",       // two dots
            ".opsdu@mafold.com",      // empty owner
            "opsdu.@mafold.com",      // empty label
            "ops-du@mafold.com",      // `-` is a label's, not a username's
            "op-s.claude@mafold.com", // …nor an owner's
            "opsdu.cl:aude@mafold.com",
            "opsdu:claude@mafold.com", // the handle's own spelling isn't an address
            "o\"psdu@mafold.com",
            "@mafold.com",
            "+tag@mafold.com",
            "opsdu.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@mafold.com", // 33-character label
        ] {
            assert_eq!(handle(bad), Err(RcptError::Malformed), "{bad}");
        }
    }

    #[test]
    fn only_mafold_com_is_ours() {
        for bad in ["opsdu@gmail.com", "opsdu@mail.mafold.com", "opsdu@mafold.com.evil.io", "opsdu", "opsdu@"] {
            assert_eq!(handle(bad), Err(RcptError::NotOurDomain), "{bad}");
        }
    }

    #[test]
    fn the_longest_agent_address_fits() {
        let owner = "a".repeat(20);
        let label = "b".repeat(32);
        let h = format!("{owner}:{label}");
        let addr = address(&h);
        assert_eq!(local_part(&h).len(), 53);
        assert_eq!(handle(&addr), Ok(h.clone()));
        // A tag may take it to the RFC's 64, not past.
        let tagged = format!("{}+{}@{DOMAIN}", local_part(&h), "t".repeat(10));
        assert_eq!(parse_rcpt(&tagged).unwrap().handle, h);
        let over = format!("{}+{}@{DOMAIN}", local_part(&h), "t".repeat(11));
        assert_eq!(handle(&over), Err(RcptError::Malformed));
    }
}
