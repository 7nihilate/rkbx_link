use beatkeeper::BeatKeeper;
use log::{Logger, ScopedLogger};
use outputmodules::ModuleDefinition;
use std::path::Path;
use std::{fs, rc::Rc};

mod offsets;
use offsets::RekordboxOffsets;

mod outputmodules;

mod beatkeeper;
mod config;
mod log;
mod utils;
mod memory;
mod status;
mod gui;

use status::{shared, OscMode};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "dev")]
const LICENSE_SERVER: &str = "localhost:4000";
#[cfg(not(feature = "dev"))]
const LICENSE_SERVER: &str = "3gg.se:4000";

#[cfg(feature = "dev")]
const REPO: &str = "grufkork/rkbx_link/rewrite";
#[cfg(not(feature = "dev"))]
const REPO: &str = "grufkork/rkbx_link/refs/heads/master";

#[cfg(target_os = "windows")]
const OFFSETS_PATH: &str = "./data/offsets";
#[cfg(target_os = "macos")]
const OFFSETS_PATH: &str = "./data/offsets-macos";

struct Args {
    /// --local / --lan: overrides osc.mode
    osc_mode: Option<OscMode>,
    /// --nogui: console only, as before
    no_gui: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        osc_mode: None,
        no_gui: false,
    };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--local" => args.osc_mode = Some(OscMode::Local),
            "--lan" => args.osc_mode = Some(OscMode::Lan),
            "--nogui" => args.no_gui = true,
            other => println!("Unknown argument '{other}' (use --local, --lan, --nogui)"),
        }
    }
    args
}

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown".to_string());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        crate::status::shared().push_log(crate::status::LogKind::Error, format!("[Panic]  {what} at {at}"));
        default_hook(info);
    }));
    // config and data/offsets are read relative to the working directory: use the
    // exe's folder, so it works when started from a shortcut or from another program.
    // A build in the repo (target/release/rkbx_link.exe) has no config next to it, so
    // the nearest folder up the tree that has one wins (the repo root).
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let dir = exe_dir
            .ancestors()
            .take(4)
            .find(|d| d.join("config").is_file())
            .unwrap_or(&exe_dir)
            .to_path_buf();
        let _ = std::env::set_current_dir(dir);
    }

    let args = parse_args();
    let gui = !args.no_gui && read_config_flag("app.gui", true);

    if !gui {
        run(args.osc_mode, false);
        return;
    }

    let osc_mode = args.osc_mode;
    std::thread::spawn(move || run(osc_mode, true));
    if let Err(e) = gui::run() {
        println!("Could not open the window ({e}), running in the console. Use --nogui to skip the window.");
        loop {
            std::thread::park();
        }
    }
    // Closing the window quits, even while the worker waits on Rekordbox
    std::process::exit(0);
}

