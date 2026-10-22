use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rosc::{decoder::decode_udp, encoder::encode, OscMessage, OscPacket};

use crate::{
    beatkeeper::TrackInfo,
    config::Config,
    log::ScopedLogger,
    status::{shared, OscMode, OscTarget},
    utils::PhraseParser,
};

use super::{ModuleCreateOutput, OutputModule};

enum OutputFormat{
    String,
    Int,
    Float
}

impl OutputFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "string" => Some(OutputFormat::String),
            "int" => Some(OutputFormat::Int),
            "float" => Some(OutputFormat::Float),
            _ => None,
        }
    }
}

struct MessageToggles{
    /*beat: bool,
    beat_master: bool,*/


    beat_subdivs: Vec<f32>,
    beat_master_subdivs: Vec<f32>,
    beat_triggers: Vec<f32>,
    beat_master_triggers: Vec<f32>,

    beat_trigger_autorelease: bool,
    time: bool,
    time_master: bool,
    phrase: bool,
    phrase_master: bool,
    phrase_output_format: OutputFormat,
}


impl MessageToggles{
    fn new(conf: &Config, logger: ScopedLogger) -> Self{
        let mut subdivs = ["msg.n/beat/subdiv", "msg.n/beat/trigger", "msg.master/beat/subdiv", "msg.master/beat/trigger"].iter().map(|conf_key|{
            conf.get_or_default(conf_key, String::new()).split(",").filter_map(|x|{
                if x.is_empty(){
                    return None;
                }
                if let Ok(val) = x.trim().parse::<f32>(){
                    Some(val)
                }else{
                    logger.err(&format!("Error parsing value '{x}' in key {conf_key}"));
                    None
                }
            }).collect()
        });

        MessageToggles { 
            /*beat: conf.get_or_default("msg.n/beat", false),
            beat_master: conf.get_or_default("msg.master/beat", false),*/


            beat_subdivs: subdivs.next().unwrap(),
            beat_triggers: subdivs.next().unwrap(),
            beat_master_subdivs: subdivs.next().unwrap(),
            beat_master_triggers: subdivs.next().unwrap(),

            beat_trigger_autorelease: conf.get_or_default("trigger_autorelease", false),
            time: conf.get_or_default("msg.n/time", false), 
            time_master: conf.get_or_default("msg.master/time", true), 
            phrase: conf.get_or_default("msg.n/phrase", false), 
            phrase_master:  conf.get_or_default("msg.master/phrase", true),
            phrase_output_format: {
                let fmt = conf.get_or_default("phrase_output_format", "string".to_string());
                match OutputFormat::from_str(&fmt) {
                    Some(format) => format,
                    None => {
                        logger.err(&format!("Unknown phrase output format: {fmt}"));
                        OutputFormat::String
                    }
                }
            }
        }
    } 
}

/// Last master values, re-sent by the heartbeat so a receiver that starts late
/// (or restarts) learns the track, tempo and phrase without waiting for a change
#[derive(Default)]
struct MasterCache {
    bpm: Option<f32>,
    original_bpm: Option<f32>,
    track: Option<TrackInfo>,
    phrase: Option<String>,
    next_phrase: Option<String>,
    countin: Option<i32>,
}

pub struct Osc {
    socket: UdpSocket,
    target: Option<SocketAddr>,
    target_generation: u64,
    info_sent: bool,
    logger: ScopedLogger,
    message_toggles: MessageToggles,
    send_period: i32,
    send_period_counter: i32,
    last_beat_master: f32,
    last_beats: Vec<f32>,
    heartbeat: Option<Duration>,
    last_heartbeat: Instant,
    cache: MasterCache,
    last_alive: Option<Instant>,
}




impl Osc {
    fn send_float(&self, addr: &str, value: f32) {
        let msg = OscPacket::Message(OscMessage {
            addr: addr.to_string(),
            args: vec![rosc::OscType::Float(value)],
        });
        self.send(msg);
    }

    fn send_string(&self, addr: &str, value: &str) {
        let msg = OscPacket::Message(OscMessage {
            addr: addr.to_string(),
            args: vec![rosc::OscType::String(value.to_string())],
        });
        self.send(msg);
    }

    fn send_int(&self, addr: &str, value: i32) {
        let msg = OscPacket::Message(OscMessage {
            addr: addr.to_string(),
            args: vec![rosc::OscType::Int(value)],
        });
        self.send(msg);
    }

