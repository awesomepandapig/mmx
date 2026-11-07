//! Operator commands. They travel through the cluster log like any other message, so every node
//! acts on them at the same point, and a restarted node replays them. Gateways never send these.

use std::env;
use std::thread::sleep;
use std::time::{Duration, Instant};

use cluster::{ClusterClient, OfferResult};

const INGRESS_STREAM: i32 = 101;
const EGRESS_STREAM: i32 = 102;
const OFFER_DEADLINE: Duration = Duration::from_secs(10);

const REBUILD_TEMPLATE: u16 = 110;
const CANCEL_ALL_TEMPLATE: u16 = 111;
const TAPE_SWEPT_TEMPLATE: u16 = 112;

const HEADER_LENGTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminCommand {
    /// Rebuilds the book from the tape. `expected` is the position the engine reported when it
    /// held, so a stale or mistyped command changes nothing.
    Rebuild { expected: i64, position: i64 },
    /// Cancels every resting order, which sends each one's owner a normal cancel report.
    CancelAll,
    /// Everything before `position` has been swept from the tape to its long-term home.
    TapeSwept { position: i64 },
}

pub const fn block_length(template: u16) -> Option<u16> {
    match template {
        REBUILD_TEMPLATE => Some(16),
        CANCEL_ALL_TEMPLATE => Some(0),
        TAPE_SWEPT_TEMPLATE => Some(8),
        _ => None,
    }
}

pub const fn is_admin(message: &[u8]) -> bool {
    if message.len() < HEADER_LENGTH {
        return false;
    }
    let template = u16::from_le_bytes([message[2], message[3]]);
    template >= REBUILD_TEMPLATE && template <= TAPE_SWEPT_TEMPLATE
}

impl AdminCommand {
    pub fn encode(self) -> Vec<u8> {
        let (template, body): (u16, Vec<i64>) = match self {
            Self::Rebuild { expected, position } => (REBUILD_TEMPLATE, vec![expected, position]),
            Self::CancelAll => (CANCEL_ALL_TEMPLATE, vec![]),
            Self::TapeSwept { position } => (TAPE_SWEPT_TEMPLATE, vec![position]),
        };
        let mut message = vec![0; HEADER_LENGTH];
        message[0..2].copy_from_slice(&((body.len() * 8) as u16).to_le_bytes());
        message[2..4].copy_from_slice(&template.to_le_bytes());
        for value in body {
            message.extend_from_slice(&value.to_le_bytes());
        }
        message
    }

    pub fn decode(message: &[u8]) -> Option<Self> {
        let value = |index: usize| {
            let at = HEADER_LENGTH + index * 8;
            Some(i64::from_le_bytes(
                message.get(at..at + 8)?.try_into().ok()?,
            ))
        };
        match u16::from_le_bytes([*message.get(2)?, *message.get(3)?]) {
            REBUILD_TEMPLATE => Some(Self::Rebuild {
                expected: value(0)?,
                position: value(1)?,
            }),
            CANCEL_ALL_TEMPLATE => Some(Self::CancelAll),
            TAPE_SWEPT_TEMPLATE => Some(Self::TapeSwept {
                position: value(0)?,
            }),
            _ => None,
        }
    }
}

fn parse(args: &[String]) -> Result<AdminCommand, String> {
    let position = |value: &String| {
        value
            .parse::<i64>()
            .map_err(|_| format!("{value} isn't a position"))
    };
    match args {
        [command, expected, target] if command == "rebuild" => {
            Ok(AdminCommand::Rebuild { expected: position(expected)?, position: position(target)? })
        }
        [command] if command == "cancel-all" => Ok(AdminCommand::CancelAll),
        [command, swept] if command == "tape-swept" => Ok(AdminCommand::TapeSwept { position: position(swept)? }),
        _ => Err("usage: matching-engine admin rebuild EXPECTED POSITION | cancel-all | tape-swept POSITION".to_string()),
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is required"))
}

fn on_error(code: i32, message: &str) {
    eprintln!("Aeron error {code}: {message}");
}

fn send(command: AdminCommand) -> Result<(), String> {
    let endpoints: Vec<String> = required("CLUSTER_INGRESS_ENDPOINTS")?
        .split(',')
        .map(str::to_string)
        .collect();
    let response_channel = required("CLUSTER_RESPONSE_CHANNEL")?;
    let aeron = transport::connect(on_error).map_err(|error| format!("{error:?}"))?;
    let mut client = ClusterClient::connect(
        &aeron,
        &endpoints,
        INGRESS_STREAM,
        &response_channel,
        EGRESS_STREAM,
    )
    .map_err(|error| format!("{error:?}"))?;

    let message = command.encode();
    let deadline = Instant::now() + OFFER_DEADLINE;
    while client.offer(&message) != OfferResult::Sent {
        if Instant::now() > deadline {
            return Err("no leader accepted the command".to_string());
        }
        client.poll_egress();
        sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// Sends one command, and returns once it's offered. The engine's logs and the tape's recovery
/// markers show what it did.
pub fn run(args: &[String]) -> Result<(), String> {
    send(parse(args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn commands_survive_the_wire() {
        for command in [
            AdminCommand::Rebuild {
                expected: 192,
                position: 1_920,
            },
            AdminCommand::CancelAll,
            AdminCommand::TapeSwept { position: 384 },
        ] {
            let message = command.encode();
            assert!(is_admin(&message));
            assert_eq!(AdminCommand::decode(&message), Some(command));
        }
    }

    #[test]
    fn block_length_lets_a_batch_step_over_a_command() {
        let message = AdminCommand::Rebuild {
            expected: 1,
            position: 2,
        }
        .encode();
        let block_length = usize::from(u16::from_le_bytes([message[0], message[1]]));
        assert_eq!(HEADER_LENGTH + block_length, message.len());
    }

    #[test]
    fn orders_and_cancels_are_not_commands() {
        assert!(!is_admin(&[64, 0, 1, 0, 0, 0, 0, 0]));
        assert!(!is_admin(&[63, 0, 2, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn a_truncated_command_decodes_to_nothing() {
        let message = AdminCommand::Rebuild {
            expected: 1,
            position: 2,
        }
        .encode();
        assert_eq!(AdminCommand::decode(&message[..message.len() - 1]), None);
    }

    #[test]
    fn the_command_line_parses_each_command() {
        assert_eq!(
            parse(&args(&["rebuild", "192", "1920"])),
            Ok(AdminCommand::Rebuild {
                expected: 192,
                position: 1_920
            })
        );
        assert_eq!(parse(&args(&["cancel-all"])), Ok(AdminCommand::CancelAll));
        assert_eq!(
            parse(&args(&["tape-swept", "384"])),
            Ok(AdminCommand::TapeSwept { position: 384 })
        );
    }

    #[test]
    fn the_command_line_rejects_anything_else() {
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["rebuild", "192"])).is_err());
        assert!(parse(&args(&["rebuild", "192", "soon"])).is_err());
        assert!(parse(&args(&["reopen"])).is_err());
    }
}
