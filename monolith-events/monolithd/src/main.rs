mod controller;
mod sdk;
mod watchdog;

fn main() {
    let role = std::env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    match role.as_str() {
        "controller" => controller::run(),
        "watchdog" => watchdog::run(),
        "sdk-info" => println!("OpenRGB SDK protocol {}", sdk::PROTOCOL_VERSION),
        _ => eprintln!("usage: monolithd <controller|watchdog|sdk-info>"),
    }
}
