//! Aeron client setup shared by every service.
//!
//! `rusteron` handles are neither `Send` nor `Sync`, so create each client on the thread that
//! uses it. Each client keeps its own conductor running until the `Aeron` handle is dropped.

use std::env;
use std::time::Duration;

pub use rusteron_client::{
    Aeron, AeronCError, AeronClaim, AeronExclusivePublication, AeronOfferError, AeronSubscription,
};
use rusteron_client::{AeronContext, Handlers, cformat};

const RESOURCE_TIMEOUT: Duration = Duration::from_secs(15);

pub const ORDERS_CHANNEL: &str = "aeron:udp?endpoint=localhost:40123";
pub const ORDERS_STREAM: i32 = 1001;

// Execution reports use multi-destination-cast: subscribers register with the engine's control
// address, so they can run on any node without network multicast.
pub const EXECUTIONS_PUB_CHANNEL: &str = "aeron:udp?control=localhost:40456|control-mode=dynamic";
pub const EXECUTIONS_SUB_CHANNEL: &str = "aeron:udp?endpoint=localhost:0|control=localhost:40456";
pub const EXECUTIONS_STREAM: i32 = 1002;

pub fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

pub fn stream_id_or(name: &str, default: i32) -> i32 {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|err| panic!("{name} must be an i32, got {value:?}: {err}")),
        Err(_) => default,
    }
}

/// Connects to the media driver and subscribes to execution reports, using `SUB_CHANNEL` and
/// `SUB_STREAM_ID` when set.
pub fn subscribe_to_executions(
    on_error: fn(i32, &str),
) -> Result<(Aeron, AeronSubscription), AeronCError> {
    let aeron = connect(on_error)?;
    let channel = env_or("SUB_CHANNEL", EXECUTIONS_SUB_CHANNEL);
    let stream_id = stream_id_or("SUB_STREAM_ID", EXECUTIONS_STREAM);
    let subscription = subscription(&aeron, &channel, stream_id)?;
    Ok((aeron, subscription))
}

pub fn aeron_dir() -> String {
    env::var("AERON_DIR").unwrap_or_else(|_| {
        #[cfg(target_os = "macos")]
        return "/Volumes/DevShm/aeron".to_string();

        #[cfg(target_os = "linux")]
        return "/dev/shm/aeron".to_string();

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        panic!(
            "AERON_DIR not set and no default available for this OS ({})",
            env::consts::OS
        );
    })
}

/// Connects to the media driver. `on_error` runs on the client's conductor thread.
pub fn connect(on_error: fn(i32, &str)) -> Result<Aeron, AeronCError> {
    let dir = aeron_dir();
    let context = AeronContext::new()?;
    context.set_dir(&cformat!("{dir}"))?;
    context.set_error_handler(Some(move |code: i32, message: &str| {
        on_error(code, message)
    }))?;
    context.set_pre_touch_mapped_memory(true)?;

    let aeron = Aeron::new(&context)?;
    aeron.start()?;
    Ok(aeron)
}

pub fn exclusive_publication(
    aeron: &Aeron,
    channel: &str,
    stream_id: i32,
) -> Result<AeronExclusivePublication, AeronCError> {
    aeron
        .async_add_exclusive_publication(&cformat!("{channel}"), stream_id)?
        .poll_blocking(RESOURCE_TIMEOUT)
}

pub fn subscription(
    aeron: &Aeron,
    channel: &str,
    stream_id: i32,
) -> Result<AeronSubscription, AeronCError> {
    aeron
        .async_add_subscription(
            &cformat!("{channel}"),
            stream_id,
            Handlers::NONE,
            Handlers::NONE,
        )?
        .poll_blocking(RESOURCE_TIMEOUT)
}
