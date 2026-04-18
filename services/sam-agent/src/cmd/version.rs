//! `sam version` — short version line.

pub fn run() {
    println!("sam-agent {}", env!("CARGO_PKG_VERSION"));
}
