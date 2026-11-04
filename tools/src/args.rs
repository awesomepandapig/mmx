use std::collections::HashMap;
use std::env;
use std::str::FromStr;

/// Parses `--name value` pairs and bare `--flag`s, exiting with `usage` on anything else.
pub struct Args {
    values: HashMap<String, String>,
    usage: &'static str,
}

impl Args {
    pub fn parse(usage: &'static str) -> Self {
        let mut values = HashMap::new();
        let mut args = env::args().skip(1).peekable();

        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else {
                eprintln!("unexpected argument {arg:?}\n\n{usage}");
                std::process::exit(2);
            };
            if name == "help" {
                println!("{usage}");
                std::process::exit(0);
            }
            let value = match args.peek() {
                Some(next) if !next.starts_with("--") => args.next().unwrap_or_default(),
                _ => String::new(),
            };
            values.insert(name.to_string(), value);
        }

        Self { values, usage }
    }

    pub fn flag(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub fn string(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    pub fn get<T: FromStr>(&self, name: &str, default: T) -> T {
        match self.values.get(name) {
            Some(value) => value.parse().unwrap_or_else(|_| {
                eprintln!("invalid value {value:?} for --{name}\n\n{}", self.usage);
                std::process::exit(2);
            }),
            None => default,
        }
    }
}
