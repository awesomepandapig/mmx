//! Reads and edits `markets.yaml`, applying the gateway's rules before anything is written, so an
//! edit can't produce a file that gateways refuse.

use crate::decimal;

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const SYMBOL_LENGTH: usize = 6;
const DEFAULT_MIN_SIZE: i64 = 1;
const DEFAULT_MAX_SIZE: i64 = i64::MAX;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketsFile {
    pub markets: Vec<MarketEntry>,
}

/// Gateways refuse unknown fields, so this must only ever hold what they accept. `halted` is
/// runtime state on the gateway and doesn't belong here.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketEntry {
    pub symbol: String,
    pub tick_size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// A market's limits as mantissas, with the gateway's defaults filled in, so that two spellings of
/// the same market compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub tick_size: i64,
    pub min_size: i64,
    pub max_size: i64,
}

/// Checks a symbol's shape and returns its normalized form. Symbols end up in gateway paths and SQL
/// filters here, so they can't contain separators or quotes.
pub fn normalize_symbol(symbol: &str) -> Result<String, String> {
    let normalized = symbol.to_uppercase();
    if normalized.is_empty() || normalized.len() > SYMBOL_LENGTH {
        return Err(format!(
            "Symbol '{symbol}' is invalid. It must be between 1 and {SYMBOL_LENGTH} characters."
        ));
    }
    if !normalized.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "Symbol '{symbol}' is invalid. It must be alphanumeric."
        ));
    }
    Ok(normalized)
}

fn positive(name: &str, symbol: &str, text: &str) -> Result<i64, String> {
    match decimal::parse(text) {
        Ok(0) => Err(format!("{symbol}: {name} must be positive")),
        Ok(value) => Ok(value),
        Err(err) => Err(format!("{symbol}: {name}: {err}")),
    }
}

/// Parses limits as the gateway does. `min_size` and `max_size` fall back to its defaults.
pub fn limits(
    symbol: &str,
    tick: &str,
    min: Option<&str>,
    max: Option<&str>,
) -> Result<Limits, String> {
    let tick_size = positive("tick_size", symbol, tick)?;
    let bound = |field: &str, value: Option<&str>, default: i64| match value {
        Some(text) => positive(field, symbol, text),
        None => Ok(default),
    };
    let min_size = bound("min_size", min, DEFAULT_MIN_SIZE)?;
    let max_size = bound("max_size", max, DEFAULT_MAX_SIZE)?;
    if min_size > max_size {
        return Err(format!("{symbol}: min_size is above max_size"));
    }
    Ok(Limits {
        tick_size,
        min_size,
        max_size,
    })
}