    fn send(&self, msg: OscPacket) {
        let packet = match encode(&msg){
            Ok(packet) => packet,
            Err(e) => {
                self.logger.err(&format!("Failed to encode OSC message: {e}"));
                return;
            }
        };
        let Some(target) = self.target else {
            return;
        };
        match self.socket.send_to(&packet, target) {
            Ok(_) => {
                shared().packets_sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                let msg = format!("Failed to send OSC message to {target}: {e}");
                let mut is_new = false;
                shared().update_status(|s| {
                    is_new = s.last_osc_error != msg;
                    s.last_osc_error = msg.clone();
                });
                // Every packet fails the same way while the network is down: log it once
                if is_new {
                    self.logger.err(&msg);
                }
            }
        };
    }
}

impl Osc {
    pub fn create(conf: Config, logger: ScopedLogger) -> ModuleCreateOutput {
        let source = conf.get_or_default("source", "0.0.0.0:4450".to_string());
        let socket = match UdpSocket::bind(&source) {
            Ok(socket) => socket,
            Err(e) => {
                logger.err(&format!("Failed to open source socket {source}: {e}"));
                return Err(());
            }
        };
        // Polled for /rkbx/reconnect and /rkbx/resend from the receiver (alive())
        if let Err(e) = socket.set_nonblocking(true) {
            logger.warn(&format!("Could not make the OSC socket non-blocking, remote reset is off: {e}"));
        }
        // Needed to send to a subnet broadcast address in LAN mode
        if let Err(e) = socket.set_broadcast(true) {
            logger.warn(&format!("Could not enable broadcast on the OSC socket: {e}"));
        }

        // The command line (--local / --lan) may already have set the target
        if shared().osc_target().is_none() {
            shared().set_osc_target(target_from_config(&conf, &logger));
        }

        let heartbeat: f32 = conf.get_or_default("heartbeat", 2.0);

        let mut osc = Osc {
            socket,
            target: None,
            target_generation: u64::MAX,
            info_sent: false,
            logger: logger.clone(),
            message_toggles: MessageToggles::new(&conf, logger),
            send_period: conf.get_or_default("send_every_nth", 2),
            send_period_counter: 0,
            last_beat_master: 0.0,
            last_beats: vec![0.0; 4],
            heartbeat: (heartbeat > 0.0).then(|| Duration::from_secs_f32(heartbeat)),
            last_heartbeat: Instant::now(),
            cache: MasterCache::default(),
            last_alive: None,
        };
        osc.refresh_target();
        Ok(Box::new(osc))
    }

    /// Picks up a target change made from the GUI
    fn refresh_target(&mut self) {
        let generation = shared().osc_target_generation.load(Ordering::SeqCst);
        if generation == self.target_generation {
            return;
        }
        self.target_generation = generation;
        let Some(target) = shared().osc_target() else {
            return;
        };
        match target.resolve() {
            Ok(addr) => {
                self.target = Some(addr);
                self.logger
                    .info(&format!("Sending OSC to {addr} ({} mode)", target.mode.as_str()));
                shared().update_status(|s| s.last_osc_error.clear());
                // A new receiver knows nothing yet
                self.send_cached_state();
            }
            Err(e) => {
                self.target = None;
                self.logger.err(&e);
                shared().update_status(|s| s.last_osc_error = e.clone());
            }
        }
    }

    fn send_cached_state(&mut self) {
        if let Some(bpm) = self.cache.bpm {
            self.send_float("/master/bpm/current", bpm);
        }
        if let Some(bpm) = self.cache.original_bpm {
            self.send_float("/master/bpm/original", bpm);
        }
        if let Some(track) = self.cache.track.clone() {
            self.send_track_master(&track);
        }
        if self.message_toggles.phrase_master {
            if let Some(phrase) = self.cache.phrase.clone() {
                self.output_phrase("/master/phrase/current", &phrase);
            }
            if let Some(phrase) = self.cache.next_phrase.clone() {
                self.output_phrase("/master/phrase/next", &phrase);
            }
            if let Some(beats) = self.cache.countin {
                self.send_float("/master/phrase/countin", beats as f32);
            }
        }
    }

