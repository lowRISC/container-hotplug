use std::{fmt::Display, str::FromStr};

use anyhow::bail;

#[derive(Clone)]
pub struct LogFormat {
    pub timestamp: bool,
    pub path: bool,
    pub module: bool,
    pub level: bool,
}

impl Default for LogFormat {
    fn default() -> Self {
        Self {
            timestamp: true,
            path: false,
            module: true,
            level: true,
        }
    }
}

impl FromStr for LogFormat {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut result = Self::default();
        let mut value = true;
        for c in s.chars() {
            match c {
                '+' => value = true,
                '-' => value = false,
                't' => result.timestamp = value,
                'p' => result.path = value,
                'm' => result.module = value,
                'l' => result.level = value,
                _ => bail!("Invalid log formatting flag"),
            };
        }
        Ok(result)
    }
}

impl Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut plus = String::from("+");
        let mut minus = String::from("-");

        if self.timestamp {
            plus.push('t');
        } else {
            minus.push('t');
        }

        if self.path {
            plus.push('p');
        } else {
            minus.push('p');
        }

        if self.module {
            plus.push('m');
        } else {
            minus.push('m');
        }

        if self.level {
            plus.push('l');
        } else {
            minus.push('l');
        }

        if plus.len() == 1 {
            plus.clear();
        }

        if minus.len() == 1 {
            minus.clear();
        }

        write!(f, "{plus}{minus}")
    }
}
