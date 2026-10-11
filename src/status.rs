// State shared between the BeatKeeper thread and the GUI: live deck info for
// display, the log, and the OSC target, which the GUI can change while running.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const LOG_LINES: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OscMode {
    /// Send to 127.0.0.1: Rekordbox and the receiver run on this machine
    Local,
    /// Send to a LAN address, normally the subnet broadcast
    Lan,
}

impl OscMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "local" => Some(OscMode::Local),
            "lan" | "network" => Some(OscMode::Lan),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            OscMode::Local => "local",
            OscMode::Lan => "lan",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OscTarget {
    pub mode: OscMode,
    /// Receiver port used in local mode
    pub port: u16,
    /// host:port used in LAN mode
    pub lan_destination: String,
}

impl OscTarget {
    pub fn resolve(&self) -> Result<SocketAddr, String> {
        let addr = match self.mode {
            OscMode::Local => format!("127.0.0.1:{}", self.port),
            OscMode::Lan => self.lan_destination.trim().to_string(),
        };
        addr.to_socket_addrs()
            .map_err(|e| format!("Invalid OSC target '{addr}': {e}"))?
            .find(|a| a.is_ipv4())
            .ok_or_else(|| format!("No IPv4 address for OSC target '{addr}'"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Debug,
    Good,
    Info,
    Warning,
    Error,
}

#[derive(Default, Clone)]
pub struct DeckStatus {
    pub rekordbox_connected: bool,
    pub target_version: String,
    pub title: String,
    pub artist: String,
    pub bpm: f32,
    pub beat: f32,
    pub phrase: String,
    pub next_phrase: String,
    pub countin: i32,
    pub last_osc_error: String,
}

pub struct Shared {
    pub status: Mutex<DeckStatus>,
    pub log: Mutex<VecDeque<(LogKind, String)>>,
    osc_target: Mutex<Option<OscTarget>>,
    /// Bumped on every target change so the OSC module knows to re-resolve
    pub osc_target_generation: AtomicU64,
    pub packets_sent: AtomicU64,
    /// Set by the GUI to re-send the cached track, tempo and phrase now
    pub resend_requested: AtomicBool,
}

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

pub fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            status: Mutex::new(DeckStatus::default()),
            log: Mutex::new(VecDeque::with_capacity(LOG_LINES)),
            osc_target: Mutex::new(None),
            osc_target_generation: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
            resend_requested: AtomicBool::new(false),
        })
    })
}

impl Shared {
    pub fn push_log(&self, kind: LogKind, line: String) {
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= LOG_LINES {
                log.pop_front();
            }
            log.push_back((kind, line));
        }
    }

    pub fn osc_target(&self) -> Option<OscTarget> {
        self.osc_target.lock().ok().and_then(|t| t.clone())
    }

    pub fn set_osc_target(&self, target: OscTarget) {
        if let Ok(mut t) = self.osc_target.lock() {
            *t = Some(target);
        }
        self.osc_target_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn update_status(&self, f: impl FnOnce(&mut DeckStatus)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }
}
