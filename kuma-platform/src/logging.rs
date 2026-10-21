//! The one log format every kuma binary speaks: `RUST_LOG` through
//! env_logger's default filter, millis in the timestamp. The millis
//! are the point: the difference between a key's bounce (two requests
//! tens of ms apart) and a human's second press, in one journalctl
//! pass, is only visible if every binary stamps the same way.

/// Install the format. Call once, first line of `main`.
pub fn init() {
    use std::io::Write as _;
    env_logger::Builder::from_env(env_logger::Env::default())
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} {:<5} {}] {}",
                buf.timestamp_millis(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}
