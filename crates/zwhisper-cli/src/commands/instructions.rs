//! `zwhisper instructions` — concise operational guidance.

use crate::cli::InstructionsArgs;

pub(crate) fn run(args: &InstructionsArgs) -> color_eyre::Result<()> {
    if args.agent {
        print_agent_instructions();
    } else {
        print_short_instructions();
    }
    Ok(())
}

fn print_short_instructions() {
    println!(
        r#"# zwhisper CLI

Use `zwhisper status`, `zwhisper toggle`, and `zwhisper profile set <name>`
for daily operation. Use `zwhisper instructions --agent` for a concise
machine-oriented reference.
"#
    );
}

fn print_agent_instructions() {
    println!(
        r#"# zwhisper Agent Instructions

zwhisper is operated through two binaries:

- `zwhisperd`: user-session daemon for recording and transcription.
- `zwhisper`: CLI for control, diagnostics, profile selection, and one-shot transcription.

Configuration is file-based:

- User profiles: `${{XDG_CONFIG_HOME:-$HOME/.config}}/zwhisper/profiles/<name>.toml`.
- Optional secrets: `${{XDG_CONFIG_HOME:-$HOME/.config}}/zwhisper/secrets.toml`.
- Local models: `${{ZWHISPER_MODELS_DIR:-$XDG_DATA_HOME/zwhisper/models}}/ggml-<model>.bin`.

Core commands:

- `zwhisper status --json`: print daemon state, active profile, and duration.
- `zwhisper status --waybar`: print Waybar-compatible JSON.
- `zwhisper toggle`: start or stop recording with the active profile.
- `zwhisper record --profile <name>`: start a foreground recording with an explicit profile.
- `zwhisper profile list`: list available profiles.
- `zwhisper profile show <name>`: print the resolved profile TOML.
- `zwhisper profile clone <src> <dst>`: create a user-editable profile copy.
- `zwhisper profile set <name>`: persist the active profile used by `toggle`.
- `zwhisper transcribe <file> --profile <name>`: transcribe an existing audio file.
- `zwhisper backend health --backend deepgram`: validate cloud backend credentials.

Manual desktop integration:

- Bind your compositor or panel to run `zwhisper toggle`.
- For Waybar, use a custom module that executes `zwhisper status --waybar`.

Do not edit shipped profiles directly. Clone them first, then edit the user copy.
"#
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_instructions_mention_machine_interfaces() {
        let text = include_str!("instructions.rs");
        assert!(text.contains("zwhisper status --json"));
        assert!(text.contains("zwhisper status --waybar"));
        assert!(text.contains("zwhisper profile set <name>"));
    }
}
