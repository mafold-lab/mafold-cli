//! Work one agent hands another (flag `a2aThreads`, `.docs/a2a-threads.md`).
//!
//! A person's question is answered where it was asked. Work an agent hands out
//! is done in a thread under the message that handed it out, and only the
//! conclusion comes back to the timeline. Measured over 10-10..10-31 in Mafold
//! DEV: 160 such exchanges made 41% of the top-level messages, and the two that
//! reached a thread got there by accident.
//!
//! The daemon and the hosted dispatcher both ask this module: a rule that lives
//! twice drifts (`steer.rs`, for the same reason).

/// The flag. Off: nothing here runs and agents get the older words.
pub const FLAG: &str = "a2aThreads";

/// The thread a reply belongs in: the one its trigger is in, else — when the
/// trigger is an agent handing this account work in a group — a new one rooted
/// on the trigger. `None`: the timeline, beside the trigger.
pub fn reply_root<T>(trigger: T, trigger_thread: Option<T>, handed_work: bool) -> Option<T> {
    trigger_thread.or(handed_work.then_some(trigger))
}

/// Is this message an agent handing `me` work: agent-written, a group, and `me`
/// called the way an agent calls, an @ that opens a line. A person's @ is
/// answered in place, and so is a quote-reply, which the agent's own @ isn't.
pub fn handed_work(on: bool, in_group: bool, by_agent: bool, summoned_me: bool) -> bool {
    on && in_group && by_agent && summoned_me
}

/// Where a turn of `me` stands in a thread it was called into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The thread hangs under `me`'s own message: this is the worker reporting back.
    Handing,
    /// Someone else's thread: `me` is doing the work.
    Worker,
}

/// `root_sender`: who wrote the thread's root.
pub fn role(me: &str, root_sender: &str) -> Role {
    if me.eq_ignore_ascii_case(root_sender) { Role::Handing } else { Role::Worker }
}

/// The line in front of a message that hands this agent work.
pub fn worker_frame(sender: &str) -> String {
    format!(
        "[AI account @{sender} handed you this. Your reply lands in a thread under its message, and \
         `mafold send` / `read` stay in that thread. Report milestones only, never \"still running\". \
         When it's done or you're stuck, on a line of its own @ {sender} once and say in five lines \
         at most what happened and where the evidence is.]"
    )
}

/// The line in front of a worker's report, for the agent that handed out the
/// work. `shell`: it has a `mafold` to run. A hosted agent doesn't.
pub fn handing_frame(sender: &str, chat: &str, shell: bool) -> String {
    let up = if shell {
        format!(
            "If someone is waiting, take the conclusion to them: `mafold send {chat} --up \"…\"` posts \
             it on the timeline this thread hangs from, quoting what was asked. The conclusion, not \
             the process."
        )
    } else {
        "Say the conclusion in your reply: what came of it, not the process.".to_string()
    };
    format!(
        "[@{sender} reported back on work you handed out. Check the evidence first. {up} If it needs \
         more work, @ {sender} on a line of its own. If it doesn't, don't @ it again: that is what \
         ends the exchange.]"
    )
}

/// Said once when a report was answered with neither of the two ways on: not
/// taken to the timeline, not handed back. The person who asked sees a thread
/// bar and nothing else, which is worse than the noise this replaces.
pub fn unsettled_nudge(chat: &str) -> String {
    format!(
        "You took in a report in this thread and nothing reached the timeline it hangs from. If \
         anyone is waiting on this, run `mafold send {chat} --up \"…\"` now with the conclusion. If \
         nobody is, say nothing more."
    )
}

/// How agents work with each other: the section of a system prompt, one text
/// for every kind of agent. `shell`: as [`handing_frame`].
pub fn advice(shell: bool) -> String {
    let tools = if shell {
        "`mafold send`, `read` and `attach` already stay in the thread your turn is in"
    } else {
        "your replies stay in the thread your turn is in"
    };
    let conclude = if shell {
        "then take the conclusion to whoever asked: `mafold send <chat> --up \"…\"` posts it on the \
         timeline the thread hangs from, quoting the question. "
    } else {
        "then say the conclusion in your reply. "
    };
    let own = if shell {
        "\n  • A long job of your own? Keep its steps under your own message: `mafold send <chat> \
         --thread <your message id> …`."
    } else {
        ""
    };
    format!(
        "TALKING TO OTHER AGENTS (A2A): other Mafold agents sit in this conversation like any person, \
         and @-mentioning one by its handle (`@owner:botname`) is what summons it. For an AI sender \
         that is the ONE door. The @ must OPEN A LINE: start a new line with the handle, then say \
         what you want. A handle anywhere else — mid-sentence, in a list item, a quote, a table or \
         code — is only a name and summons nobody; when you just mention an agent, write its name \
         without the @.\n\
         WORK GOES IN THREADS: a person's question is answered where it was asked, and work an agent \
         hands out is done in a thread under the message that handed it out. The platform puts your \
         reply there; {tools}. Results go up one level and no further, so the timeline holds the \
         hand-off and the conclusion.\n  \
         • Handed work? Report milestones only, never \"still running\". Done or stuck: on a line of \
         its own @ whoever handed it to you, once, and say in five lines at most what happened and \
         where the evidence is.\n  \
         • Handing work out? One job, one message: what you want, what counts as done, what evidence \
         you want. Never post a job in several channels. Work that spans days gets its own channel, \
         with a summary posted back.\n  \
         • Work came back? Check the evidence, {conclude}Conclusion only, not the process. No more \
         work means no more @: that ends the exchange, and it is the only brake on two agents \
         volleying forever.{own}\n\
         An @ only lands if that agent's owner allows you — you, or your owner, on its list. If one \
         never answers, that is usually why: say so instead of @-ing it again. All of this happens \
         in the open: the people here read every turn and can cut in at any point."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_keeps_its_replies_and_handed_work_opens_one() {
        assert_eq!(reply_root(1, Some(9), true), Some(9));
        assert_eq!(reply_root(1, Some(9), false), Some(9));
        assert_eq!(reply_root(1, None, true), Some(1));
        assert_eq!(reply_root(1, None, false), None);
    }

    #[test]
    fn only_an_agents_opening_at_in_a_group_hands_work() {
        assert!(handed_work(true, true, true, true));
        assert!(!handed_work(false, true, true, true), "flag off");
        assert!(!handed_work(true, false, true, true), "a DM has no timeline to keep clear");
        assert!(!handed_work(true, true, false, true), "a person is answered in place");
        assert!(!handed_work(true, true, true, false), "a quote-reply isn't a hand-off");
    }

    #[test]
    fn who_wrote_the_root_says_who_is_reporting_to_whom() {
        assert_eq!(role("ops:claude", "OPS:Claude"), Role::Handing);
        assert_eq!(role("ops:claude", "linsky:rei"), Role::Worker);
    }

    #[test]
    fn an_agent_without_a_shell_is_never_told_to_run_one() {
        assert!(advice(true).contains("--up"));
        assert!(!advice(false).contains("mafold send"));
        assert!(!handing_frame("a", "c", false).contains("mafold send"));
        assert!(handing_frame("a", "c", true).contains("mafold send c --up"));
    }
}
