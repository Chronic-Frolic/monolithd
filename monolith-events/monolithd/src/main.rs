mod controller;
mod sdk;
mod watchdog;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("sdk-discover") => println!("{:?}", sdk::discover()),
        Some("sdk-identities") => match sdk::identities() {
            Ok(items) => for item in items {
                println!("{:?} {} {} {} {}", item.id, item.name, item.vendor, item.serial, item.location);
            },
            Err(error) => { eprintln!("SDK identity failed: {error}"); std::process::exit(1); }
        },
        Some("controller") => controller::run(),
        Some("watchdog") => watchdog::run(),
        _ => eprintln!("usage: monolithd <sdk-discover|sdk-identities|controller|watchdog>"),
    }
}
