//! Bounded portable worker mappings, separate from allocation/RSS observation.
use super::sqlite_body_scenario::{self, Mode};

#[path = "stack.rs"]
mod stack;

pub fn from_argument(argument: &str) -> Option<Mode> {
    Mode::from_argument(argument.strip_suffix("-stack")?)
}

pub fn run(mode: Mode) {
    if !cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) || cfg!(debug_assertions)
    {
        panic!("requires the pinned release x86-64 musl artifact");
    }
    std::thread::Builder::new()
        .name(format!("{}-stack", mode.scenario()))
        .stack_size(240 * 1024)
        .spawn(move || {
            let before = stack::bounded_stack_mapping(
                &format!("{}_stack_before_mapping_bytes", mode.scenario()),
                256 * 1024,
            );
            let mut observed = 0usize;
            sqlite_body_scenario::run(mode, || {
                observed += 1;
                assert!(observed <= mode.phases().len());
            });
            assert_eq!(observed, mode.phases().len());
            let after = stack::bounded_stack_mapping(
                &format!("{}_stack_after_mapping_bytes", mode.scenario()),
                256 * 1024,
            );
            assert_eq!(after, before);
        })
        .unwrap()
        .join()
        .unwrap();
    println!("{}-stack-v1: passed", mode.scenario());
}
