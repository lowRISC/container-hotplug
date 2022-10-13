use std::{str::FromStr, time::Duration};

#[derive(Clone, Copy)]
pub enum Timeout {
    Some(Duration),
    Infinite,
}

impl FromStr for Timeout {
    type Err = humantime::DurationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "inf" || s == "infinite" || s == "none" || s == "forever" {
            return Ok(Timeout::Infinite);
        }
        Ok(Timeout::Some(humantime::parse_duration(s)?))
    }
}
