use crossterm::event::{KeyCode, KeyModifiers};
use std::io::{Write as _, stdout};

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_copy(text: &str) -> bool {
    let mut child = match std::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(text.as_bytes()).is_err() {
        drop(stdin);
        let _ = child.wait();
        return false;
    }

    drop(stdin);

    match child.wait() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn clipboard_copy(text: &str) -> bool {
    let mut child = match std::process::Command::new("clip.exe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.wait();
        return false;
    };

    if stdin.write_all(text.as_bytes()).is_err() {
        drop(stdin);
        let _ = child.wait();
        return false;
    }

    drop(stdin);

    match child.wait() {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub(crate) fn clipboard_copy(text: &str) -> bool {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")
        .and_then(|_| out.flush())
        .is_ok()
}

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_paste() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/pbpaste")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .filter(|text| !text.is_empty())
}

#[cfg(target_os = "windows")]
pub(crate) fn clipboard_paste() -> Option<String> {
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Get-Clipboard -Raw"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .filter(|text| !text.is_empty())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub(crate) fn clipboard_paste() -> Option<String> {
    const CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[
        ("wl-paste", &["-n"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];

    for (program, args) in CLIPBOARD_COMMANDS {
        let output = match std::process::Command::new(program)
            .args(*args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => continue,
        };

        if let Ok(text) = String::from_utf8(output.stdout) {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    None
}

pub(crate) fn key_is_clipboard_paste(key: &crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Insert) && key.modifiers.contains(KeyModifiers::SHIFT)
        || matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'v'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub(crate) fn text_input_key_is_cancel(code: KeyCode) -> bool {
    matches!(code, KeyCode::Esc)
}
