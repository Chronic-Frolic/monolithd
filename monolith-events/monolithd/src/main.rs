mod allocator;
mod calibrate;
mod config;
mod controller;
mod e131;
mod gateway;
mod qlc;
mod registry;
mod remote;
mod renderer;
mod supervisor;
mod watchdog;

fn usage() -> ! {
    eprintln!("usage: monolithd <describe|render-idle|render-working CPU GPU MEMORY TASK|render-progress COMPLETED|render-warning|render-fault PHASE|render-quiet|render-controller-fault|e131-receiver|validate-registry [PATH]|scene <status|start NAME|start-set NAME...|replace NAME|stop NAME|progress ZONE N>|calibrate <--show|ZONE R G B>|event <status|ambient SET|job-start ID LABEL TOTAL [PRIORITY]|job-progress ID N [TOTAL]|job-complete ID|job-fail ID REASON|pause|resume>|remote [PORT]|probe-header <sweep [FROM TO [DWELL_MS]]|at N [SECONDS]>|controller|watchdog>");
    std::process::exit(2);
}

fn number(argument: Option<String>, name: &str) -> usize {
    argument.unwrap_or_else(|| usage()).parse().unwrap_or_else(|_| { eprintln!("{name} must be an integer"); usage() })
}

fn validate_registry(path: Option<String>) -> Result<(), String> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let path = path.map(std::path::PathBuf::from).unwrap_or_else(|| root.join("qlc-functions.toml"));
    let layout = config::load_layout(&root.join("scene-layout.toml"))?;
    let mut problems = registry::validate_files(&path, &layout)?;
    match config::check_calibration_file(&root.join("led-calibration.toml"), &layout) {
        Ok(Some(calibration)) => println!("calibration: {}", calibration.describe()),
        Ok(None) => println!("calibration: no file; all zones at unity gain"),
        Err(error) => problems.push(error),
    }
    match registry::load_and_validate(&path, &layout) {
        Ok((registry, _)) => match controller::load_config(&root.join("controller.toml")) {
            Ok(config) => {
                let found = controller::check_config(&config, &registry);
                if found.is_empty() {
                    println!("controller policy: ambient {}, progress zones {:?}, hold {} s", config.default_ambient, config.progress_zones, config.complete_hold_seconds);
                }
                problems.extend(found);
            }
            Err(error) => problems.push(error),
        },
        Err(error) => problems.push(error),
    }
    if problems.is_empty() {
        println!("registry, calibration and controller policy OK");
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
        ["sweep"] => renderer::probe_header(1..=60, std::time::Duration::from_millis(2500)).await,
        ["sweep", from, to] => renderer::probe_header(number(from)?..=number(to)?, std::time::Duration::from_millis(2500)).await,
        ["sweep", from, to, dwell] => renderer::probe_header(number(from)?..=number(to)?, milliseconds(dwell)?).await,
        ["at", n] => renderer::probe_header(number(n)?..=number(n)?, std::time::Duration::from_secs(60)).await,
        ["at", n, seconds] => renderer::probe_header(number(n)?..=number(n)?, std::time::Duration::from_secs(number(seconds)? as u64)).await,
        _ => Err("usage: monolithd probe-header sweep [FROM TO [DWELL_MS]] | probe-header at N [SECONDS]".to_owned()),
    }
}

#[tokio::main]
async fn main() {
    let mut arguments = std::env::args().skip(1);
    let result = match arguments.next().as_deref() {
        Some("describe") => renderer::describe().map(|description| println!("{description}")),
        Some("render-idle") => renderer::idle().await,
        Some("render-working") => renderer::working(number(arguments.next(), "CPU"), number(arguments.next(), "GPU"), number(arguments.next(), "MEMORY"), number(arguments.next(), "TASK")).await,
        Some("render-progress") => renderer::working_progress(number(arguments.next(), "COMPLETED")).await,
        Some("render-warning") => renderer::warning().await,
        Some("render-fault") => renderer::fault(number(arguments.next(), "PHASE")).await,
        Some("render-quiet") => renderer::quiet().await,
        Some("render-controller-fault") => renderer::controller_fault().await,
        Some("e131-receiver") => e131::run().await,
        Some("scene") => gateway::client(arguments.collect()).await,
        Some("calibrate") => calibrate::run(arguments.collect()),
        Some("probe-header") => probe_header(arguments.collect()).await,
        Some("validate-registry") => validate_registry(arguments.next()),
        Some("controller") => controller::run().await,
        Some("event") => controller::client(arguments.collect()).await,
        Some("remote") => remote::run(arguments.next()).await,
        Some("watchdog") => { watchdog::run(); Ok(()) },
        _ => usage(),
    };
    if let Err(error) = result { eprintln!("monolithd: {error}"); std::process::exit(1); }
}
