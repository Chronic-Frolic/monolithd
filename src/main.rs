mod allocator;
mod animation;
mod calibrate;
mod config;
mod controller;
mod e131;
mod gateway;
mod qlc;
mod registry;
mod remote;
mod reporter;
mod reporters;
mod openrgb;
mod openrgb_sdk;
mod paths;
mod hardware;
mod input_activity;
mod job;
mod sleep_policy;
mod steam;
mod storage;
mod ws;
mod supervisor;
mod watchdog;
mod workspace;

fn usage() -> ! {
    eprintln!("usage: monolithd <e131-receiver|validate-registry [PATH]|workspace <list|select NAME [--check]>|scene <status|start NAME|start-set NAME...|replace NAME|stop NAME|progress ZONE N>|calibrate <--show|ZONE R G B>|event <status|ambient SET|job-start ID LABEL TOTAL [PRIORITY]|job-progress ID N [TOTAL]|job-complete ID|job-fail ID REASON|pause|resume>|remote [PORT]|steam-reporter [--dry-run]|reporters [--dry-run]|sleep-policy|storage-status MOUNT...|hardware-status|hardware-ack|job [--dry-run] [--label TEXT] rsync ARGS...|probe-header <sweep [FROM TO [DWELL_MS]]|at N [SECONDS]>|controller|watchdog|input-activity [FILE]>");
    std::process::exit(2);
}

fn validate_registry(path: Option<String>) -> Result<(), String> {
    println!("{}", paths::describe());
    let root = paths::config_dir();
    let path = path.map(std::path::PathBuf::from).unwrap_or_else(|| root.join("qlc-functions.toml"));
    let layout = config::load_layout(&root.join("scene-layout.toml"))?;
    let mut problems = registry::validate_files(&path, &layout)?;
    match config::check_calibration_file(&root.join("led-calibration.toml"), &layout) {
        Ok(Some(calibration)) => println!("calibration: {}", calibration.describe()),
        Ok(None) => println!("calibration: no file; all zones at unity gain"),
        Err(error) => problems.push(error),
    }
    match registry::load_and_validate(&path, &layout) {
        Ok((registry, _)) => {
            match std::fs::read_to_string(root.join(&registry.workspace)) {
                Ok(xml) => {
                    problems.extend(workspace::check_fill_orders(&xml, &registry));
                    match animation::Animation::load() {
                        Ok(Some(animation)) => {
                            let found = animation.check(&xml, &registry);
                            if found.is_empty() {
                                println!("animated bars: {} loop sets, {} ambient cue lists", animation.loops.len(), animation.cues.len());
                            }
                            problems.extend(found);
                        }
                        Ok(None) => println!("animated bars: no progress-animation.toml; bars are static"),
                        Err(error) => problems.push(error),
                    }
                }
                Err(error) => problems.push(format!("read the workspace to check fill orders: {error}")),
            }
            match controller::load_config(&root.join("controller.toml")) {
                Ok(config) => {
                    let found = controller::check_config(&config, &registry);
                    if found.is_empty() {
                        println!("controller policy: ambient {}, progress zones {:?}, hold {} s", config.default_ambient, config.progress_zones, config.complete_hold_seconds);
                    }
                    problems.extend(found);
                }
                Err(error) => problems.push(error),
            }
            let watchdog_path = root.join("watchdog.toml");
            match std::fs::read_to_string(&watchdog_path).map_err(|error| error.to_string()).and_then(|text| watchdog::parse_config(&text)) {
                Ok(zones) => {
                    let found = watchdog::check_zones(&zones, &registry);
                    if found.is_empty() {
                        println!("watchdog zones: {}", zones.join(", "));
                    }
                    problems.extend(found.into_iter().map(|problem| format!("watchdog.toml: {problem}")));
                }
                Err(error) => problems.push(format!("{}: {error}", watchdog_path.display())),
            }
        }
        Err(error) => problems.push(error),
    }
    if problems.is_empty() {
        println!("registry, calibration, controller and watchdog policy OK");
        return Ok(());
    }
    for problem in &problems { eprintln!("  - {problem}"); }
    Err(format!("{} configuration problem(s)", problems.len()))
}

async fn probe_header(arguments: Vec<String>) -> Result<(), String> {
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let number = |text: &str| text.parse::<usize>().map_err(|_| format!("{text:?} is not a positive integer"));
    let milliseconds = |text: &str| number(text).map(|value| std::time::Duration::from_millis(value as u64));
    match words.as_slice() {
        ["sweep"] => openrgb::probe_header(1..=60, std::time::Duration::from_millis(2500)).await,
        ["sweep", from, to] => openrgb::probe_header(number(from)?..=number(to)?, std::time::Duration::from_millis(2500)).await,
        ["sweep", from, to, dwell] => openrgb::probe_header(number(from)?..=number(to)?, milliseconds(dwell)?).await,
        ["at", n] => openrgb::probe_header(number(n)?..=number(n)?, std::time::Duration::from_secs(60)).await,
        ["at", n, seconds] => openrgb::probe_header(number(n)?..=number(n)?, std::time::Duration::from_secs(number(seconds)? as u64)).await,
        _ => Err("usage: monolithd probe-header sweep [FROM TO [DWELL_MS]] | probe-header at N [SECONDS]".to_owned()),
    }
}

#[tokio::main]
async fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        Some("e131-receiver") => e131::run().await,
        Some("scene") => gateway::client(arguments.collect()).await,
        Some("calibrate") => calibrate::run(arguments.collect()),
        Some("probe-header") => probe_header(arguments.collect()).await,
        Some("validate-registry") => validate_registry(arguments.next()),
        Some("workspace") => workspace::run(arguments.collect()),
        Some("controller") => controller::run().await,
        Some("event") => controller::client(arguments.collect()).await,
        Some("remote") => remote::run(arguments.next()).await,
        Some("steam-reporter") => steam::run(arguments.collect()).await,
        Some("reporters") => reporters::run(arguments.collect()).await,
        Some("sleep-policy") => sleep_policy::run(arguments.collect()).await,
        Some("job") => job::run(arguments.collect()).await,
        Some("storage-status") => storage::run(arguments.collect()).await,
        Some("hardware-status") => hardware::status().await,
        Some("hardware-ack") => hardware::acknowledge().await,
        Some("watchdog") => watchdog::run().await,
        Some("input-activity") => input_activity::run(arguments.collect()).await,
        _ => usage(),
    };
    if let Err(error) = result { eprintln!("monolithd: {error}"); std::process::exit(1); }
}
