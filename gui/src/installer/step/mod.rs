mod descriptor;
mod mnemonic;

pub use descriptor::{
    BackupDescriptor, DefineDescriptor, ImportDescriptor, ParticipateXpub, RegisterDescriptor,
};

pub use mnemonic::{BackupMnemonic, RecoverMnemonic};

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use iced::Command;
use liana::{
    config::BitcoindConfig,
    miniscript::bitcoin::{bip32::Fingerprint, Network},
};

use tracing::info;

use jsonrpc::{client::Client, simple_http::SimpleHttpTransport};

use liana_ui::{component::form, widget::*};

use crate::{
    bitcoind::{start_internal_bitcoind, stop_internal_bitcoind, StartInternalBitcoindError},
    installer::{
        context::Context,
        internal_bitcoind_datadir,
        message::{self, Message},
        view, Error, InternalBitcoindExeConfig,
    },
    utils::poll_for_file,
};

pub trait Step {
    fn update(&mut self, _message: Message) -> Command<Message> {
        Command::none()
    }
    fn view(&self, progress: (usize, usize)) -> Element<Message>;
    fn load_context(&mut self, _ctx: &Context) {}
    fn load(&self) -> Command<Message> {
        Command::none()
    }
    fn skip(&self, _ctx: &Context) -> bool {
        false
    }
    fn apply(&mut self, _ctx: &mut Context) -> bool {
        true
    }
    fn stop(&self) {}
}

#[derive(Default)]
pub struct Welcome {}

impl Step for Welcome {
    fn view(&self, _progress: (usize, usize)) -> Element<Message> {
        view::welcome()
    }
}

impl From<Welcome> for Box<dyn Step> {
    fn from(s: Welcome) -> Box<dyn Step> {
        Box::new(s)
    }
}

pub struct DefineBitcoind {
    cookie_path: form::Value<String>,
    address: form::Value<String>,
    is_running: Option<Result<(), Error>>,
}

pub struct InternalBitcoindStep {
    bitcoind_datadir: PathBuf,
    network: Network,
    started: Option<Result<(), StartInternalBitcoindError>>,
    exe_path: Option<PathBuf>,
    bitcoind_config: Option<BitcoindConfig>,
    exe_config: Option<InternalBitcoindExeConfig>,
    internal_bitcoind_config: Option<InternalBitcoindConfig>,
    error: Option<String>,
}

pub struct SelectBitcoindTypeStep {
    use_external: bool,
}

/// Default prune value used by internal bitcoind.
pub const PRUNE_DEFAULT: u32 = 15_000;
/// Default ports used by bitcoind across all networks.
pub const BITCOIND_DEFAULT_PORTS: [u16; 8] = [8332, 8333, 18332, 18333, 18443, 18444, 38332, 38333];

/// Represents section for a single network in `bitcoin.conf` file.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct InternalBitcoindNetworkConfig {
    rpc_port: u16,
    p2p_port: u16,
    prune: u32,
}

/// Represents the `bitcoin.conf` file to be used by internal bitcoind.
#[derive(Debug, Clone)]
pub struct InternalBitcoindConfig {
    networks: BTreeMap<Network, InternalBitcoindNetworkConfig>,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum InternalBitcoindConfigError {
    KeyNotFound(String),
    CouldNotParseValue(String),
    UnexpectedSection(String),
    TooManyElements(String),
    FileNotFound,
    ReadingFile(String),
    WritingFile(String),
    Unexpected(String),
}

impl std::fmt::Display for InternalBitcoindConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::KeyNotFound(e) => write!(f, "Config file does not contain expected key: {}", e),
            Self::CouldNotParseValue(e) => write!(f, "Value could not be parsed: {}", e),
            Self::UnexpectedSection(e) => write!(f, "Unexpected section in file: {}", e),
            Self::TooManyElements(section) => {
                write!(f, "Section in file contains too many elements: {}", section)
            }
            Self::FileNotFound => write!(f, "File not found"),
            Self::ReadingFile(e) => write!(f, "Error while reading file: {}", e),
            Self::WritingFile(e) => write!(f, "Error while writing file: {}", e),
            Self::Unexpected(e) => write!(f, "Unexpected error: {}", e),
        }
    }
}

