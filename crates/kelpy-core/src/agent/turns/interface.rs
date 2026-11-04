//! What the model is told about where the administrator talks to it: the Web UI's controls, each with its value in
//! the conversation and what the other values allow, and the main things to know about Kelpy. With it the model can
//! say which control to change, instead of only what it cannot do.
//!
//! The names are the Web UI's own: a test reads them in its sources, so a control renamed there fails here.

use kelpy_storage::{Access, Approval, Modes};
use kelpy_tools::Account;

/// A value of a selector, as the Web UI names it, and what it means for the model.
struct Choice<T> {
    value: T,
    name: &'static str,
    means: &'static str,
    /// Offered in the Web UI; one that is not is shown disabled.
    available: bool,
}

const ACCESS: [Choice<Access>; 3] = [
    Choice {
        value: Access::None,
        name: "None",
        means: "you only answer, and read nothing on this system",
        available: true,
    },
    Choice {
        value: Access::ReadOnly,
        name: "Read-only",
        means: "you may look at this system with your tools, changing nothing",
        available: true,
    },
    Choice {
        value: Access::Full,
        name: "Full",
        means: "you may also change the system: files, services, packages and databases with the typed tools, \
                anything else with commands you write",
        available: true,
    },
];

const APPROVAL: [Choice<Approval>; 3] = [
    Choice {
        value: Approval::Ask,
        name: "Ask",
        means: "each of your actions waits for the administrator's approval",
        available: true,
    },
    Choice {
        value: Approval::Plan,
        name: "Plan",
        means: "looking needs no approval; you propose the changes with propose_plan and the administrator approves \
                the plan once, its steps marked where they would always ask otherwise; a change outside the plan \
                waits for approval",
        available: true,
    },
    Choice {
        value: Approval::Auto,
        name: "Auto",
        means: "looking, and the changes Kelpy checks and puts back (files, services, package installations, \
                databases), need no approval; removing packages, stopping SSH, remote management or the network, and \
                changes to the SSH, sudo, login, firewall, network or boot configuration still wait for it",
        available: true,
    },
];

/// The section of the instructions on the Web UI, with the conversation's `modes`, and the main things to know,
/// with the `account` actions run as.
pub(super) fn interface(modes: Modes, account: &Account) -> String {
    let rights = match account.limits() {
        Some(limits) => format!(
            "{limits} When a change needs those rights, say so and how to restart Kelpy before asking for it."
        ),
        None => "It has administrator rights.".to_owned(),
    };
    let explanations = if modes.explanations { "on" } else { "off" };
    let approval_note = if modes.access == Access::None {
        " It cannot be changed while System access is None."
    } else {
        ""
    };
    format!(
        "## Where the administrator talks to you

The administrator uses Kelpy's Web UI. Under the message box are three controls; each applies to this conversation \
only, and the administrator may change it at any time.
- Explanations, a toggle, now {explanations}: on, you add an `explanation` block after each step, shown as a tab \
of the command's code block it follows, or else as a callout with its own Copy that a link under the answer opens or \
closes; off, you stay concise and write none.
- System access, now {access}. {access_choices}
- Approval mode, now {approval}.{approval_note} {approval_choices} The commands you write with `run_command` \
always wait for approval, in every mode.
Each action you take shows in your answer as a card with Approve and Deny; on a card that is not a command, Switch \
to Plan and Switch to Auto approve it and change the Approval mode. While an action runs, its card shows the time \
it has taken and Stop, which ends that action only: you are told, and your answer goes on. A plan shows as one card \
with its steps, each marked as it is carried out. The terminal button in the top bar opens the Command log: what \
you ran in this conversation. There, Restore puts a file back as it was before one of your changes, from its \
backup: that is the administrator's own action, told to you with their next message. Report gives the handover \
report of the conversation (what changed, what did not complete, where secrets went, what stays on the server), \
and Export the operations that succeeded, with their arguments. When Kelpy finds that something it changed was \
changed outside it, a bar above the conversation says so, and the administrator settles each difference with Accept \
or Put back. Stop, beside the message box, ends your answer. The AI provider menu in the top bar changes the service \
and the model (Change provider).

When a request needs a setting other than the current one, say which control to change, where it is and to which \
value. When no setting allows it, say so plainly. Name the controls and their values as written here, in English, whatever language \
you answer in. Promise no Kelpy feature, version or date beyond what is described here.

## Things to know

Kelpy runs on this server, started by the administrator for this session: it stops when they stop it. Conversations \
are kept in Kelpy's data folder and survive a restart. What you read on the system is sent to the AI service with \
the conversation. Files that may hold secrets (private keys, `.env` files, password stores, Kelpy's own data) are \
never shown to you. Your actions run as the account that started Kelpy, {account}. {rights} An action \
not approved within 30 minutes counts as denied. One answer may use the tools up to 20 times. If Kelpy stops while \
a change is under way, its outcome is uncertain: it is never run again by itself. Secrets you generate last until \
Kelpy stops.",
        access = name(&ACCESS, modes.access),
        access_choices = choices(&ACCESS),
        approval = name(&APPROVAL, modes.approval),
        approval_choices = choices(&APPROVAL),
        account = account.label(),
    )
}

