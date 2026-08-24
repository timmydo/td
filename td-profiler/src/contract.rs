/// Exact line printed only after a current-boot capture has been validated.
/// A normal boot uses this generic evidence marker; the QEMU system oracle
/// requires the stronger attribution marker below.
pub const CAPTURE_MARKER: &str = "TD-PROFILER-CAPTURE-OK";

/// Exact line printed in an autotest boot only after a persisted source-line
/// report attributes samples to the profiler's deterministic CPU workload.
/// The host QEMU oracle includes this same source file through td-recipe.
pub const ATTRIBUTION_MARKER: &str = "TD-PROFILER-ATTRIBUTION-OK";

/// Stable fragment retained inside rustc's mangled symbol name for the
/// no-inline attribution workload.
pub const ATTRIBUTION_FUNCTION_FRAGMENT: &str = "td_profiler_attribution_workload";

/// Source identity emitted by the bounded reader for a DWARF-v4 file whose
/// directory index is zero. The remapped compilation directory is deliberately
/// not recovered from `.debug_info`, which the line-table-only reader does not
/// load.
pub const ATTRIBUTION_SOURCE_FILE: &str = "evidence.rs";
