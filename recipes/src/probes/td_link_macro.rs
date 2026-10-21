//! td-link-test's proc macro: a dylib td-link links and rustc then loads.
//! Its expansion catches an unwind, so the unwinder linked into it works
//! inside rustc's process.
#![forbid(unsafe_code)]

use proc_macro::TokenStream;

#[proc_macro]
pub fn answer(_: TokenStream) -> TokenStream {
    // resume_unwind raises through the unwinder without the panic hook.
    let caught =
        std::panic::catch_unwind(|| std::panic::resume_unwind(Box::new("td-link-test"))).is_err();
    let value = if caught { "42" } else { "0" };
    value.parse().unwrap_or_default()
}
