use std::env;
use std::sync::LazyLock;

pub static PORT: LazyLock<u16> = LazyLock::new(|| {
    env::var("PORT")
        .map(|port| port.parse().unwrap())
        .unwrap_or(8091)
});
