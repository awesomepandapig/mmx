//! Publishes order books, tickers and candles from the tape.
//!
//! Each feed keeps its own thread, Aeron subscription, NATS queue and checkpoint, so running them
//! in one process costs the same as running them apart.

mod candles;
mod level2;
mod ticker;

use std::{panic, process, thread};

use tape::feed::{self, Processor};

use tracing::error;

fn spawn<P: Processor + 'static>() {
    let started = thread::Builder::new()
        .name(P::NAME.to_string())
        .spawn(feed::run::<P>);
    if let Err(e) = started {
        error!("Failed to start {}: {}", P::NAME, e);
        process::exit(1);
    }
}

fn main() {
    // One feed dying must restart the service, not leave the others running without it.
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(1);
    }));

    spawn::<level2::Level2>();
    spawn::<ticker::Tickers>();
    spawn::<candles::Candles>();

    loop {
        thread::park();
    }
}