impl Default for InternalBitcoindConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl InternalBitcoindConfig {
    pub fn new() -> Self {
        Self {
            networks: BTreeMap::new(),
        }
    }

    pub fn from_ini(ini: &ini::Ini) -> Result<Self, InternalBitcoindConfigError> {
        let mut networks = BTreeMap::new();
        for (maybe_sec, prop) in ini {
            if let Some(sec) = maybe_sec {
                let network = Network::from_core_arg(sec)
                    .map_err(|e| InternalBitcoindConfigError::UnexpectedSection(e.to_string()))?;
                if prop.len() > 3 {
                    return Err(InternalBitcoindConfigError::TooManyElements(
                        sec.to_string(),
                    ));
                }
                let rpc_port = prop
                    .get("rpcport")
                    .ok_or_else(|| InternalBitcoindConfigError::KeyNotFound("rpcport".to_string()))?
                    .parse::<u16>()
                    .map_err(|e| InternalBitcoindConfigError::CouldNotParseValue(e.to_string()))?;
                let p2p_port = prop
                    .get("port")
                    .ok_or_else(|| InternalBitcoindConfigError::KeyNotFound("port".to_string()))?
                    .parse::<u16>()
                    .map_err(|e| InternalBitcoindConfigError::CouldNotParseValue(e.to_string()))?;
                let prune = prop
                    .get("prune")
                    .ok_or_else(|| InternalBitcoindConfigError::KeyNotFound("prune".to_string()))?
                    .parse::<u32>()
                    .map_err(|e| InternalBitcoindConfigError::CouldNotParseValue(e.to_string()))?;
                networks.insert(
                    network,
                    InternalBitcoindNetworkConfig {
                        rpc_port,
                        p2p_port,
                        prune,
                    },
                );
            } else if !prop.is_empty() {
                return Err(InternalBitcoindConfigError::UnexpectedSection(
                    "General section should be empty".to_string(),
                ));
            }
        }
        Ok(Self { networks })
    }

    pub fn from_file(path: &PathBuf) -> Result<Self, InternalBitcoindConfigError> {
        if !path.exists() {
            return Err(InternalBitcoindConfigError::FileNotFound);
        }
        let conf_ini = ini::Ini::load_from_file(path)
            .map_err(|e| InternalBitcoindConfigError::ReadingFile(e.to_string()))?;

        Self::from_ini(&conf_ini)
    }

    pub fn to_ini(&self) -> ini::Ini {
        let mut conf_ini = ini::Ini::new();

        for (network, network_conf) in &self.networks {
            conf_ini
                .with_section(Some(network.to_core_arg()))
                .set("rpcport", network_conf.rpc_port.to_string())
                .set("port", network_conf.p2p_port.to_string())
                .set("prune", network_conf.prune.to_string());
        }
        conf_ini
    }

    pub fn to_file(&self, path: &PathBuf) -> Result<(), InternalBitcoindConfigError> {
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| InternalBitcoindConfigError::Unexpected("No parent".to_string()))?,
        )
        .map_err(|e| InternalBitcoindConfigError::Unexpected(e.to_string()))?;
        info!("Writing to file {}", path.to_string_lossy());
        self.to_ini()
            .write_to_file(path)
            .map_err(|e| InternalBitcoindConfigError::WritingFile(e.to_string()))?;

        Ok(())
    }
}

/// Path of the `bitcoin.conf` file used by internal bitcoind.
fn internal_bitcoind_config_path(bitcoind_datadir: &PathBuf) -> PathBuf {
    let mut config_path = PathBuf::from(bitcoind_datadir);
    config_path.push("bitcoin.conf");
    config_path
}

