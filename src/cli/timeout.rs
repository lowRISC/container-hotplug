use std::{str::FromStr, time::Duration};

#[derive(Clone, Copy)]
pub enum Timeout {
    Some(Duration),
    Infinite,
}

impl FromStr for Timeout {
    type Err = humantime::DurationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "inf" | "infinite" | "none" | "forever" => Timeout::Infinite,
            _ => Timeout::Some(humantime::parse_duration(s)?),
        })
    }
}
