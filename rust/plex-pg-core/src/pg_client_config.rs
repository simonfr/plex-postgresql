use crate::env_utils;
use crate::pg_config::PgEnvConfig;

pub(crate) fn load_conn_config() -> PgEnvConfig {
    PgEnvConfig::from_env()
}

pub(crate) fn parse_positive_env_or_default(name: &str, default_value: i32) -> i32 {
    env_utils::env_string(name)
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default_value)
}

pub(crate) fn env_nonzero(name: &str) -> bool {
    env_utils::env_string(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}
