//! Printing that survives a closed pipe. `println!` panics when `mmx watch | head` finishes.

use std::io::{self, Write};
use std::process;

/// Writes a line of results to stdout. With nobody left reading, there's nothing more to do.
pub fn line(text: &str) {
    let mut stdout = io::stdout().lock();
    if writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        process::exit(0);
    }
}

/// Writes a line about the run itself to stderr, so stdout stays machine-readable.
pub fn note(text: &str) {
    let _ = writeln!(io::stderr(), "{text}");
}
