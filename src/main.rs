//! zap: a native Arch Linux and AUR helper.
//!
//! The same executable is the command-line tool and, when started by run0 or
//! sudo as `zap __transaction <request>`, the root transaction worker.
mod alpm;
mod artifacts;
mod aur;
mod builder;
mod cli;
mod commands;
mod config;
mod errors;
mod help;
mod libalpm;
mod net;
mod news;
mod query;
mod sandbox;
mod signals;
mod srcinfo;
mod transaction;
#[cfg(test)]
mod testing;
mod ui;
mod util;

fn main() {
    unsafe {
        libc::setlocale(libc::LC_CTYPE, c"".as_ptr());
    }
    let status = run();
    ui::flush();
    if status != 0 {
        std::process::exit(status as i32);
    }
}

fn run() -> u8 {
    let args: Vec<String> = match std::env::args_os().skip(1).map(|a| a.into_string()).collect() {
        Ok(args) => args,
        Err(_) => return errors::report(&errors::Kind::Io.into()),
    };
    if args.first().map(String::as_str) == Some("__transaction") {
        if args.len() != 2 {
            return errors::report(&errors::Kind::InvalidWorkerArguments.into());
        }
        return match commands::worker(&args[1]) {
            Ok(()) => 0,
            Err(e) => errors::report(&e),
        };
    }
    ui::set_color_mode(cli::color_preference(&args).unwrap_or(ui::ColorMode::Auto));
    let result = cli::parse(&args).and_then(commands::run);
    match result {
        Ok(()) => 0,
        Err(e) => errors::report(&e),
    }
}
