//! Conservative decisions for read-only descriptor screening, not real I/O.
pub(super) fn decision(
    api_ok: bool,
    access_status: bool,
    granted: u32,
    requested: u32,
) -> Option<bool> {
    if !api_ok || (access_status && granted & requested != requested) {
        None
    } else {
        Some(access_status)
    }
}
pub(super) fn passes_screen(
    device: Option<bool>,
    rights: &[Option<bool>],
    controls_ok: bool,
) -> bool {
    controls_ok
        && device == Some(true)
        && !rights.is_empty()
        && rights.iter().all(|right| *right == Some(false))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn access_screen_uses_access_status_not_api_success() {
        assert_eq!(decision(true, false, 0, 2), Some(false));
        assert_eq!(decision(true, true, 2, 2), Some(true));
    }
    #[test]
    fn access_screen_keeps_query_errors_unknown() {
        assert_eq!(decision(false, false, 0, 2), None);
        assert_eq!(decision(true, true, 1, 2), None);
    }
    #[test]
    fn access_screen_never_accepts_missing_paths_or_partial_write_grants() {
        assert!(!passes_screen(Some(true), &[], true));
        assert!(!passes_screen(Some(true), &[None], true));
        assert!(!passes_screen(Some(true), &[Some(false), Some(true)], true));
        assert!(!passes_screen(Some(true), &[Some(false)], false));
        assert!(passes_screen(Some(true), &[Some(false), Some(false)], true));
    }
}
