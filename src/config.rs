use bitcoin::network::constants::Network;
use dirs::home_dir;
use num_cpus;
use std::convert::TryInto;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use stderrlog;

use crate::daemon::CookieGetter;
use crate::errors::*;

//
// Default IP address of the RPC server
//
const DEFAULT_SERVER_ADDRESS: [u8; 4] = [127, 0, 0, 1]; // by default, serve on IPv4 localhost

mod internal {
    #![allow(unused)]
    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

//
// A simple error type representing invalid UTF-8 input.
//
pub struct InvalidUtf8(OsString);

impl fmt::Display for InvalidUtf8 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} isn't a valid UTF-8 sequence", self.0)
    }
}

//
// An error that might happen when resolving an address
//
pub enum AddressError {
    ResolvError { addr: String, err: std::io::Error },
    NoAddrError(String),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AddressError::ResolvError { addr, err } => {
                write!(f, "Failed to resolve address {}: {}", addr, err)
            }
            AddressError::NoAddrError(addr) => write!(f, "No address found for {}", addr),
        }
    }
}

//
// This newtype implements `ParseArg` for `Network`.
//
#[derive(Deserialize)]
pub struct BitcoinNetwork(Network);

impl Default for BitcoinNetwork {
    fn default() -> Self {
        BitcoinNetwork(Network::Bitcoin)
    }
}

impl FromStr for BitcoinNetwork {
    type Err = <Network as FromStr>::Err;

    fn from_str(string: &str) -> std::result::Result<Self, Self::Err> {
        Network::from_str(string).map(BitcoinNetwork)
    }
}

impl ::configure_me::parse_arg::ParseArgFromStr for BitcoinNetwork {
    fn describe_type<W: fmt::Write>(mut writer: W) -> std::fmt::Result {
        write!(writer, "either 'bitcoin', 'testnet' or 'regtest'")
    }
}

impl Into<Network> for BitcoinNetwork {
    fn into(self) -> Network {
        self.0
    }
}

//
// Parsed and post-processed configuration
//
#[derive(Debug)]
pub struct Config {
    // See below for the documentation of each field:
    pub log: stderrlog::StdErrLog,
    pub network_type: Network,
    pub db_path: PathBuf,
    pub daemon_dir: PathBuf,
    pub daemon_rpc_host: Ipv4Addr,
    pub daemon_rpc_port: u16,
    pub cookie: Option<String>,
    pub indexer_rpc_host: Ipv4Addr,
    pub indexer_rpc_port: u16,
    pub jsonrpc_import: bool,
    pub index_batch_size: usize,
    pub bulk_index_threads: usize,
    pub txid_limit: usize,
    pub blocktxids_cache_size: usize,
}

/// Returns default daemon directory
fn default_daemon_dir() -> PathBuf {
    let mut home = home_dir().unwrap_or_else(|| {
        eprintln!("Error: unknown home directory");
        std::process::exit(1)
    });
    home.push(".bitcoin");
    home
}

impl Config {
    /// Parses args, env vars, config files and post-processes them
    pub fn from_args() -> Config {
        use internal::ResultExt;

        let system_config: &OsStr = "/etc/addrindexrs/config.toml".as_ref();

        let home_config = home_dir().map(|mut dir| {
            dir.extend(&[".addrindexrs", "config.toml"]);
            dir
        });

        let cwd_config: &OsStr = "addrindexrs.toml".as_ref();
        let configs = std::iter::once(cwd_config)
            .chain(home_config.as_ref().map(AsRef::as_ref))
            .chain(std::iter::once(system_config));

        let (mut config, _) =
            internal::Config::including_optional_config_files(configs).unwrap_or_exit();

        let db_subdir = match config.network {
            // We must keep the name "mainnet" due to backwards compatibility
            Network::Bitcoin => "mainnet",
            Network::Testnet => "testnet",
            Network::Regtest => "regtest",
        };

        config.db_dir.push(db_subdir);

        let default_daemon_port = match config.network {
            Network::Bitcoin => 8332,
            Network::Testnet => 18332,
            Network::Regtest => 18443,
        };

        let default_indexer_port = match config.network {
            Network::Bitcoin => 8432,
            Network::Testnet => 18432,
            Network::Regtest => 18543,
        };

        let daemon_rpc_host = config
            .daemon_rpc_host
            .unwrap_or(DEFAULT_SERVER_ADDRESS.into());
        let daemon_rpc_port = config.daemon_rpc_port.unwrap_or(default_daemon_port);

        let indexer_rpc_host = config
            .indexer_rpc_host
            .unwrap_or(DEFAULT_SERVER_ADDRESS.into());
        let indexer_rpc_port = config.indexer_rpc_port.unwrap_or(default_indexer_port);

        match config.network {
            Network::Bitcoin => (),
            Network::Testnet => config.daemon_dir.push("testnet3"),
            Network::Regtest => config.daemon_dir.push("regtest"),
        }

        let mut log = stderrlog::new();
        log.verbosity(
            config
                .verbose
                .try_into()
                .expect("Overflow: Running addrindexrs on less than 32 bit devices is unsupported"),
        );

        log.timestamp(if config.timestamp {
            stderrlog::Timestamp::Millisecond
        } else {
            stderrlog::Timestamp::Off
        });

        log.init().unwrap_or_else(|err| {
            eprintln!("Error: logging initialization failed: {}", err);
            std::process::exit(1)
        });

        // Could have been default, but it's useful to allow the user to specify 0 when overriding
        // configs.
        if config.bulk_index_threads == 0 {
            config.bulk_index_threads = num_cpus::get();
        }

        const MB: f32 = (1 << 20) as f32;

        let config = Config {
            log,
            network_type: config.network,
            db_path: config.db_dir,
            daemon_dir: config.daemon_dir,
            daemon_rpc_host,
            daemon_rpc_port,
            indexer_rpc_host,
            indexer_rpc_port,
            cookie: config.cookie,
            jsonrpc_import: config.jsonrpc_import,
            index_batch_size: config.index_batch_size,
            bulk_index_threads: config.bulk_index_threads,
            blocktxids_cache_size: (config.blocktxids_cache_size_mb * MB) as usize,
            txid_limit: config.txid_limit,
        };

        eprintln!("{:#?}", config);
        config
    }

    pub fn cookie_getter(&self) -> Arc<dyn CookieGetter> {
        if let Some(ref value) = self.cookie {
            Arc::new(StaticCookie {
                value: value.as_bytes().to_vec(),
            })
        } else {
            Arc::new(CookieFile {
                daemon_dir: self.daemon_dir.clone(),
            })
        }
    }
}

//
// Auth cookie for bitcoind
//
struct StaticCookie {
    value: Vec<u8>,
}

impl CookieGetter for StaticCookie {
    fn get(&self) -> Result<Vec<u8>> {
        Ok(self.value.clone())
    }
}

struct CookieFile {
    daemon_dir: PathBuf,
}

impl CookieGetter for CookieFile {
    fn get(&self) -> Result<Vec<u8>> {
        let path = self.daemon_dir.join(".cookie");
        let contents = fs::read(&path).chain_err(|| {
            ErrorKind::Connection(format!("failed to read cookie from {:?}", path))
        })?;
        Ok(contents)
    }
}