/// Reads one boolean before the logger exists, without warnings for missing keys
fn read_config_flag(key: &str, default: bool) -> bool {
    let Ok(src) = fs::read_to_string("config") else {
        return default;
    };
    src.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(key).and_then(|v| v.strip_prefix(' ')))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn run(osc_mode: Option<OscMode>, gui: bool) {
    println!();
    println!("======================================================================");
    println!();
    println!("Rekordbox Link v{VERSION}");
    println!("Updates          https://github.com/grufkork/rkbx_link/releases/latest");
    println!("Get a license    https://3gg.se/products/rkbx_link");
    println!("Repo and docs    https://github.com/grufkork/rkbx_link");
    println!("Missing a feature? Spotted a bug? Just shoot me a message!");
    println!();
    println!("======================================================================");
    println!();

    let logger = Rc::new(Logger::new(true));

    if let Err(e) = fs::create_dir("./data") {
        match e.kind() {
            std::io::ErrorKind::AlreadyExists => {} // Directory already exists, no problem
            _ => {
                logger.error("App", &format!("Failed to create data directory: {e}"));
                enter_to_exit();
                return;
            }
        }
    }

    let mut config = config::Config::read(ScopedLogger::new(&logger, "Config"));
    let yes_to_all = config.get_or_default("app.yes_to_all", false);

    let logger = Rc::new(Logger::new(config.get_or_default("app.debug", true)));
    config.logger = ScopedLogger::new(&logger, "Config");
    let applogger = ScopedLogger::new(&logger, "App");

    if let Some(mode) = osc_mode {
        let osc_config = config.reduce_to_namespace("osc");
        let mut target = outputmodules::osc::target_from_config(&osc_config, &ScopedLogger::new(&logger, "OSC"));
        target.mode = mode;
        shared().set_osc_target(target);
        applogger.info(&format!("OSC mode from the command line: {}", mode.as_str()));
    }
    // The status module feeds the window, so it runs only with the window
    config.set("status.enabled", if gui { "true" } else { "false" });

    let modules = vec![
        ModuleDefinition::new(
            "link",
            "Ableton Link",
            outputmodules::abletonlink::AbletonLink::create,
        ),
        ModuleDefinition::new("osc", "OSC", outputmodules::osc::Osc::create),
		ModuleDefinition::new("sacn", "sACN", outputmodules::sacn::Sacn::create),
        ModuleDefinition::new("file", "File", outputmodules::file::File::create),
        ModuleDefinition::new(
            "setlist",
            "Setlist",
            outputmodules::setlist::Setlist::create,
        ),
        ModuleDefinition::new(
            "display",
            "Live Display",
            outputmodules::display::Display::create,
        ),
        ModuleDefinition::new(
            "status",
            "Status window",
            outputmodules::status::Status::create,
        ),
    ];

    let mut update = config.get_or_default("app.auto_update", true);
    if !Path::new(OFFSETS_PATH).exists() {
        applogger.err("No offset file found, updating...");
        update = true;
    }

    let license = config.get_or_default::<String>("app.licensekey", "evaluation".to_string());
    update_routine(&license, REPO, ScopedLogger::new(&logger, "Update"), update, yes_to_all);

    let offsets =
        match RekordboxOffsets::from_file(OFFSETS_PATH, ScopedLogger::new(&logger, "Parser")) {
            Ok(offsets) => offsets,
            Err(e) => {
                applogger.err(&format!("Failed to parse offsets: {e}"));
                applogger.err("Enable debug in config for details");
                enter_to_exit();
                return;
            }
        };

    let mut versions: Vec<String> = offsets.keys().map(|x| x.to_string()).collect();
    versions.sort();
    versions.reverse();

    applogger.info(&format!("Rekordbox versions available: {versions:?}"));

    let selected_version = if let Some(version) = config.get("keeper.rekordbox_version") {
        version
    } else {
        applogger.warn("No version specified in config, using latest version");
        versions[0].clone()
    };

    applogger.info(&format!("Targeting Rekordbox version: {selected_version}"));
    shared().update_status(|s| s.target_version = selected_version.clone());

    let offset = if let Some(offset) = offsets.get(&selected_version) {
        offset
    } else {
        applogger.err(&format!(
            "Offsets for Rekordbox version {selected_version} not available"
        ));
        enter_to_exit();
        return;
    };

    BeatKeeper::start(
        offset.clone(),
        modules,
        config,
        ScopedLogger::new(&logger, "BeatKeeper"),
    );
}