/// Path of the cookie file used by internal bitcoind on a given network.
fn internal_bitcoind_cookie_path(bitcoind_datadir: &Path, network: &Network) -> PathBuf {
    let mut cookie_path = bitcoind_datadir.to_path_buf();
    if let Some(dir) = bitcoind_network_dir(network) {
        cookie_path.push(dir);
    }
    cookie_path.push(".cookie");
    cookie_path
}

/// RPC address for internal bitcoind.
fn internal_bitcoind_address(rpc_port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), rpc_port)
}

fn bitcoind_default_datadir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let configs_dir = dirs::home_dir();

    #[cfg(not(target_os = "linux"))]
    let configs_dir = dirs::config_dir();

    if let Some(mut path) = configs_dir {
        #[cfg(target_os = "linux")]
        path.push(".bitcoin");

        #[cfg(not(target_os = "linux"))]
        path.push("Bitcoin");

        return Some(path);
    }
    None
}

fn bitcoind_network_dir(network: &Network) -> Option<String> {
    let dir = match network {
        Network::Bitcoin => {
            return None;
        }
        Network::Testnet => "testnet3",
        Network::Regtest => "regtest",
        Network::Signet => "signet",
        _ => panic!("Directory required for this network is unknown."),
    };
    Some(dir.to_string())
}

fn bitcoind_default_cookie_path(network: &Network) -> Option<String> {
    if let Some(mut path) = bitcoind_default_datadir() {
        if let Some(dir) = bitcoind_network_dir(network) {
            path.push(dir);
        }
        path.push(".cookie");
        return path.to_str().map(|s| s.to_string());
    }
    None
}

fn bitcoind_default_address(network: &Network) -> String {
    match network {
        Network::Bitcoin => "127.0.0.1:8332".to_string(),
        Network::Testnet => "127.0.0.1:18332".to_string(),
        Network::Regtest => "127.0.0.1:18443".to_string(),
        Network::Signet => "127.0.0.1:38332".to_string(),
        _ => "127.0.0.1:8332".to_string(),
    }
}

/// Looks for bitcoind executable path and returns `None` if not found.
fn bitcoind_exe_path() -> Option<PathBuf> {
    which::which("bitcoind").ok()
}

/// Get available port that is valid for use by internal bitcoind.
// Modified from https://github.com/RCasatta/bitcoind/blob/f047740d7d0af935ff7360cf77429c5f294cfd59/src/lib.rs#L435
pub fn get_available_port() -> Result<u16, Error> {
    // Perform multiple attempts to get a valid port.
    for _ in 0..10 {
        // Using 0 as port lets the system assign a port available.
        let t = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| Error::CannotGetAvailablePort(e.to_string()))?;
        let port = t
            .local_addr()
            .map(|s| s.port())
            .map_err(|e| Error::CannotGetAvailablePort(e.to_string()))?;
        if port_is_valid(&port) {
            return Ok(port);
        }
    }
    Err(Error::CannotGetAvailablePort(
        "Exhausted attempts".to_string(),
    ))
}

/// Checks if port is valid for use by internal bitcoind.
pub fn port_is_valid(port: &u16) -> bool {
    !BITCOIND_DEFAULT_PORTS.contains(port)
}

impl Default for SelectBitcoindTypeStep {
    fn default() -> Self {
        Self::new()
    }
}

impl From<SelectBitcoindTypeStep> for Box<dyn Step> {
    fn from(s: SelectBitcoindTypeStep) -> Box<dyn Step> {
        Box::new(s)
    }
}

impl SelectBitcoindTypeStep {
    pub fn new() -> Self {
        Self { use_external: true }
    }
}

impl Step for SelectBitcoindTypeStep {
    fn update(&mut self, message: Message) -> Command<Message> {
        if let Message::SelectBitcoindType(msg) = message {
            match msg {
                message::SelectBitcoindTypeMsg::UseExternal(selected) => {
                    self.use_external = selected;
                }
            };
            return Command::perform(async {}, |_| Message::Next);
        };
        Command::none()
    }