    fn send_track_master(&self, track: &TrackInfo) {
        self.send_string("/master/track/title", &track.title);
        self.send_string("/master/track/artist", &track.artist);
        self.send_string("/master/track/album", &track.album);
    }
}

/// osc.mode picks the target: "local" sends to 127.0.0.1:osc.port, "lan" sends to
/// osc.destination (normally the subnet broadcast). Without osc.mode, a loopback
/// osc.destination means local and anything else means lan.
pub fn target_from_config(conf: &Config, logger: &ScopedLogger) -> OscTarget {
    let destination = conf.get_or_default("destination", "127.0.0.1:4460".to_string());
    let default_port = destination
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4460);
    let mode = match conf.get::<String>("mode") {
        Some(m) => OscMode::from_str(&m).unwrap_or_else(|| {
            logger.err(&format!("Unknown osc.mode '{m}', use local or lan"));
            OscMode::Local
        }),
        None => {
            if destination.starts_with("127.") || destination.starts_with("localhost") {
                OscMode::Local
            } else {
                OscMode::Lan
            }
        }
    };
    OscTarget {
        mode,
        port: conf.get_or_default("port", default_port),
        lan_destination: destination,
    }
}

/// Time between /rkbx/alive messages (sent in every state, so a receiver can tell a silent
/// rkbx_link from a Rekordbox with nothing playing).
const ALIVE_EVERY: Duration = Duration::from_secs(1);

