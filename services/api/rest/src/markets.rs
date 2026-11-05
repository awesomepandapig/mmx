//! The markets this gateway accepts, from `markets.yaml`.
//!
//! The file is only read at startup and on an explicit reload. A bad file never replaces a good
//! one, so a failed reload leaves the gateway running on what it had.

use super::decimal;
use super::order::SymbolType;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use serde::Deserialize;

const SYMBOL_LENGTH: usize = 6;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketsFile {
    markets: Vec<MarketEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarketEntry {
    symbol: String,
    tick_size: String,
    min_size: Option<String>,
    max_size: Option<String>,
    enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug)]
pub struct Market {
    pub symbol: SymbolType,
    pub tick_size: i64,
    pub min_size: i64,
    pub max_size: i64,
}

#[derive(Debug, Default)]
pub struct Markets {
    enabled: HashMap<String, Market>,
}

/// Checks a symbol's shape and returns its normalized form. Symbols become NATS subjects and
/// QuestDB filters downstream, so they can't contain separators or quotes.
pub fn normalize_symbol(symbol: &str) -> Result<String, String> {
    let normalized = symbol.to_uppercase();
    if normalized.is_empty() || normalized.len() > SYMBOL_LENGTH {
        return Err(format!(
            "Product ID '{symbol}' is invalid. It must be between 1 and {SYMBOL_LENGTH} characters."
        ));
    }
    if !normalized.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("Product ID '{symbol}' is invalid. It must be alphanumeric."));
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

impl Markets {
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: MarketsFile = serde_yaml::from_str(text).map_err(|err| err.to_string())?;

        let mut seen = HashSet::new();
        let mut enabled = HashMap::new();
        for entry in file.markets {
            let name = normalize_symbol(&entry.symbol)?;
            if !seen.insert(name.clone()) {
                return Err(format!("{name}: listed more than once"));
            }

            let tick_size = positive("tick_size", &name, &entry.tick_size)?;
            let bound = |field: &str, value: &Option<String>, default: i64| match value {
                Some(text) => positive(field, &name, text),
                None => Ok(default),
            };
            let min_size = bound("min_size", &entry.min_size, 1)?;
            let max_size = bound("max_size", &entry.max_size, i64::MAX)?;
            if min_size > max_size {
                return Err(format!("{name}: min_size is above max_size"));
            }

            if entry.enabled.unwrap_or(true) {
                let mut symbol = [0u8; SYMBOL_LENGTH];
                symbol[..name.len()].copy_from_slice(name.as_bytes());
                enabled.insert(name, Market { symbol, tick_size, min_size, max_size });
            }
        }
        Ok(Self { enabled })
    }

    pub fn get(&self, symbol: &str) -> Option<&Market> {
        self.enabled.get(symbol)
    }

    pub fn symbols(&self) -> Vec<String> {
        let mut symbols: Vec<String> = self.enabled.keys().cloned().collect();
        symbols.sort_unstable();
        symbols
    }
}

/// The live market set, shared by every request.
pub struct MarketRegistry {
    path: PathBuf,
    current: RwLock<Arc<Markets>>,
}

impl MarketRegistry {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let markets = Self::read(&path)?;
        Ok(Self { path, current: RwLock::new(Arc::new(markets)) })
    }

    fn read(path: &Path) -> Result<Markets, String> {
        let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        Markets::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
    }

    pub fn current(&self) -> Arc<Markets> {
        self.current.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn reload(&self) -> Result<Arc<Markets>, String> {
        let markets = Arc::new(Self::read(&self.path)?);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = markets.clone();
        Ok(markets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
markets:
  - symbol: item
    tick_size: "0.05"
    min_size: "1"
    max_size: "10"
  - symbol: GEM
    tick_size: "1"
  - symbol: OLD
    tick_size: "1"
    enabled: false
"#;

    #[test]
    fn reads_markets_and_skips_disabled_ones() {
        let markets = Markets::parse(GOOD).unwrap();
        assert_eq!(markets.symbols(), ["GEM", "ITEM"]);
        let item = markets.get("ITEM").unwrap();
        assert_eq!(item.tick_size, 5_000_000);
        assert_eq!((item.min_size, item.max_size), (100_000_000, 1_000_000_000));
        assert_eq!(&item.symbol, b"ITEM\0\0");
        assert_eq!(markets.get("GEM").unwrap().max_size, i64::MAX);
    }

    #[test]
    fn refuses_bad_files() {
        let entry = |body: &str| format!("markets:\n  - {body}\n");
        for text in [
            entry(r#"{symbol: "A-B", tick_size: "1"}"#),
            entry(r#"{symbol: TOOLONG1, tick_size: "1"}"#),
            entry(r#"{symbol: A, tick_size: "0"}"#),
            entry(r#"{symbol: A, tick_size: "0.000000001"}"#),
            entry(r#"{symbol: A}"#),
            entry(r#"{symbol: A, tick_size: "1", surprise: true}"#),
            entry(r#"{symbol: A, tick_size: "1", min_size: "2", max_size: "1"}"#),
            "markets:\n  - {symbol: A, tick_size: \"1\"}\n  - {symbol: a, tick_size: \"1\"}\n".to_string(),
            "not yaml: [".to_string(),
        ] {
            assert!(Markets::parse(&text).is_err(), "{text} should be refused");
        }
    }

    #[test]
    fn a_bad_reload_keeps_the_markets_it_had() {
        let path = std::env::temp_dir().join(format!("markets-{}.yaml", uuid::Uuid::new_v4()));
        fs::write(&path, GOOD).unwrap();
        let registry = MarketRegistry::load(&path).unwrap();
        assert_eq!(registry.current().symbols(), ["GEM", "ITEM"]);

        fs::write(&path, "markets:\n  - {symbol: NEW, tick_size: \"1\"}\n").unwrap();
        assert_eq!(registry.current().symbols(), ["GEM", "ITEM"], "edits wait for a reload");
        registry.reload().unwrap();
        assert_eq!(registry.current().symbols(), ["NEW"]);

        fs::write(&path, "markets: [").unwrap();
        assert!(registry.reload().is_err());
        assert_eq!(registry.current().symbols(), ["NEW"]);

        fs::remove_file(&path).unwrap();
        assert!(registry.reload().is_err());
        assert_eq!(registry.current().symbols(), ["NEW"]);
    }

    #[test]
    fn the_shipped_file_is_valid() {
        let markets = Markets::parse(include_str!("../markets.yaml")).unwrap();
        assert_eq!(markets.symbols(), ["ITEM"]);
    }
}
