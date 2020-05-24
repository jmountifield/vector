#[macro_use]
extern crate tracing;

use futures01::{future, Future, Stream};
use std::{
    cmp::max,
    fs::File,
    path::{Path, PathBuf},
};
use structopt::{clap::AppSettings, StructOpt};
#[cfg(unix)]
use tokio_signal::unix::{Signal, SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use topology::Config;
use vector::{config_paths, event, generate, list, metrics, runtime, topology, trace, unit_test};

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
struct Opts {
    #[structopt(flatten)]
    root: RootOpts,

    #[structopt(subcommand)]
    sub_command: Option<SubCommand>,
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
struct RootOpts {
    /// Read configuration from one or more files. Wildcard paths are supported.
    /// If zero files are specified the default config path
    /// `/etc/vector/vector.toml` will be targeted.
    #[structopt(name = "config", short, long)]
    config_paths: Vec<PathBuf>,

    /// Exit on startup if any sinks fail healthchecks
    #[structopt(short, long)]
    require_healthy: bool,

    /// Number of threads to use for processing (default is number of available cores)
    #[structopt(short, long)]
    threads: Option<usize>,

    /// Enable more detailed internal logging. Repeat to increase level. Overridden by `--quiet`.
    #[structopt(short, long, parse(from_occurrences))]
    verbose: u8,

    /// Reduce detail of internal logging. Repeat to reduce further. Overrides `--verbose`.
    #[structopt(short, long, parse(from_occurrences))]
    quiet: u8,

    /// Set the logging format
    #[structopt(long, default_value = "text", possible_values = &["text", "json"])]
    log_format: LogFormat,

    /// Control when ANSI terminal formatting is used.
    ///
    /// By default `vector` will try and detect if `stdout` is a terminal, if it is
    /// ANSI will be enabled. Otherwise it will be disabled. By providing this flag with
    /// the `--color always` option will always enable ANSI terminal formatting. `--color never`
    /// will disable all ANSI terminal formatting. `--color auto` will attempt
    /// to detect it automatically.
    #[structopt(long, default_value = "auto", possible_values = &["auto", "always", "never"])]
    color: Color,

    /// Watch for changes in configuration file, and reload accordingly.
    #[structopt(short, long)]
    watch_config: bool,
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
enum SubCommand {
    /// Validate the target config, then exit.
    Validate(Validate),

    /// Generate a Vector configuration containing a list of components.
    Generate(generate::Opts),

    /// List available components, then exit.
    List(list::Opts),

    /// Run Vector config unit tests, then exit. This command is experimental and therefore subject to change.
    /// For guidance on how to write unit tests check out: https://vector.dev/docs/setup/guides/unit-testing/
    Test(unit_test::Opts),
}

#[derive(StructOpt, Debug)]
#[structopt(rename_all = "kebab-case")]
struct Validate {
    /// Disables topology check
    #[structopt(short, long)]
    no_topology: bool,

    /// Disables healthcheck check
    #[structopt(short, long)]
    no_healthchecks: bool,

    /// Fail validation on warnings
    #[structopt(short, long)]
    deny_warnings: bool,

    /// Any number of Vector config files to validate. If none are specified the
    /// default config path `/etc/vector/vector.toml` will be targeted.
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
enum Color {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq)]
enum LogFormat {
    Text,
    Json,
}

impl std::str::FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Color::Auto),
            "always" => Ok(Color::Always),
            "never" => Ok(Color::Never),
            s => Err(format!(
                "{} is not a valid option, expected `auto`, `always` or `never`",
                s
            )),
        }
    }
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            s => Err(format!(
                "{} is not a valid option, expected `text` or `json`",
                s
            )),
        }
    }
}

