//! Operator-only command policy.
//!
//! A catdesk session killed the whole WSL VM twice in one evening by running
//! `wsl.exe --shutdown` (poepricer SDD NAT tests, 2026-09-20) — every Claude
//! session, agent and the GLM semaphore proxy died with it. Bouncing the VM
//! is an operator decision, never an agent decision, so the gate is enforced
//! before spawn rather than left to AGENTS.md instructions alone.

/// Error returned to the agent when a command would kill the whole WSL VM.
/// Written for the agent to read: it must be obvious what was blocked, why,
/// and what to do instead (report BLOCKED, let the operator bounce the VM).
pub const VM_BOUNCE_DENY_MESSAGE: &str = "DENIED (operator-only): this command \
kills/bounces the whole WSL VM (`wsl --shutdown` / `wsl --terminate` / `wsl -t`): \
every session, agent and the GLM semaphore proxy die with it. Never run it, in \
any form (direct, powershell, scripts, systemd-run). Report BLOCKED to the \
operator with the alternatives you considered — the operator bounces the VM at \
a chosen moment. See ~/.catdesk/AGENTS.md, section \"Forbidden commands\".";

/// Rejects commands that would terminate the whole WSL VM (or a distro in it,
/// which on a single-distro setup is the same outage).
///
/// Scans the whole command string, so wrapped forms are caught too:
/// `powershell.exe -Command "wsl --shutdown"`, `systemd-run ... bash -lc
/// "...wsl.exe --shutdown"`. Limitation (accepted): fully encoded payloads
/// (base64 piped into bash) are not decoded; the deny message plus the
/// AGENTS.md rule are the backstop for that.
pub fn check_vm_bounce(command: &str) -> Result<(), String> {
    let lowered = command.to_ascii_lowercase();
    for after_token in wsl_token_ends(&lowered) {
        let segment = &lowered[after_token..shell_segment_end(&lowered, after_token)];
        if segment_requests_vm_bounce(segment) {
            return Err(VM_BOUNCE_DENY_MESSAGE.to_string());
        }
    }
    Ok(())
}

fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Byte offsets just past every word-bounded `wsl` / `wsl.exe` token.
///
/// `.wslconfig` is not a token (the token must end at a non-word char), but
/// `/mnt/c/Windows/System32/wsl.exe` is — the `/` and `.` around it are not
/// word chars.
fn wsl_token_ends(lowered: &str) -> Vec<usize> {
    let bytes = lowered.as_bytes();
    let mut ends = Vec::new();
    let mut search_from = 0;
    while let Some(found) = lowered[search_from..].find("wsl") {
        let start = search_from + found;
        search_from = start + 1;
        let word_start = start == 0 || !is_word_char(bytes[start - 1]);
        if !word_start {
            continue;
        }
        let mut end = start + "wsl".len();
        if lowered[end..].starts_with(".exe") {
            end += ".exe".len();
        }
        let word_end = end == bytes.len() || !is_word_char(bytes[end]);
        if !word_end {
            continue;
        }
        ends.push(end);
    }
    ends
}

/// End of the shell command segment starting at `from`: the next `|`, `;`,
/// `&` or newline. A flag before the separator belongs to this command, so
/// `wsl --shutdown; echo done` still hits.
fn shell_segment_end(lowered: &str, from: usize) -> usize {
    lowered[from..]
        .find(|c: char| matches!(c, '|' | ';' | '&' | '\n'))
        .map(|offset| from + offset)
        .unwrap_or(lowered.len())
}

fn segment_requests_vm_bounce(segment: &str) -> bool {
    segment.split_whitespace().any(|token| {
        let token = token.trim_matches(|c| c == '"' || c == '\'' || c == '`');
        matches!(token, "--shutdown" | "--terminate" | "-t")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(command: &str) {
        let result = check_vm_bounce(command);
        assert!(
            result.is_err(),
            "expected DENY for: {command:?}, got Ok"
        );
        let message = result.unwrap_err();
        assert!(
            message.contains("DENIED"),
            "deny message must identify itself as DENIED, got: {message}"
        );
    }

    fn allowed(command: &str) {
        assert!(
            check_vm_bounce(command).is_ok(),
            "expected ALLOW for: {command:?}"
        );
    }

    #[test]
    fn denies_plain_wsl_shutdown() {
        denied("wsl.exe --shutdown");
        denied("wsl --shutdown");
    }

    #[test]
    fn denies_absolute_path_wsl_shutdown_from_incident() {
        // Literal string from the 2026-09-20 incident journal.
        denied("/mnt/c/Windows/System32/wsl.exe --shutdown");
    }

    #[test]
    fn denies_case_variants() {
        denied("WSL.EXE --SHUTDOWN");
        denied("Wsl --Shutdown");
    }

    #[test]
    fn denies_terminate_and_short_flag() {
        denied("wsl --terminate Ubuntu-22.04");
        denied("wsl -t Ubuntu-22.04");
        denied("wsl.exe -t Ubuntu");
    }

    #[test]
    fn denies_powershell_wrapped() {
        denied("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"wsl --shutdown\"");
    }

    #[test]
    fn denies_systemd_run_wrapped_from_incident() {
        // Literal shape of the transient unit from the incident journal.
        denied(
            "systemd-run --user --unit poepricer-do-nat-shutdown-220156 \
             /bin/bash -lc \"/mnt/c/Windows/System32/wsl.exe --shutdown\"",
        );
    }

    #[test]
    fn denies_bash_wrapped() {
        denied("bash -lc 'wsl.exe -t Ubuntu-22.04'");
    }

    #[test]
    fn denies_after_shell_separator() {
        denied("echo hi && wsl --shutdown");
        denied("true; wsl --terminate Ubuntu");
    }

    #[test]
    fn denies_flag_before_separator() {
        denied("wsl --shutdown; echo done");
    }

    #[test]
    fn allows_read_only_wsl_commands() {
        allowed("wsl.exe --list --verbose");
        allowed("wsl -l -v");
        allowed("wsl --status");
    }

    #[test]
    fn allows_shutdown_without_wsl_token() {
        allowed("sudo shutdown now");
        allowed("systemctl restart catdesk-ssh-agent");
    }

    #[test]
    fn allows_wsl_token_in_unrelated_position() {
        allowed("grep wsl README.md");
        // `/wsl` is a word-boundary token, but the flag that follows belongs
        // to curl and is not an exact `--shutdown` token.
        allowed("curl https://example.com/wsl --verbose");
    }

    #[test]
    fn allows_lookalike_flags() {
        // `--shutdown-catcher` is not the `--shutdown` flag.
        allowed("echo 'wsl --shutdown-catcher'");
    }

    #[test]
    fn allows_ordinary_commands() {
        allowed("cargo build --release -j1");
        allowed("ls -la");
        allowed("");
    }

    #[test]
    fn allows_wslconfig_edits() {
        // Editing the config is fine; only bouncing the VM is not.
        allowed("echo '[wsl2]' > /mnt/c/Users/Morts/.wslconfig");
    }

    #[test]
    fn allows_wslconfig_word_inside_larger_token() {
        // `.wslconfig` must not register as a `wsl` token.
        allowed("cat /mnt/c/Users/Morts/.wslconfig --shutdown-something");
    }
}
