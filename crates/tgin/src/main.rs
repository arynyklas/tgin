//! The `tgin` executable; see [`tgin::cli`].

use std::process::ExitCode;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    ExitCode::from(tgin::cli::main(std::env::args_os()))
}