fn main() {
    openssl_probe::init_ssl_cert_env_vars();
    let version = vector::get_version();
    let app = Opts::clap().version(&version[..]).global_settings(&[
        AppSettings::ColoredHelp,
        AppSettings::InferSubcommands,
        AppSettings::DeriveDisplayOrder,
    ]);
    let root_opts = Opts::from_clap(&app.get_matches());
    let opts = root_opts.root;
    let sub_command = root_opts.sub_command;

    let level = match opts.quiet {
        0 => match opts.verbose {
            0 => "info",
            1 => "debug",
            2..=255 => "trace",
        },
        1 => "warn",
        2 => "error",
        3..=255 => "off",
    };

    let levels = match std::env::var("LOG").ok() {
        Some(level) => level,
        None => match level {
            "off" => "off".to_string(),
            _ => [
                format!("vector={}", level),
                format!("codec={}", level),
                format!("file_source={}", level),
                format!("tower_limit=trace"),
                format!("rdkafka={}", level),
            ]
            .join(",")
            .to_string(),
        },
    };

    let color = match opts.color.clone() {
        #[cfg(unix)]
        Color::Auto => atty::is(atty::Stream::Stdout),
        #[cfg(windows)]
        Color::Auto => false, // ANSI colors are not supported by cmd.exe
        Color::Always => true,
        Color::Never => false,
    };

    let json = match &opts.log_format {
        LogFormat::Text => false,
        LogFormat::Json => true,
    };

    trace::init(color, json, levels.as_str());

    metrics::init().expect("metrics initialization failed");

    sub_command.map(|s| {
        std::process::exit(match s {
            SubCommand::Validate(v) => validate(&v),
            SubCommand::List(l) => list::cmd(&l),
            SubCommand::Test(t) => unit_test::cmd(&t),
            SubCommand::Generate(g) => generate::cmd(&g),
        })
    });

    info!("Log level {:?} is enabled.", level);

    if let Some(threads) = opts.threads {
        if threads < 1 {
            error!("The `threads` argument must be greater or equal to 1.");
            std::process::exit(exitcode::CONFIG);
        }
    }

    let config_paths = prepare_config_paths(opts.config_paths.clone()).unwrap_or_else(|| {
        std::process::exit(exitcode::CONFIG);
    });

    if opts.watch_config {
        // Start listening for config changes immediately.
        vector::topology::config::watcher::config_watcher(
            config_paths.clone(),
            vector::topology::config::watcher::CONFIG_WATCH_DELAY,
        )
        .unwrap_or_else(|error| {
            error!(message = "Unable to start config watcher.", %error);
            std::process::exit(exitcode::CONFIG);
        });
    }

    info!(
        message = "Loading configs.",
        path = ?config_paths
    );

    let config = read_configs(&config_paths);
    let config = handle_config_errors(config);
    let config = config.unwrap_or_else(|| {
        std::process::exit(exitcode::CONFIG);
    });
    event::LOG_SCHEMA
        .set(config.global.log_schema.clone())
        .expect("Couldn't set schema");

    let mut rt = {
        let threads = opts.threads.unwrap_or(max(1, num_cpus::get()));
        runtime::Runtime::with_thread_count(threads).expect("Unable to create async runtime")
    };

    info!(
        message = "Vector is starting.",
        version = built_info::PKG_VERSION,
        git_version = built_info::GIT_VERSION.unwrap_or(""),
        released = built_info::BUILT_TIME_UTC,
        arch = built_info::CFG_TARGET_ARCH
    );

    let diff = topology::ConfigDiff::initial(&config);
    let pieces = topology::validate(&config, &diff, rt.executor()).unwrap_or_else(|| {
        std::process::exit(exitcode::CONFIG);
    });

    let result = topology::start_validated(config, diff, pieces, &mut rt, opts.require_healthy);
    let (topology, mut graceful_crash) = result.unwrap_or_else(|| {
        std::process::exit(exitcode::CONFIG);
    });

    #[cfg(unix)]
    {
        let mut topology = topology;
        let sigint = Signal::new(SIGINT).flatten_stream();
        let sigterm = Signal::new(SIGTERM).flatten_stream();
        let sigquit = Signal::new(SIGQUIT).flatten_stream();
        let sighup = Signal::new(SIGHUP).flatten_stream();

        let mut signals = sigint.select(sigterm.select(sigquit.select(sighup)));

        let signal = loop {
            let signal = future::poll_fn(|| signals.poll());
            let to_shutdown = future::poll_fn(|| graceful_crash.poll())
                .map(|_| ())
                .select(topology.sources_finished());

            let next = signal
                .select2(to_shutdown)
                .wait()
                .map_err(|_| ())
                .expect("Neither stream errors");

            let signal = match next {
                future::Either::A((signal, _)) => signal.expect("Signal streams never end"),
                // Trigger graceful shutdown if a component crashed, or all sources have ended.
                future::Either::B((_to_shutdown, _)) => SIGINT,
            };

            if signal != SIGHUP {
                break signal;
            }

            // Reload config
            info!(
                message = "Reloading configs.",
                path = ?config_paths
            );
            let config = read_configs(&config_paths);

            trace!("Parsing config");
            let config = handle_config_errors(config);
            if let Some(config) = config {
                match topology.reload_config_and_respawn(config, &mut rt, opts.require_healthy) {
                    Ok(true) => (),
                    Ok(false) => error!("Reload was not successful."),
                    // Trigger graceful shutdown for what remains of the topology
                    Err(()) => break SIGINT,
                }
            } else {
                error!("Reload aborted.");
            }
        };

        if signal == SIGINT || signal == SIGTERM {
            use futures01::future::Either;

            info!("Shutting down.");
            let shutdown = topology.stop();

            match rt.block_on(shutdown.select2(signals.into_future())) {
                Ok(Either::A(_)) => { /* Graceful shutdown finished */ }
                Ok(Either::B(_)) => {
                    info!("Shutting down immediately.");
                    // Dropping the shutdown future will immediately shut the server down
                }
                Err(_) => unreachable!(),
            }
        } else if signal == SIGQUIT {
            info!("Shutting down immediately");
            drop(topology);
        } else {
            unreachable!();
        }
    }
    #[cfg(windows)]
    {
        let ctrl_c = tokio_signal::ctrl_c().flatten_stream().into_future();
        let to_shutdown = future::poll_fn(move || graceful_crash.poll())
            .map(|_| ())
            .select(topology.sources_finished());

        let interruption = rt
            .block_on(ctrl_c.select2(to_shutdown))
            .map_err(|_| ())
            .expect("Neither stream errors");

        use futures01::future::Either;

        let ctrl_c = match interruption {
            Either::A(((_, ctrl_c_stream), _)) => ctrl_c_stream.into_future(),
            Either::B((_, ctrl_c)) => ctrl_c,
        };

        info!("Shutting down.");
        let shutdown = topology.stop();

        match rt.block_on(shutdown.select2(ctrl_c)) {
            Ok(Either::A(_)) => { /* Graceful shutdown finished */ }
            Ok(Either::B(_)) => {
                info!("Shutting down immediately.");
                // Dropping the shutdown future will immediately shut the server down
            }
            Err(_) => unreachable!(),
        }
    }

    rt.shutdown_now().wait().unwrap();
}