    fn apply(&mut self, ctx: &mut Context) -> bool {
        if !self.use_external {
            if ctx.internal_bitcoind_config.is_none() {
                ctx.bitcoind_config = None; // Ensures internal bitcoind can be restarted in case user has switched selection.
            }
        } else {
            ctx.internal_bitcoind_config = None;
            ctx.internal_bitcoind_exe_config = None;
        }
        ctx.bitcoind_is_external = self.use_external;
        true
    }

    fn view(&self, progress: (usize, usize)) -> Element<Message> {
        view::select_bitcoind_type(progress)
    }
}

impl DefineBitcoind {
    pub fn new() -> Self {
        Self {
            cookie_path: form::Value::default(),
            address: form::Value::default(),
            is_running: None,
        }
    }

    pub fn ping(&self) -> Command<Message> {
        let address = self.address.value.to_owned();
        let cookie_path = self.cookie_path.value.to_owned();
        Command::perform(
            async move {
                let cookie = std::fs::read_to_string(&cookie_path)
                    .map_err(|e| Error::Bitcoind(format!("Failed to read cookie file: {}", e)))?;
                let client = Client::with_transport(
                    SimpleHttpTransport::builder()
                        .url(&address)?
                        .timeout(std::time::Duration::from_secs(3))
                        .cookie_auth(cookie)
                        .build(),
                );
                client.send_request(client.build_request("echo", &[]))?;
                Ok(())
            },
            |res| Message::DefineBitcoind(message::DefineBitcoind::PingBitcoindResult(res)),
        )
    }
}

impl Step for DefineBitcoind {
    fn load_context(&mut self, ctx: &Context) {
        if self.cookie_path.value.is_empty() {
            self.cookie_path.value =
                bitcoind_default_cookie_path(&ctx.bitcoin_config.network).unwrap_or_default()
        }
        if self.address.value.is_empty() {
            self.address.value = bitcoind_default_address(&ctx.bitcoin_config.network);
        }
    }
    fn update(&mut self, message: Message) -> Command<Message> {
        if let Message::DefineBitcoind(msg) = message {
            match msg {
                message::DefineBitcoind::PingBitcoind => {
                    self.is_running = None;
                    return self.ping();
                }
                message::DefineBitcoind::PingBitcoindResult(res) => self.is_running = Some(res),
                message::DefineBitcoind::AddressEdited(address) => {
                    self.is_running = None;
                    self.address.value = address;
                    self.address.valid = true;
                }
                message::DefineBitcoind::CookiePathEdited(path) => {
                    self.is_running = None;
                    self.cookie_path.value = path;
                    self.address.valid = true;
                }
            };
        };
        Command::none()
    }

    fn apply(&mut self, ctx: &mut Context) -> bool {
        match (
            PathBuf::from_str(&self.cookie_path.value),
            std::net::SocketAddr::from_str(&self.address.value),
        ) {
            (Err(_), Ok(_)) => {
                self.cookie_path.valid = false;
                false
            }
            (Ok(_), Err(_)) => {
                self.address.valid = false;
                false
            }
            (Err(_), Err(_)) => {
                self.cookie_path.valid = false;
                self.address.valid = false;
                false
            }
            (Ok(path), Ok(addr)) => {
                ctx.bitcoind_config = Some(BitcoindConfig {
                    cookie_path: path,
                    addr,
                });
                true
            }
        }
    }

    fn view(&self, progress: (usize, usize)) -> Element<Message> {
        view::define_bitcoin(
            progress,
            &self.address,
            &self.cookie_path,
            self.is_running.as_ref(),
        )
    }

    fn load(&self) -> Command<Message> {
        self.ping()
    }

    fn skip(&self, ctx: &Context) -> bool {
        !ctx.bitcoind_is_external
    }
}

impl Default for DefineBitcoind {
    fn default() -> Self {
        Self::new()
    }
}

impl From<DefineBitcoind> for Box<dyn Step> {
    fn from(s: DefineBitcoind) -> Box<dyn Step> {
        Box::new(s)
    }
}

impl From<InternalBitcoindStep> for Box<dyn Step> {
    fn from(s: InternalBitcoindStep) -> Box<dyn Step> {
        Box::new(s)
    }
}