fn update_routine(license: &str, repo: &str, logger: ScopedLogger, update_offsets: bool, is_yes_to_all: bool) {
    logger.info("Checking for updates...");
    // Exe update
    let new_exe_version = match get_git_file_http("version_exe", repo) {
        Ok(version) => version,
        Err(e) => {
            logger.err(&format!(
                "Failed to fetch new executable version from repository: {e}"
            ));
            return;
        }
    };
    let new_exe_version = new_exe_version.trim();

    if new_exe_version == VERSION {
        logger.info(&format!("Program up to date (v{new_exe_version})"));
    } else {
        logger.warn(" ");
        logger.warn(&format!(
            "   !! An executable update available is available: v{new_exe_version} !!"
        ));
        logger.warn("Update the program to get the latest offset updates");
        logger.warn("https://github.com/grufkork/rkbx_link/releases/latest");
        logger.warn("");
        return;
    }

    if !update_offsets {
        logger.info("Auto update disabled, skipping offset update check");
        return;
    }

    // Offset update
    let Ok(new_offset_version) = get_licensed_file("version_offsets", license, &logger) else {
        logger.err("Failed to fetch new offset version");
        return;
    };
    let Ok(new_offset_version) = new_offset_version.trim().parse::<i32>() else {
        logger.err(&format!(
            "Failed to parse new offset version: {new_offset_version}"
        ));
        return;
    };

    let mut update_offsets = false;

    if Path::new(OFFSETS_PATH).exists() {
        if Path::new("./data/version_offsets").exists() {
            match fs::read_to_string("./data/version_offsets") {
                Ok(version_offsets) => {
                    if let Ok(version) = version_offsets.trim().parse::<i32>() {
                        if version < new_offset_version {
                            logger.info("Offset update available");
                            update_offsets = true;
                        } else {
                            logger.info(&format!("Offsets up to date (v{new_offset_version})"));
                        }
                    } else {
                        logger.err("Failed to parse version_offsets file");
                        update_offsets = true;
                    }
                }
                Err(e) => {
                    logger.err(&format!("Failed to read version_offsets file: {e}"));
                    update_offsets = true;
                }
            }
        } else {
            logger.warn("Missing version_offsets file");
            update_offsets = true;
        }
    } else {
        logger.warn("Missing offsets file");
        update_offsets = true;
    }

    if update_offsets && y_n("Update offsets?", is_yes_to_all) {
        // Offset update available
        logger.info("Downloading offsets...");
        match get_licensed_file("offsets", license, &logger) {
            Ok(offsets) => {
                std::fs::write(OFFSETS_PATH, offsets).expect("Failed to write offsets file!");
                std::fs::write("./data/version_offsets", new_offset_version.to_string()).expect("Failed to write version_offsets file!");
                logger.good("Offsets updated");
            }
            Err(e) => {
                logger.err(&format!("Failed to fetch offsets from server: {e}"));
            }
        }
    }
}

fn get_git_file_http(path: &str, repo: &str) -> Result<String, String> {
    let url = format!("https://raw.githubusercontent.com/{repo}/{path}");
    let Ok(res) = reqwest::blocking::get(&url) else {
        return Err(format!("Get error: {}", &url));
    };
    if res.status().is_success() {
        Ok(res.text().map_err(|e| e.to_string())?)
    } else {
        Err(format!("Get error {}: {}", res.status(), &url))
    }
}

fn get_licensed_file(path: &str, license: &str, logger: &ScopedLogger) -> Result<String, String> {
    let url = format!("https://{LICENSE_SERVER}/{path}?license={license}");
    logger.debug(&format!("Fetching: {}", &url));
    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(cfg!(feature = "dev"))
        .build()
        .unwrap();

    let res = match client.get(&url).send() {
        Ok(res) => res,
        Err(e) => return Err(format!("Get error: {e}")),
    };

    logger.debug(&format!("{}: {}", res.status(), &url));

    match res.status() {
        reqwest::StatusCode::UNAUTHORIZED => {
            logger.warn("Evaluation or invalid license. Check your license key and try again.");
            Err("Unauthorized".to_string())
        }
        reqwest::StatusCode::OK => Ok(res.text().map_err(|e| e.to_string())?),
        _ => Err(format!("Get error {}: {}", res.status(), &url)),
    }
}

fn y_n(msg: &str, is_yes_to_all: bool) -> bool {
    if is_yes_to_all {
        return true;
    }

    use std::io::{self, Write};
    let mut input = String::new();
    print!("{msg} (y/n): ");
    io::stdout().flush().unwrap();
    io::stdin().read_line(&mut input).unwrap();
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn enter_to_exit() {
    use std::io::{self, Write};
    let mut input = String::new();
    print!("Press Enter to exit...");
    io::stdout().flush().unwrap();
    io::stdin().read_line(&mut input).unwrap();
}

// !cargo r