impl Osc {
    /// Control messages sent back to the OSC source port: /rkbx/reconnect, /rkbx/resend.
    fn poll_control(&mut self) {
        let mut buf = [0u8; 1024];
        for _ in 0..8 {
            // Errors include WouldBlock (nothing waiting) and, on Windows, the ICMP "port
            // unreachable" of a receiver that is not running: all mean "nothing to read".
            let Ok((len, from)) = self.socket.recv_from(&mut buf) else {
                return;
            };
            let Ok((_, OscPacket::Message(msg))) = decode_udp(&buf[..len]) else {
                continue;
            };
            match msg.addr.as_str() {
                "/rkbx/reconnect" => {
                    self.logger.info(&format!("Reconnect requested by {from}"));
                    shared().reconnect_requested.store(true, Ordering::SeqCst);
                    shared().resend_requested.store(true, Ordering::SeqCst);
                }
                "/rkbx/resend" => shared().resend_requested.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
    }
}

// TODO: Avoid formatting strings every loop
impl OutputModule for Osc {
    fn alive(&mut self, rekordbox_ok: bool) {
        self.poll_control();
        if self.last_alive.is_none_or(|t| t.elapsed() >= ALIVE_EVERY) {
            self.last_alive = Some(Instant::now());
            self.refresh_target();
            self.send_int("/rkbx/alive", rekordbox_ok as i32);
        }
    }

    fn pre_update(&mut self) {
        self.send_period_counter = (self.send_period_counter + 1) % self.send_period;
        self.refresh_target();

        let resend = shared().resend_requested.swap(false, Ordering::SeqCst);
        let heartbeat_due = self
            .heartbeat
            .is_some_and(|period| self.last_heartbeat.elapsed() >= period);
        if resend || heartbeat_due {
            self.last_heartbeat = Instant::now();
            self.send_cached_state();
        }
    }

    fn bpm_changed_master(&mut self, bpm: f32) {
        self.cache.bpm = Some(bpm);
        self.send_float("/master/bpm/current", bpm);
    }

    fn bpm_changed(&mut self, bpm: f32, deck: usize) {
        self.send_float(&format!("/{deck}/bpm/current"), bpm);
    }

    fn original_bpm_changed_master(&mut self, bpm: f32) {
        self.cache.original_bpm = Some(bpm);
        self.send_float("/master/bpm/original", bpm);
    }

    fn original_bpm_changed(&mut self, bpm: f32, deck: usize) {
        self.send_float(&format!("/{deck}/bpm/original"), bpm);
    }

    fn beat_update_master(&mut self, beat: f32) {
        if self.send_period_counter != 0 {
            return;
        }

        for d in &self.message_toggles.beat_master_subdivs{
            let value = (beat % d) / d;
            self.send_float(&format!("/master/beat/subdiv/{d}"), value);
        }

        for d in &self.message_toggles.beat_master_triggers{
            if beat % d < self.last_beat_master % d {
                self.send_float(&format!("/master/beat/trigger/{d}"), 1.);
            }else if self.message_toggles.beat_trigger_autorelease && (beat + d * 0.2) % d < (self.last_beat_master + d * 0.2) % d{
                self.send_float(&format!("/master/beat/trigger/{d}"), 0.);
            }
        }
        
        self.last_beat_master = beat;
    }


    fn time_update_master(&mut self, time: f32) {
        if self.send_period_counter != 0 {
            return;
        }
        if self.message_toggles.time_master{
            self.send_float("/master/time", time);
        }
    }

    fn beat_update(&mut self, beat: f32, deck: usize) {
        if self.send_period_counter != 0 {
            return;
        }

        for d in &self.message_toggles.beat_subdivs{
            let value = (beat % d) / d;
            self.send_float(&format!("/{deck}/beat/subdiv/{d}"), value);
        }


        for d in &self.message_toggles.beat_triggers{
            if beat % d < self.last_beats[deck] % d {
                self.send_float(&format!("/{deck}/beat/trigger/{d}"), 1.);
            }else if self.message_toggles.beat_trigger_autorelease && (beat + d * 0.2) % d < (self.last_beats[deck] + d * 0.2) % d{
                self.send_float(&format!("/{deck}/beat/trigger/{d}"), 0.);
            }
        }
        self.last_beats[deck] = beat;
    }

    fn time_update(&mut self, time: f32, deck: usize) {
        if self.send_period_counter != 0 {
            return;
        }
        if self.message_toggles.time{
            self.send_float(&format!("/{deck}/time"), time);
        }
    }

    fn track_changed(&mut self, track: &TrackInfo, deck: usize) {
        self.send_string(&format!("/{deck}/track/title"), &track.title);
        self.send_string(&format!("/{deck}/track/artist"), &track.artist);
        self.send_string(&format!("/{deck}/track/album"), &track.album);
    }

    fn track_changed_master(&mut self, track: &TrackInfo) {
        self.cache.track = Some(track.clone());
        self.send_track_master(track);
    }

    fn slow_update(&mut self) {
        if !self.info_sent {
            self.info_sent = true;
            let source_addr = match self.socket.local_addr() {
                Ok(addr) => addr.to_string(),
                Err(_) => "No source!!".to_string(),
            };
            let target_addr = match self.target {
                Some(addr) => addr.to_string(),
                None => "No target!!".to_string(),
            };
            self.logger
                .info(&format!("Sending {source_addr} -> {target_addr}"));
        }
    }

    fn phrase_changed_master(&mut self, phrase: &str) {
        self.cache.phrase = Some(phrase.to_string());
        if self.message_toggles.phrase_master{
            self.output_phrase("/master/phrase/current", phrase);
        }
    }

    fn next_phrase_changed_master(&mut self, phrase: &str) {
        self.cache.next_phrase = Some(phrase.to_string());
        if self.message_toggles.phrase_master{
            self.output_phrase("/master/phrase/next", phrase);
        }
    }

    fn next_phrase_in_master(&mut self, beats: i32) {
        self.cache.countin = Some(beats);
        if self.message_toggles.phrase_master{
            self.send_float("/master/phrase/countin", beats as f32);
        }
    }

    fn phrase_changed(&mut self, phrase: &str, deck: usize) {
        if self.message_toggles.phrase{
            self.output_phrase(&format!("/{deck}/phrase/current"), phrase);
        }
    }

    fn next_phrase_changed(&mut self, phrase: &str, deck: usize) {
        if self.message_toggles.phrase{
            self.output_phrase(&format!("/{deck}/phrase/next"), phrase);
        }
    }

    fn next_phrase_in(&mut self, beats: i32, deck: usize) {
        if self.message_toggles.phrase{
            self.send_float(&format!("/{deck}/phrase/countin"), beats as f32);
        }
    }
}

impl Osc{
    fn output_phrase(&mut self, addr: &str, phrase: &str){
        match self.message_toggles.phrase_output_format {
            OutputFormat::String => self.send_string(addr, phrase),
            OutputFormat::Int => self.send_int(addr, PhraseParser::phrase_name_to_index(phrase)),
            OutputFormat::Float => self.send_float(addr, PhraseParser::phrase_name_to_index(phrase) as f32),
        }
    }
}