impl MarketEntry {
    pub fn limits(&self) -> Result<Limits, String> {
        limits(
            &self.symbol,
            &self.tick_size,
            self.min_size.as_deref(),
            self.max_size.as_deref(),
        )
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

impl MarketsFile {
    /// Parses with the same rules as the gateway. Symbols come back normalized.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut file: MarketsFile = serde_yaml::from_str(text).map_err(|err| err.to_string())?;
        let mut seen = HashSet::new();
        for entry in &mut file.markets {
            entry.symbol = normalize_symbol(&entry.symbol)?;
            if !seen.insert(entry.symbol.clone()) {
                return Err(format!("{}: listed more than once", entry.symbol));
            }
            entry.limits()?;
        }
        Ok(file)
    }

    pub fn get(&self, symbol: &str) -> Option<&MarketEntry> {
        self.markets.iter().find(|entry| entry.symbol == symbol)
    }

    /// Replaces a market where it stands, or adds it at the end.
    pub fn upsert(&mut self, entry: MarketEntry) {
        match self
            .markets
            .iter_mut()
            .find(|existing| existing.symbol == entry.symbol)
        {
            Some(existing) => *existing = entry,
            None => self.markets.push(entry),
        }
    }

    pub fn remove(&mut self, symbol: &str) -> bool {
        let before = self.markets.len();
        self.markets.retain(|entry| entry.symbol != symbol);
        self.markets.len() != before
    }

    fn render(&self, header: &str) -> Result<String, String> {
        let body = serde_yaml::to_string(self).map_err(|err| err.to_string())?;
        Ok(format!("{header}{body}"))
    }
}

/// The comment block above `markets:`. It documents the format for whoever edits the file by hand,
/// so an edit keeps it. Comments between entries can't survive a rewrite.
fn header(text: &str) -> String {
    let lines = text
        .lines()
        .take_while(|line| !line.starts_with("markets:"));
    lines.map(|line| format!("{line}\n")).collect()
}

pub fn read(path: &Path) -> Result<(MarketsFile, String), String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let file = MarketsFile::parse(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok((file, text))
}

fn replace_file(target: &Path, text: &str) -> io::Result<()> {
    let directory = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("markets.yaml");
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));

    let write = || -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        // A gateway may run as another user, so the new file has to stay as readable as the old one.
        if let Ok(metadata) = fs::metadata(target) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        fs::File::open(directory)?.sync_all()
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Checks the new contents with the gateway's rules, then swaps them in with a rename, so a reader
/// sees the old file or the new one and never part of either.
pub fn write(path: &Path, file: &MarketsFile, previous: &str) -> Result<(), String> {
    let text = file.render(&header(previous))?;
    MarketsFile::parse(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    // Renaming over a symlink would replace the link instead of the file it points to.
    let target: PathBuf = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    replace_file(&target, &text).map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATEWAY_FILE: &str = include_str!("../../api/rest/markets.yaml");

    fn entry(symbol: &str, tick: &str) -> MarketEntry {
        MarketEntry {
            symbol: symbol.to_string(),
            tick_size: tick.to_string(),
            min_size: None,
            max_size: None,
            enabled: None,
        }
    }

    fn temp_file(text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mmx-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("markets.yaml");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn reads_the_file_the_gateway_ships() {
        let file = MarketsFile::parse(GATEWAY_FILE).unwrap();
        assert_eq!(
            file.get("ITEM").unwrap().limits().unwrap().tick_size,
            1_000_000
        );
    }

    #[test]
    fn refuses_what_the_gateway_refuses() {
        let entry = |body: &str| format!("markets:\n  - {body}\n");
        for text in [
            entry(r#"{symbol: "A-B", tick_size: "1"}"#),
            entry(r#"{symbol: TOOLONG1, tick_size: "1"}"#),
            entry(r#"{symbol: A, tick_size: "0"}"#),
            entry(r#"{symbol: A, tick_size: "0.000000001"}"#),
            entry(r#"{symbol: A}"#),
            entry(r#"{symbol: A, tick_size: "1", surprise: true}"#),
            entry(r#"{symbol: A, tick_size: "1", min_size: "2", max_size: "1"}"#),
            "markets:\n  - {symbol: A, tick_size: \"1\"}\n  - {symbol: a, tick_size: \"1\"}\n"
                .to_string(),
            "not yaml: [".to_string(),
        ] {
            assert!(
                MarketsFile::parse(&text).is_err(),
                "{text} should be refused"
            );
        }
    }

    #[test]
    fn an_edit_keeps_the_header_and_leaves_no_temporary_file() {
        let path = temp_file(GATEWAY_FILE);
        let (mut file, previous) = read(&path).unwrap();
        file.upsert(entry("GEM", "0.5"));
        write(&path, &file, &previous).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Markets this gateway accepts"), "{text}");
        assert_eq!(read(&path).unwrap().0, file);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn symbols_that_look_like_other_yaml_types_stay_strings() {
        let mut file = MarketsFile::default();
        for symbol in ["NULL", "TRUE", "NO", "1E5", "0X10", "123"] {
            file.upsert(entry(symbol, "0.10"));
        }
        let text = file.render("").unwrap();
        assert_eq!(MarketsFile::parse(&text).unwrap(), file, "{text}");
    }

    #[test]
    fn a_bad_edit_leaves_the_file_alone() {
        let path = temp_file(GATEWAY_FILE);
        let (mut file, previous) = read(&path).unwrap();
        file.upsert(entry("GEM", "0"));
        assert!(
            write(&path, &file, &previous)
                .unwrap_err()
                .contains("tick_size")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), GATEWAY_FILE);
    }

    #[cfg(unix)]
    #[test]
    fn an_edit_writes_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let real = temp_file(GATEWAY_FILE);
        fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
        let link = real.with_file_name("link.yaml");
        symlink(&real, &link).unwrap();

        let (mut file, previous) = read(&link).unwrap();
        file.remove("ITEM");
        write(&link, &file, &previous).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(read(&real).unwrap().0.markets.is_empty());
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
