//! Which model is answering: the line of the system prompt that names it. A
//! model told nothing answers "which model are you?" from its training, and an
//! open model that learned from Claude's writing says it is Claude — Beam on
//! OpenCode Go did, in a Mafold Harness agent on a computer (ops, 11-13).
//!
//! One text for every kind of agent, like [`crate::a2a::advice`]: the hosted
//! harness's model pick says it, and so does Mafold Harness on a computer.

/// The line naming the model a turn runs on: `id` as it goes out, `name` as
/// people know it (left out when it is only the id again), `on` where it is
/// served when the agent knows (an owner's OpenCode Go plan) — told nothing,
/// Beam made one up. `shell`: an agent on a computer, whose owner sets its
/// model; otherwise a hosted agent, whose sender picks it.
pub fn line(id: &str, name: &str, on: Option<&str>, shell: bool) -> String {
    let named = if name.is_empty() || name == id { format!("`{id}`") } else { format!("`{id}` ({name})") };
    let model = match on {
        Some(door) => format!("{named} on {door}"),
        None => named,
    };
    let change = if shell {
        "Your owner changes it on your profile's Customize sheet, or for one chat with `/model`."
    } else {
        "The sender can change it on your profile's Customize sheet."
    };
    format!(
        "You are currently serving replies with the model {model}. This is live configuration, \
         not a guess: when asked which model you are or which model is answering, state exactly \
         this. {change}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hosted agent hears what the hosted model pick has always said.
    #[test]
    fn a_hosted_agent_hears_the_words_it_always_did() {
        assert_eq!(
            line("m-pro", "M Pro", None, false),
            "You are currently serving replies with the model `m-pro` (M Pro). This is live \
             configuration, not a guess: when asked which model you are or which model is \
             answering, state exactly this. The sender can change it on your profile's Customize \
             sheet."
        );
    }

    #[test]
    fn an_agent_on_a_computer_hears_its_model_and_who_changes_it() {
        let said = line("beam-free", "Beam Free", Some("OpenCode Go"), true);
        assert!(said.starts_with("You are currently serving replies with the model `beam-free` (Beam Free) on OpenCode Go."), "{said}");
        assert!(said.ends_with("Your owner changes it on your profile's Customize sheet, or for one chat with `/model`."), "{said}");
    }

    /// No name known: the id once, not "`deepseek-v4` (deepseek-v4)".
    #[test]
    fn a_model_with_no_name_of_its_own_is_named_once() {
        for name in ["", "deepseek-v4"] {
            let said = line("deepseek-v4", name, None, true);
            assert!(said.starts_with("You are currently serving replies with the model `deepseek-v4`. "), "{said}");
        }
    }
}