impl InternalBitcoindStep {
    pub fn new(liana_datadir: &PathBuf) -> Self {
        Self {
            bitcoind_datadir: internal_bitcoind_datadir(liana_datadir),
            network: Network::Bitcoin,
            started: None,
            exe_path: None,
            bitcoind_config: None,
            exe_config: None,
            internal_bitcoind_config: None,
            error: None,
        }
    }
}

impl Step for InternalBitcoindStep {
    fn load_context(&mut self, ctx: &Context) {
        if self.exe_path.is_none() {
            self.exe_path = if let Some(exe_config) = ctx.internal_bitcoind_exe_config.clone() {
                Some(exe_config.exe_path)
            } else {
                bitcoind_exe_path()
            };
        }
        self.network = ctx.bitcoin_config.network;
        if let Some(Ok(_)) = self.started {
            // This case can arise if a user switches from internal bitcoind to external and back to internal.
            if ctx.bitcoind_config.is_none() {
                self.started = None; // So that internal bitcoind will be restarted.
            }
        }
    }
    fn update(&mut self, message: Message) -> Command<Message> {
        if let Message::InternalBitcoind(msg) = message {
            match msg {
                message::InternalBitcoindMsg::Previous => {
                    if self.internal_bitcoind_config.is_some() {
                        if let Some(bitcoind_config) = &self.bitcoind_config {
                            stop_internal_bitcoind(bitcoind_config);
                        }
                    }
                    return Command::perform(async {}, |_| Message::Previous);
                }
                message::InternalBitcoindMsg::Reload => {
                    return self.load();
                }
                message::InternalBitcoindMsg::DefineConfig => {
                    let mut conf = match InternalBitcoindConfig::from_file(
                        &internal_bitcoind_config_path(&self.bitcoind_datadir),
                    ) {
                        Ok(conf) => conf,
                        Err(InternalBitcoindConfigError::FileNotFound) => {
                            InternalBitcoindConfig::new()
                        }
                        Err(e) => {
                            self.error = Some(e.to_string());
                            return Command::none();
                        }
                    };
                    // Insert entry for network if not present.
                    if conf.networks.get(&self.network).is_none() {
                        let network_conf = match (get_available_port(), get_available_port()) {
                            (Ok(rpc_port), Ok(p2p_port)) => {
                                // In case ports are the same, user will need to click button again for another attempt.
                                if rpc_port == p2p_port {
                                    self.error = Some(
                                        "Could not get distinct ports. Please try again."
                                            .to_string(),
                                    );
                                    return Command::none();
                                }
                                InternalBitcoindNetworkConfig {
                                    rpc_port,
                                    p2p_port,
                                    prune: PRUNE_DEFAULT,
                                }
                            }
                            (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
                                self.error = Some(format!("Could not get available port: {}.", e));
                                return Command::none();
                            }
                            (Err(e1), Err(e2)) => {
                                self.error =
                                    Some(format!("Could not get available ports: {}; {}.", e1, e2));
                                return Command::none();
                            }
                        };
                        conf.networks.insert(self.network, network_conf);
                    }
                    if let Err(e) =
                        conf.to_file(&internal_bitcoind_config_path(&self.bitcoind_datadir))
                    {
                        self.error = Some(e.to_string());
                        return Command::none();
                    };
                    self.error = None;
                    self.internal_bitcoind_config = Some(conf.clone());
                    return Command::perform(async {}, |_| {
                        Message::InternalBitcoind(message::InternalBitcoindMsg::Reload)
                    });
                }
                message::InternalBitcoindMsg::Start => {
                    if let Some(path) = &self.exe_path {
                        let datadir = match self.bitcoind_datadir.canonicalize() {
                            Ok(datadir) => datadir,
                            Err(e) => {
                                self.started = Some(Err(
                                    StartInternalBitcoindError::CouldNotCanonicalizeDataDir(
                                        e.to_string(),
                                    ),
                                ));
                                return Command::none();
                            }
                        };
                        let exe_config = InternalBitcoindExeConfig {
                            exe_path: path.to_path_buf(),
                            data_dir: datadir,
                        };
                        if let Err(e) = start_internal_bitcoind(&self.network, exe_config.clone()) {
                            self.started =
                                Some(Err(StartInternalBitcoindError::CommandError(e.to_string())));
                            return Command::none();
                        }
                        // Need to wait for cookie file to appear.
                        let cookie_path =
                            internal_bitcoind_cookie_path(&self.bitcoind_datadir, &self.network);
                        if !poll_for_file(&cookie_path, 200, 15) {
                            self.started =
                                Some(Err(StartInternalBitcoindError::CookieFileNotFound(
                                    cookie_path.to_string_lossy().into_owned(),
                                )));
                            return Command::none();
                        }
                        let rpc_port = self
                            .internal_bitcoind_config
                            .as_ref()
                            .expect("Already added")
                            .clone()
                            .networks
                            .get(&self.network)
                            .expect("Already added")
                            .rpc_port;
                        let bitcoind_config = match cookie_path.canonicalize() {
                            Ok(cookie_path) => BitcoindConfig {
                                cookie_path,
                                addr: internal_bitcoind_address(rpc_port),
                            },
                            Err(e) => {
                                self.started = Some(Err(
                                    StartInternalBitcoindError::CouldNotCanonicalizeCookiePath(
                                        e.to_string(),
                                    ),
                                ));
                                return Command::none();
                            }
                        };
                        match liana::BitcoinD::new(
                            &bitcoind_config,
                            "internal_bitcoind_connection_check".to_string(),
                        ) {
                            Ok(_) => {
                                self.error = None;
                                self.bitcoind_config = Some(bitcoind_config);
                                self.exe_config = Some(exe_config);
                                self.started = Some(Ok(()));
                            }
                            Err(e) => {
                                self.started = Some(Err(
                                    StartInternalBitcoindError::BitcoinDError(e.to_string()),
                                ));
                            }
                        }
                    }
                }
            };
        };
        Command::none()
    }

