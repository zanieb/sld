#[cfg(feature = "mimalloc")]
#[global_allocator]
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    if let Err(error) = run() {
        libsld::error::report_error_and_exit(&error)
    }
}

/// The current sld version as written by build.rs.
const VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

fn run() -> libsld::error::Result {
    #[cfg(feature = "dhat")]
    let _profiler = dhat::Profiler::new_heap();

    if handle_command()? {
        return Ok(());
    }

    libsld::init_timing()?;

    let mut args = libsld::Args::new(std::env::args)?;
    args.set_version(VERSION);
    args.parse(std::env::args)?;

    if libsld::should_fork(&args) {
        // Safety: We haven't spawned any threads yet.
        unsafe { libsld::run_in_subprocess(args) };
    } else {
        // Run the linker in this process without forking.

        // Note, we need to setup tracing before worker, otherwise the threads won't contribute to
        // counters such as --time=cycles,instructions etc.
        libsld::setup_tracing(&args)?;

        libsld::run(args)
    }
}

fn handle_command() -> libsld::error::Result<bool> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(command) = args.next() else {
        return Ok(false);
    };
    if command != "log" {
        return Ok(false);
    }
    if args.next().is_some() {
        libsld::bail!("Usage: sld log");
    }

    let stdout = std::io::stdout();
    libsld::print_incremental_log(stdout.lock())?;
    Ok(true)
}
