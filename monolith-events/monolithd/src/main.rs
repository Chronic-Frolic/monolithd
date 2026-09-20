mod controller;
mod sdk;
mod watchdog;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("sdk-discover") => match sdk::discover() {
            Ok(ids) => println!("{ids:?}"),
            Err(error) => { eprintln!("SDK discovery failed: {error}"); std::process::exit(1); }
        },
        Some("controller") => controller::run(),
        Some("watchdog") => watchdog::run(),
        _ => eprintln!("usage: monolithd <sdk-discover|controller|watchdog>"),
    }
}
