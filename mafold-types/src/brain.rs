//! The brain card: how an agent's turn reads while it works (flag
//! `brainCard`). Its work — thinking, reads, edits, commands — goes into one
//! card that stops at a set height (`cards/brain`, drawn by
//! `mafold-transcript`), and the agent talks to the room in messages of its
//! own, the way a person texts an update. When it finishes, what it wrote last
//! is the reply, with the card's steps folded under it.
//!
//! The card is drawn by the driver; the talking is the agent's. This is the
//! paragraph of the system prompt that tells it so — one text for every kind
//! of agent, like [`crate::a2a::advice`].

/// The flag's key: read by the daemon as the bot (its owner's record covers
/// it).
pub const FLAG: &str = "brainCard";

/// What the agent is told about working under the brain card. `shell`: it
/// sends with `mafold send` (an agent on a computer); otherwise with its
/// `send_message` tool.
pub fn advice(shell: bool) -> String {
    let send = if shell {
        "send it as its own message with `mafold send <chat> \"…\"` (in a thread or a channel it \
         stays where your turn is)"
    } else {
        "send it as its own message with the `send_message` tool"
    };
    format!(
        "WHILE YOU WORK: your turn shows as a working card at the bottom of the chat — your \
         thinking, the files you read, your edits and commands, with the time and tokens — and \
         it stops at a set height, so the people here watch the work without reading it. What \
         you write between steps goes into that card; it isn't a message to anyone. When you have \
         something to tell them while you work — that you've started on a longer job, something \
         you found that changes the plan, a question that can't wait — {send}: short, the way a \
         person texts an update. The card moves under each message you send, so it stays last. \
         Send one when it's worth an interruption, not one per step, and none for a quick \
         answer. What you write last is your reply: the card gives way to it and its steps fold \
         under it. Make it the summary — what you did, what came of it, what's next — without \
         repeating what you already sent."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_way_to_send_matches_what_the_agent_has() {
        let shell = advice(true);
        assert!(shell.contains("`mafold send <chat>"), "{shell}");
        assert!(!shell.contains("send_message"), "{shell}");
        let hosted = advice(false);
        assert!(hosted.contains("`send_message` tool"), "{hosted}");
        assert!(!hosted.contains("mafold send"), "{hosted}");
        for s in [shell, hosted] {
            assert!(s.starts_with("WHILE YOU WORK:"), "{s}");
            assert!(s.contains("What you write last is your reply"), "{s}");
        }
    }
}
