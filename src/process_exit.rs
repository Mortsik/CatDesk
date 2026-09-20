pub(crate) const EXIT_CODE_TIMEOUT: i32 = 124;
pub(crate) const EXIT_CODE_INTERNAL_ERROR: i32 = 125;
pub(crate) const EXIT_CODE_CANCELLED: i32 = 130;

#[cfg(unix)]
pub(crate) fn exit_code_for_status(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128_i32.saturating_add(signal)))
        .unwrap_or(EXIT_CODE_INTERNAL_ERROR)
}

#[cfg(not(unix))]
pub(crate) fn exit_code_for_status(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(EXIT_CODE_INTERNAL_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_signal_uses_conventional_shell_exit_code() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(9);

        assert_eq!(exit_code_for_status(&status), 137);
    }

    #[test]
    fn terminal_reason_codes_are_never_null() {
        assert_eq!(EXIT_CODE_TIMEOUT, 124);
        assert_eq!(EXIT_CODE_INTERNAL_ERROR, 125);
        assert_eq!(EXIT_CODE_CANCELLED, 130);
    }
}