    fn load(&self) -> Command<Message> {
        if self.internal_bitcoind_config.is_none() {
            return Command::perform(async {}, |_| {
                Message::InternalBitcoind(message::InternalBitcoindMsg::DefineConfig)
            });
        }
        if self.started.is_none() {
            return Command::perform(async {}, |_| {
                Message::InternalBitcoind(message::InternalBitcoindMsg::Start)
            });
        }
        Command::none()
    }

    fn apply(&mut self, ctx: &mut Context) -> bool {
        // Any errors have been handled as part of `message::InternalBitcoindMsg::Start`
        if let Some(Ok(_)) = self.started {
            ctx.bitcoind_config = self.bitcoind_config.clone();
            ctx.internal_bitcoind_config = self.internal_bitcoind_config.clone();
            ctx.internal_bitcoind_exe_config = self.exe_config.clone();
            self.error = None;
            return true;
        }
        false
    }

    fn view(&self, progress: (usize, usize)) -> Element<Message> {
        view::start_internal_bitcoind(
            progress,
            self.exe_path.as_ref(),
            self.started.as_ref(),
            self.error.as_ref(),
        )
    }

    fn stop(&self) {
        // In case the installer is closed before changes written to context, stop bitcoind.
        if let Some(Ok(_)) = self.started {
            if let Some(bitcoind_config) = &self.bitcoind_config {
                stop_internal_bitcoind(bitcoind_config);
            }
        }
    }

    fn skip(&self, ctx: &Context) -> bool {
        ctx.bitcoind_is_external
    }
}

pub struct Final {
    generating: bool,
    context: Option<Context>,
    warning: Option<String>,
    config_path: Option<PathBuf>,
    hot_signer_fingerprint: Fingerprint,
    hot_signer_is_not_used: bool,
}

impl Final {
    pub fn new(hot_signer_fingerprint: Fingerprint) -> Self {
        Self {
            context: None,
            generating: false,
            warning: None,
            config_path: None,
            hot_signer_fingerprint,
            hot_signer_is_not_used: false,
        }
    }
}