fn prepare_config_paths(paths: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    let mut config_paths = config_paths::expand(paths)?;
    config_paths.sort();
    config_paths.dedup();
    config_paths::CONFIG_PATHS
        .set(config_paths.clone())
        .expect("Cannot set global config paths");
    Some(config_paths)
}

fn handle_config_errors(config: Result<Config, Vec<String>>) -> Option<Config> {
    match config {
        Err(errors) => {
            for error in errors {
                error!("Configuration error: {}", error);
            }
            None
        }
        Ok(config) => Some(config),
    }
}

fn read_configs(config_paths: &Vec<PathBuf>) -> Result<Config, Vec<String>> {
    let mut config = vector::topology::Config::empty();
    let mut errors = Vec::new();

    config_paths.iter().for_each(|p| {
        let file = if let Some(file) = open_config(&p) {
            file
        } else {
            errors.push(format!("Config file not found in path: {:?}.", p));
            return;
        };

        trace!(
            message = "Parsing config.",
            path = ?p
        );

        match Config::load(file).and_then(|n| config.append(n)) {
            Err(errs) => errors.extend(errs.iter().map(|e| format!("{:?}: {}", p, e))),
            _ => (),
        };
    });

    if let Err(mut errs) = config.expand_macros() {
        errors.append(&mut errs);
    }

    if !errors.is_empty() {
        Err(errors)
    } else {
        Ok(config)
    }
}

fn open_config(path: &Path) -> Option<File> {
    match File::open(path) {
        Ok(f) => Some(f),
        Err(error) => {
            if let std::io::ErrorKind::NotFound = error.kind() {
                error!(message = "Config file not found in path.", ?path);
                None
            } else {
                error!(message = "Error opening config file.", %error);
                None
            }
        }
    }
}

