/// Exact line printed only after a current-boot capture has been validated.
/// A normal boot uses this generic evidence marker; the QEMU system oracle
/// requires the stronger attribution marker below.
pub const CAPTURE_MARKER: &str = "TD-PROFILER-CAPTURE-OK";

/// Exact line printed in an autotest boot only after a persisted
/// `lines.jsonl` row attributes samples to the profiler's deterministic CPU
/// workload by function symbol, with source-line resolution unavailable.
/// The host QEMU oracle includes this same source file through td-recipe.
pub const ATTRIBUTION_MARKER: &str = "TD-PROFILER-ATTRIBUTION-OK";

/// Stable fragment retained inside rustc's mangled symbol name for the
/// no-inline attribution workload.
pub const ATTRIBUTION_FUNCTION_FRAGMENT: &str = "td_profiler_attribution_workload";

/// Longest bounded wait accepted by `td-profiler evidence`. The system image
/// imports this value only for the exact-token attribution path; ordinary
/// evidence retains a shorter image-selected wait.
pub const MAX_EVIDENCE_WAIT_SECS: u16 = 900;