/// The name of `value` among `choices`.
fn name<T: PartialEq>(choices: &[Choice<T>], value: T) -> &'static str {
    choices
        .iter()
        .find(|choice| choice.value == value)
        .map_or("unknown", |choice| choice.name)
}

/// Each of `choices` with what it means, `None: you only answer…; Read-only: …`.
fn choices<T>(choices: &[Choice<T>]) -> String {
    let described: Vec<String> = choices
        .iter()
        .map(|choice| {
            if choice.available {
                format!("{}: {}", choice.name, choice.means)
            } else {
                format!("{}: {}; not available yet", choice.name, choice.means)
            }
        })
        .collect();
    format!("{}.", described.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// A file of the Web UI's sources.
    fn web(file: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../web/src")
            .join(file);
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn the_controls_named_are_the_web_uis() {
        let modes = web("lib/modes.ts");
        for (name, available) in ACCESS
            .iter()
            .map(|choice| (choice.name, choice.available))
            .chain(
                APPROVAL
                    .iter()
                    .map(|choice| (choice.name, choice.available)),
            )
        {
            let at = modes
                .find(&format!("name: '{name}'"))
                .unwrap_or_else(|| panic!("{name} is not in modes.ts"));
            // The option's own entry, up to the next one.
            let entry = modes[at..].split("},").next().unwrap();
            assert_eq!(!entry.contains("disabled: true"), available, "{name}");
        }
        let composer = web("lib/Composer.svelte");
        for label in [
            "Explanations",
            "label=\"System access\"",
            "label=\"Approval mode\"",
            "title=\"Stop\"",
        ] {
            assert!(composer.contains(label), "{label}");
        }
        let card = web("lib/ActionCard.svelte");
        for label in [
            ">Approve<",
            ">Deny<",
            ">Switch to Plan<",
            ">Switch to Auto<",
            ">Stop<",
        ] {
            assert!(card.contains(label), "{label}");
        }
        let log = web("lib/CommandLog.svelte");
        for label in [">Restore<", ">Report<", ">Export<"] {
            assert!(log.contains(label), "{label}");
        }
        let differences = web("lib/Differences.svelte");
        for label in [">Accept<", ">Put back<", "Changed outside Kelpy"] {
            assert!(differences.contains(label), "{label}");
        }
        let app = web("App.svelte");
        for label in [
            "aria-label=\"Command log\"",
            "title=\"AI provider\"",
            "'Change provider'",
        ] {
            assert!(app.contains(label), "{label}");
        }
    }

    #[test]
    fn the_values_of_the_conversation_are_told() {
        let account = Account::current();
        let told = interface(
            Modes {
                explanations: true,
                access: Access::None,
                approval: Approval::Plan,
            },
            &account,
        );
        assert!(told.contains("Explanations, a toggle, now on"));
        assert!(told.contains("System access, now None."));
        assert!(told.contains(
            "Approval mode, now Plan. It cannot be changed while System access is None."
        ));
        assert!(told.contains("Full: you may also change the system"));
        assert!(!told.contains("later") && !told.contains("not available"));

        let told = interface(Modes::default(), &account);
        assert!(told.contains(&format!(
            "the account that started Kelpy, {}.",
            account.label()
        )));
        assert!(
            told.contains("now off") && told.contains("now Read-only") && told.contains("now Ask")
        );
        assert!(!told.contains("cannot be changed while"));
    }
}
