/// ```cargo
/// [dependencies]
/// chrono = { version = "0.4", features = ["serde"] }
/// serde_json = "1.0"
/// ```

/// Print Termux device status as JSON.
use chrono::Local;
use serde_json::{json, Value};
use std::process::{Command, Stdio};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--help") {
        println!("Print Termux battery and Wi-Fi status as JSON.");
        return;
    }
    let battery = termux_json("termux-battery-status");
    let wifi = termux_json("termux-wifi-connectioninfo");
    let output = json!({
        "timestamp": Local::now().to_rfc3339(),
        "battery": battery,
        "wifi": wifi,
    });
    println!("{output}");
}

fn termux_json(command: &str) -> Value {
    Command::new(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice(&output.stdout).ok())
        .unwrap_or(Value::Null)
}