impl Step for Final {
    fn load_context(&mut self, ctx: &Context) {
        self.context = Some(ctx.clone());
        if let Some(signer) = &ctx.recovered_signer {
            self.hot_signer_fingerprint = signer.fingerprint();
            self.hot_signer_is_not_used = false;
        } else if ctx
            .descriptor
            .as_ref()
            .unwrap()
            .to_string()
            .contains(&self.hot_signer_fingerprint.to_string())
        {
            self.hot_signer_is_not_used = false;
        } else {
            self.hot_signer_is_not_used = true;
        }
    }
    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::Installed(res) => {
                self.generating = false;
                match res {
                    Err(e) => {
                        self.config_path = None;
                        self.warning = Some(e.to_string());
                    }
                    Ok(path) => self.config_path = Some(path),
                }
            }
            Message::Install => {
                self.generating = true;
                self.config_path = None;
                self.warning = None;
            }
            _ => {}
        };
        Command::none()
    }

    fn view(&self, progress: (usize, usize)) -> Element<Message> {
        let ctx = self.context.as_ref().unwrap();
        let desc = ctx.descriptor.as_ref().unwrap().to_string();
        view::install(
            progress,
            ctx,
            desc,
            self.generating,
            self.config_path.as_ref(),
            self.warning.as_ref(),
            if self.hot_signer_is_not_used {
                None
            } else {
                Some(self.hot_signer_fingerprint)
            },
        )
    }
}

impl From<Final> for Box<dyn Step> {
    fn from(s: Final) -> Box<dyn Step> {
        Box::new(s)
    }
}

#[cfg(test)]
mod tests {
    use crate::installer::step::{InternalBitcoindConfig, InternalBitcoindNetworkConfig};
    use ini::Ini;
    use liana::miniscript::bitcoin::Network;

    // Test the format of the internal bitcoind configuration file.
    #[test]
    fn internal_bitcoind_config() {
        // A valid config
        let mut conf_ini = Ini::new();
        conf_ini
            .with_section(Some("main"))
            .set("rpcport", "43345")
            .set("port", "42355")
            .set("prune", "15246");
        conf_ini
            .with_section(Some("regtest"))
            .set("rpcport", "34067")
            .set("port", "45175")
            .set("prune", "2043");
        let conf = InternalBitcoindConfig::from_ini(&conf_ini).expect("Loading conf from ini");
        let main_conf = InternalBitcoindNetworkConfig {
            rpc_port: 43345,
            p2p_port: 42355,
            prune: 15246,
        };
        let regtest_conf = InternalBitcoindNetworkConfig {
            rpc_port: 34067,
            p2p_port: 45175,
            prune: 2043,
        };
        assert_eq!(conf.networks.len(), 2);
        assert_eq!(
            conf.networks.get(&Network::Bitcoin).expect("Missing main"),
            &main_conf
        );
        assert_eq!(
            conf.networks
                .get(&Network::Regtest)
                .expect("Missing regtest"),
            &regtest_conf
        );

        let mut conf = InternalBitcoindConfig::new();
        conf.networks.insert(Network::Bitcoin, main_conf);
        conf.networks.insert(Network::Regtest, regtest_conf);
        for (sec, prop) in &conf.to_ini() {
            if let Some(sec) = sec {
                assert_eq!(prop.len(), 3);
                let rpc_port = prop.get("rpcport").expect("rpcport");
                let p2p_port = prop.get("port").expect("port");
                let prune = prop.get("prune").expect("prune");
                if sec == "main" {
                    assert_eq!(rpc_port, "43345");
                    assert_eq!(p2p_port, "42355");
                    assert_eq!(prune, "15246");
                } else if sec == "regtest" {
                    assert_eq!(rpc_port, "34067");
                    assert_eq!(p2p_port, "45175");
                    assert_eq!(prune, "2043");
                } else {
                    panic!("Unexpected section");
                }
            } else {
                assert!(prop.is_empty())
            }
        }
    }
}
