//! What is answering: the line of the system prompt that names the harness
//! and the model. A model told nothing answers "which model are you?" from its
//! training, and an open model that learned from Claude's writing says it is
//! Claude — Beam on OpenCode Go did, in a Mafold Harness agent on a computer
//! (ops, 11-13). Told only its model, the same agent read `opencode` in the
//! daemon's config and said there was no Mafold Harness (ops, 11-20).
//!
//! One text for every kind of agent, like [`crate::a2a::advice`]: the hosted
//! harness's model pick says it, and so does Mafold Harness on a computer.

/// The line naming what a turn runs on — Mafold Harness, where, and the
/// model: `id` as it goes out, `name` as people know it (left out when it is
/// only the id again), `on` what pays for it when the agent knows (an owner's
/// OpenCode Go plan) — told nothing, Beam made one up. `shell`: an agent on a
/// computer, whose owner sets its model; otherwise a hosted agent, whose
/// sender picks it.
pub fn line(id: &str, name: &str, on: Option<&str>, shell: bool) -> String {
    let named = if name.is_empty() || name == id { format!("`{id}`") } else { format!("`{id}` ({name})") };
    let model = match on {
        Some(door) => format!("{named}, on {door}"),
        None => named,
    };
    let (place, change) = if shell {
        (
            "here on your owner's computer",
            "Your owner changes the model on your profile's Customize sheet, or for one chat with `/model`.",
        )
    } else {
        ("on Mafold's servers", "The sender can change the model on your profile's Customize sheet.")
    };
    format!(
        "You run on Mafold Harness, Mafold's own agent loop, {place}, and you are currently serving \
         replies with the model {model}. This is live configuration, not a guess: when asked which \
         harness or model you run on, or which model is answering, state exactly this. {change}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hosted agent hears where it runs and on what, and who changes it.
    #[test]
    fn a_hosted_agent_hears_its_harness_and_model() {
        assert_eq!(
            line("m-pro", "M Pro", None, false),
            "You run on Mafold Harness, Mafold's own agent loop, on Mafold's servers, and you are \
             currently serving replies with the model `m-pro` (M Pro). This is live configuration, \
             not a guess: when asked which harness or model you run on, or which model is \
             answering, state exactly this. The sender can change the model on your profile's \
             Customize sheet."
        );
    }

    /// An agent on a computer hears it is Mafold Harness there — not the
    /// runtime id an older build stored (`opencode`), nor the plan that pays.
    #[test]
    fn an_agent_on_a_computer_hears_its_harness_model_and_who_changes_it() {
        let said = line("beam-free", "Beam Free", Some("your owner's OpenCode Go plan"), true);
        assert!(
            said.starts_with(
                "You run on Mafold Harness, Mafold's own agent loop, here on your owner's computer, \
                 and you are currently serving replies with the model `beam-free` (Beam Free), on \
                 your owner's OpenCode Go plan."
            ),
            "{said}"
        );
        assert!(said.ends_with("Your owner changes the model on your profile's Customize sheet, or for one chat with `/model`."), "{said}");
    }

    /// No name known: the id once, not "`deepseek-v4` (deepseek-v4)".
    #[test]
    fn a_model_with_no_name_of_its_own_is_named_once() {
        for name in ["", "deepseek-v4"] {
            let said = line("deepseek-v4", name, None, true);
            assert!(said.contains("serving replies with the model `deepseek-v4`. "), "{said}");
        }
    }
}
