#![type_length_limit = "10000000"]
mod instrumentation;
pub mod node;

#[cfg(feature = "redis")]
mod redis_store;

use clap::{crate_version, App, Arg, ArgMatches};
use config::{Config, Source};
use config::{ConfigError, FileFormat, Value};
use libc::{c_int, isatty};
use node::InterledgerNode;
use std::{
    ffi::{OsStr, OsString},
    io::Read,
    vec::Vec,
};

#[tokio::main]
async fn main() {
    // The naming convention of arguments
    //
    // - URL vs URI
    //     - Basically it is recommended to use `URL` though both are technically correct.
    //       https://danielmiessler.com/study/url-uri/
    // - An address or a port
    //     - Use `xxx_bind_address` because it becomes more flexible than just binding it to
    //       `127.0.0.1` with a given port.
    // - ILP over HTTP or BTP server URLs which accept ILP packets
    //     - `ilp_over_http_url`
    //     - `ilp_over_btp_url`
    // - Addresses to which ILP over HTTP or BTP servers are bound
    //     - `http_bind_address`
    // - Addresses to which other services are bound
    //     - `xxx_bind_address`
    let mut app = App::new("ilp-node")
        .about("Run an Interledger.rs node (sender, connector, receiver bundle)")
        .version(crate_version!())
        // TODO remove this line once this issue is solved:
    // https://github.com/clap-rs/clap/issues/1536
    .after_help("")
    .args(&[
        // Positional arguments
        Arg::with_name("config")
            .takes_value(true)
            .index(1)
            .help("Name of config file (in JSON, YAML, or TOML format)"),
        // Non-positional arguments
        Arg::with_name("ilp_address")
            .long("ilp_address")
            .takes_value(true)
            .help("ILP Address of this account"),
        Arg::with_name("secret_seed")
            .long("secret_seed")
            .takes_value(true)
            .required(true)
            .help("Root secret used to derive encryption keys. This MUST NOT be changed after once you started up the node. You can generate a random secret by running `openssl rand -hex 32`"),
        Arg::with_name("admin_auth_token")
            .long("admin_auth_token")
            .takes_value(true)
            .required(true)
            .help("HTTP Authorization token for the node admin (sent as a Bearer token)"),
        Arg::with_name("database_url")
            .long("database_url")
            // temporary alias for backwards compatibility
            .alias("redis_url")
            .takes_value(true)
            .default_value("redis://127.0.0.1:6379")
            .help("Redis URI (for example, \"redis://127.0.0.1:6379\" or \"unix:/tmp/redis.sock\")"),
        Arg::with_name("http_bind_address")
            .long("http_bind_address")
            .takes_value(true)
            .help("IP address and port to listen for HTTP connections. This is used for both the API and ILP over HTTP packets. ILP over HTTP is a means to transfer ILP packets instead of BTP connections"),
        Arg::with_name("settlement_api_bind_address")
            .long("settlement_api_bind_address")
            .takes_value(true)
            .help("IP address and port to listen for the Settlement Engine API"),
        Arg::with_name("default_spsp_account")
            .long("default_spsp_account")
            .takes_value(true)
            .help("When SPSP payments are sent to the root domain, the payment pointer is resolved to <domain>/.well-known/pay. This value determines which account those payments will be sent to."),
        Arg::with_name("route_broadcast_interval")
            .long("route_broadcast_interval")
            .takes_value(true)
            .help("Interval, defined in milliseconds, on which the node will broadcast routing information to other nodes using CCP. Defaults to 30000ms (30 seconds)."),
        Arg::with_name("exchange_rate.provider")
            .long("exchange_rate.provider")
            .takes_value(true)
            .help("Exchange rate API to poll for exchange rates. If this is not set, the node will not poll for rates and will instead use the rates set via the HTTP API. \
                Note that CryptoCompare can also be used when the node is configured via a config file or stdin, because an API key must be provided to use that service."),
        Arg::with_name("exchange_rate.poll_interval")
            .long("exchange_rate.poll_interval")
            .default_value("60000")
            .help("Interval, defined in milliseconds, on which the node will poll the exchange_rate.provider (if specified) for exchange rates."),
        Arg::with_name("exchange_rate.spread")
            .long("exchange_rate.spread")
            .default_value("0")
            .help("Spread, as a fraction, to add on top of the exchange rate. \
                This amount is kept as the node operator's profit, or may cover \
                fluctuations in exchange rates.
                For example, take an incoming packet with an amount of 100. If the \
                exchange rate is 1:0.5 and the spread is 0.01, the amount on the \
                    outgoing packet would be 198 (instead of 200 without the spread)."),
        Arg::with_name("prometheus.bind_address")
            .long("prometheus.bind_address")
            .takes_value(true)
            .help("IP address and port to host the Prometheus endpoint on."),
        Arg::with_name("prometheus.histogram_window")
            .long("prometheus.histogram_window")
            .takes_value(true)
            .help("Amount of time, in milliseconds, that the node will collect data \
                points for the Prometheus histograms. Defaults to 300000ms (5 minutes)."),
        Arg::with_name("prometheus.histogram_granularity")
            .long("prometheus.histogram_granularity")
            .takes_value(true)
            .help("Granularity, in milliseconds, that the node will use to roll off \
                old data. For example, a value of 1000ms (1 second) would mean that the \
                node forgets the oldest 1 second of histogram data points every second. \
                Defaults to 10000ms (10 seconds)."),
        ]);

    let mut config = get_env_config("ilp");
    if let Ok((path, config_file)) = precheck_arguments(app.clone()) {
        if !is_fd_tty(0) {
            if let Err(error) = merge_std_in(&mut config) {
                output_config_error(error, None);
                return;
            };
        }
        if let Some(ref config_path) = config_file {
            if let Err(error) = merge_config_file(config_path, &mut config) {
                output_config_error(error, Some(config_path));
                return;
            };
        }
        set_app_env(&config, &mut app, &path, path.len());
    }
    let matches = app.clone().get_matches();
    merge_args(&mut config, &matches);

    let node = config
        .try_into::<InterledgerNode>()
        .expect("Could not parse provided configuration options into an Interledger Node config");
    node.serve().await.unwrap();

    // Add a future which is always pending. This will ensure main does not exist
    // TODO: Is there a better way of doing this?
    futures::future::pending().await
}

fn output_config_error(error: ConfigError, config_path: Option<&str>) {
    let is_config_path_ilp_node = match config_path {
        Some(path) => path == "ilp-node",
        None => false,
    };

    match &error {
        ConfigError::PathParse(_) => println!("Error in parsing config: {:?}", error),
        _ if is_config_path_ilp_node => println!("Running ilp-node with `cargo run ilp-node` and \
                    `cargo run -p ilp-node` is deprecated. Please either execute the binary directly, or use \
                    `cargo run --bin ilp-node`"),
        _ => println!("Error: {:?}", error),
    }
}

// returns (subcommand paths, config path)
fn precheck_arguments(mut app: App) -> Result<(Vec<String>, Option<String>), ()> {
    // not to cause `required fields error`.
    reset_required(&mut app);
    let matches = app.get_matches_safe();
    if matches.is_err() {
        // if app could not get any appropriate match, just return not to show help etc.
        return Err(());
    }
    let matches = &matches.unwrap();
    let mut path = Vec::<String>::new();
    let subcommand = get_deepest_command(matches, &mut path);
    let mut config_path: Option<String> = None;
    if let Some(config_path_arg) = subcommand.value_of("config") {
        config_path = Some(config_path_arg.to_string());
    };
    Ok((path, config_path))
}

fn merge_config_file(config_path: &str, config: &mut Config) -> Result<(), ConfigError> {
    let file_config = config::File::with_name(config_path);
    let file_config = file_config.collect()?;
    // if the key is not defined in the given config already, set it to the config
    // because the original values override the ones from the config file
    for (k, v) in file_config {
        if config.get_str(&k).is_err() {
            config.set(&k, v)?;
        }
    }

    Ok(())
}

fn merge_std_in(config: &mut Config) -> Result<(), ConfigError> {
    let stdin = std::io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buf = Vec::new();
    if let Ok(_read) = stdin_lock.read_to_end(&mut buf) {
        if let Ok(buf_str) = String::from_utf8(buf) {
            let config_hash = FileFormat::Json
                .parse(None, &buf_str)
                .or_else(|_| FileFormat::Yaml.parse(None, &buf_str))
                .or_else(|_| FileFormat::Toml.parse(None, &buf_str))
                .ok();
            if let Some(config_hash) = config_hash {
                // if the key is not defined in the given config already, set it to the config
                // because the original values override the ones from the stdin
                for (k, v) in config_hash {
                    if config.get_str(&k).is_err() {
                        config.set(&k, v)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn merge_args(config: &mut Config, matches: &ArgMatches) {
    for (key, value) in &matches.args {
        if config.get_str(key).is_ok() {
            continue;
        }
        if value.vals.is_empty() {
            // flag
            config.set(key, Value::new(None, true)).unwrap();
        } else {
            // value
            config
                .set(key, Value::new(None, value.vals[0].to_str().unwrap()))
                .unwrap();
        }
    }
}

// retrieve Config from a certain prefix
// if the prefix is `ilp`, `address` is resolved to `ilp_address`
fn get_env_config(prefix: &str) -> Config {
    let mut config = Config::new();
    config
        .merge(config::Environment::with_prefix(prefix).separator("__"))
        .unwrap();

    if prefix.to_lowercase() == "ilp" {
        if let Ok(value) = config.get_str("address") {
            config.set("ilp_address", value).unwrap();
        }
    }

    config
}

// This sets the Config values which contains environment variables, config file settings, and STDIN
// settings, into each option's env value which is used when Parser parses the arguments. If this
// value is set, the Parser reads the value from it and doesn't warn even if the argument is not
// given from CLI.
// Usually `env` fn is used when creating `App` but this function automatically fills it so
// we don't need to call `env` fn manually.
fn set_app_env(env_config: &Config, app: &mut App, path: &[String], depth: usize) {
    if depth == 1 {
        for item in &mut app.p.opts {
            if let Ok(value) = env_config.get_str(&item.b.name.to_lowercase()) {
                item.v.env = Some((&OsStr::new(item.b.name), Some(OsString::from(value))));
            }
        }
        return;
    }
    for subcommand in &mut app.p.subcommands {
        if subcommand.get_name() == path[path.len() - depth] {
            set_app_env(env_config, subcommand, path, depth - 1);
        }
    }
}

fn get_deepest_command<'a>(matches: &'a ArgMatches, path: &mut Vec<String>) -> &'a ArgMatches<'a> {
    let (name, subcommand_matches) = matches.subcommand();
    path.push(name.to_string());
    if let Some(matches) = subcommand_matches {
        return get_deepest_command(matches, path);
    }
    matches
}

fn reset_required(app: &mut App) {
    app.p.required.clear();
    for subcommand in &mut app.p.subcommands {
        reset_required(subcommand);
    }
}

// Check whether the file descriptor is pointed to TTY.
// For example, this function could be used to check whether the STDIN (fd: 0) is pointed to TTY.
// We use this function to check if we should read config from STDIN. If STDIN is NOT pointed to
// TTY, we try to read config from STDIN.
fn is_fd_tty(file_descriptor: c_int) -> bool {
    let result: c_int;
    // Because `isatty` is a `libc` function called using FFI, this is unsafe.
    // https://doc.rust-lang.org/book/ch19-01-unsafe-rust.html#using-extern-functions-to-call-external-code
    unsafe {
        result = isatty(file_descriptor);
    }
    result == 1
}
