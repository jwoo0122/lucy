pub(crate) const NORMAL_OUTPUT_RESERVE_TOKENS: usize = 16_384;
pub(crate) const COMPACTION_OUTPUT_RESERVE_TOKENS: usize = 4_096;
const MIN_SAFETY_MARGIN_TOKENS: usize = 2_048;

pub(crate) fn usable_context(context_window: usize, output_reserve: usize) -> usize {
    let safety_margin = MIN_SAFETY_MARGIN_TOKENS.max(context_window / 50);
    context_window
        .saturating_sub(output_reserve)
        .saturating_sub(safety_margin)
}

pub(crate) fn should_compact(context_tokens: usize, context_window: usize) -> bool {
    context_window > 0
        && context_tokens >= usable_context(context_window, NORMAL_OUTPUT_RESERVE_TOKENS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_output_and_estimation_headroom() {
        assert_eq!(usable_context(128_000, 16_384), 109_056);
        assert_eq!(
            usable_context(128_000, COMPACTION_OUTPUT_RESERVE_TOKENS),
            121_344
        );
        assert!(!should_compact(109_055, 128_000));
        assert!(should_compact(109_056, 128_000));
    }

    #[test]
    fn small_windows_saturate_without_underflow() {
        assert_eq!(usable_context(4_000, 4_096), 0);
        assert_eq!(usable_context(0, 4_096), 0);
        assert!(should_compact(1, 4_000));
        assert!(!should_compact(1, 0));
    }
}