fn validate(opts: &Validate) -> exitcode::ExitCode {
    use futures::compat::Future01CompatExt;

    // Print constants,functions
    let error_intro = "   x";
    let warning_intro = "   ~";
    let print_sub = |intro, errors| {
        for error in errors {
            println!("  {} {}", intro, error);
        }
    };
    let print_success = |message: &str| println!("√ {}", message);
    let print_title = |title| println!("- {}", title); // TODO: Maybe add ------ beneth it
    let print_warnings = |warnings| {
        let intro = if opts.deny_warnings {
            error_intro
        } else {
            warning_intro
        };
        print_sub(intro, warnings);
    };
    let print_errors = |errors| print_sub(error_intro, errors);
    let print_error = |error| println!("{} {}", error_intro, error);

    // Prepare paths
    let paths = if let Some(paths) = prepare_config_paths(opts.paths.clone()) {
        paths
    } else {
        println!("x No config file paths");
        return exitcode::CONFIG;
    };

    // Validate configuration files
    let mut success = true;
    let mut full_config = vector::topology::Config::empty();
    for config_path in paths {
        let mut failed = || {
            success = false;
            println!("# Config: {:?}", &config_path);
        };

        let file = match File::open(&config_path) {
            Ok(file) => file,
            Err(error) => {
                failed();
                if let std::io::ErrorKind::NotFound = error.kind() {
                    println!("{} File not found", error_intro);
                } else {
                    println!("{} Error opening file: {:?}", error_intro, error);
                }
                continue;
            }
        };

        trace!(
            message = "Parsing config.",
            path = ?config_path
        );

        let mut sub_failed = |title, errors| {
            failed();
            print_title(title);
            print_errors(errors);
        };

        let mut config = match vector::topology::Config::load(file) {
            Ok(config) => config,
            Err(errors) => {
                sub_failed("Failed to parse file", errors);
                continue;
            }
        };

        if let Err(errors) = config.expand_macros() {
            sub_failed("Failed to expand macros", errors);
            continue;
        }

        if let Err(errors) = full_config.append(config) {
            sub_failed("Failed in merging config", errors);
            continue;
        }

        debug!(
            message = "Validation successful",
            path = ?config_path
        );
    }

    // Validate configuration of components

    event::LOG_SCHEMA
        .set(full_config.global.log_schema.clone())
        .expect("Couldn't set schema");

    let mut rt = runtime::Runtime::with_thread_count(1).expect("Unable to create async runtime");
    let diff = topology::ConfigDiff::initial(&full_config);
    let mut pieces = match topology::builder::build_pieces(&full_config, &diff, rt.executor()) {
        Ok((pieces, warnings)) => {
            if warnings.is_empty() {
                pieces
            } else {
                print_title("Component warnings");
                print_warnings(warnings);
                if opts.deny_warnings {
                    return exitcode::CONFIG;
                }
                pieces
            }
        }
        Err(errors) => {
            print_title("Component errors");
            print_errors(errors);
            return exitcode::CONFIG;
        }
    };

    if success {
        print_success("Configuration options are valid");
    } else {
        return exitcode::CONFIG;
    }

    // Validate topology
    if !opts.no_topology {
        let success = match topology::builder::check(&full_config) {
            Ok(warnings) => {
                if warnings.is_empty() {
                    true
                } else {
                    print_title("Topology warnings");
                    print_warnings(warnings);
                    !opts.deny_warnings
                }
            }
            Err(errors) => {
                print_title("Topology errors");
                print_errors(errors);
                false
            }
        };

        if success {
            print_success("Configuration topology is valid");
        } else {
            return exitcode::CONFIG;
        }
    }

    // Validate healthchecks
    if !opts.no_healthchecks {
        let healthchecks = topology::take_healthchecks(&diff, &mut pieces);
        if !healthchecks.is_empty() {
            print_title("Health checks");
            // We are running health checks in serial so it's easier for the users
            // to parse which errors/warnings/etc. belong to which healthcheck.
            let mut success = true;
            for (name, healthcheck) in healthchecks {
                let mut failed = |error| {
                    success = false;
                    print_error(error);
                };
                let handle = rt.spawn_handle(healthcheck.compat());
                match rt.block_on_std(handle) {
                    Ok(Ok(())) => print_success(name.as_str()),
                    Ok(Err(())) => failed(name),
                    Err(error) if error.is_cancelled() => {
                        failed(format!("for {} was cancelled", name))
                    }
                    Err(_) => failed(format!("for {} has panicked", name)),
                }
            }
            if !success {
                return exitcode::CONFIG;
            }
        }
    }

    if topology::start_validated(full_config, diff, pieces, &mut rt, false).is_none() {
        print_error("Topology failed to start".to_owned());
        return exitcode::CONFIG;
    }

    println!("Validated");

    exitcode::OK
}

#[allow(unused)]
mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}
