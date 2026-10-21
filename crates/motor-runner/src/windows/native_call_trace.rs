//! Pure x64 ABI checks shared by the diagnostic native-call collector and tests.

pub(super) fn matches_return(
    entry_stack: u64,
    return_address: u64,
    stopped_stack: u64,
    stopped_address: u64,
) -> bool {
    return_address == stopped_address && entry_stack.checked_add(8) == Some(stopped_stack)
}

pub(super) fn ntstatus(rax: u64) -> (u32, bool) {
    let code = rax as u32;
    (code, (code as i32) < 0)
}
pub(super) fn defer_breakpoint(active_step: Option<u32>, event_thread: u32) -> bool {
    active_step.is_some_and(|owner| owner != event_thread)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_call_trace_matches_the_actual_x64_return_stack() {
        assert!(matches_return(0x1000, 0x2000, 0x1008, 0x2000));
        assert!(!matches_return(0x1000, 0x2000, 0x1000, 0x2000));
        assert!(!matches_return(0x1000, 0x2000, 0x1008, 0x3000));
    }
    #[test]
    fn native_call_trace_rejects_wrapped_stack_values() {
        assert!(!matches_return(u64::MAX - 7, 1, 0, 1));
    }
    #[test]
    fn native_call_trace_preserves_denial_and_success_statuses() {
        assert_eq!(ntstatus(0xffff_ffff_c000_0022), (0xc000_0022, true));
        assert_eq!(ntstatus(0x0000_0000_c000_0022), (0xc000_0022, true));
        assert_eq!(ntstatus(0), (0, false));
        assert_eq!(ntstatus(0x103), (0x103, false));
    }
    #[test]
    fn native_call_trace_defers_other_threads_queued_breakpoints() {
        assert!(defer_breakpoint(Some(10), 20));
        assert!(!defer_breakpoint(Some(10), 10));
        assert!(!defer_breakpoint(None, 20));
    }
}
