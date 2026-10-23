use crate::config::Config;
use crate::log::ScopedLogger;
use crate::memory::MemReader;
use crate::memory::MemoryReadErrorType;
use crate::memory::MemoryReadError;
use crate::outputmodules::ModuleDefinition;
use crate::outputmodules::OutputModule;
use crate::utils::PhraseParser;
use crate::RekordboxOffsets;
use crate::memory::PointerChainValue;
use binrw::BinRead;
use notify::Watcher;
use rekordcrate::anlz::{self, BeatGrid};
use std::io::Cursor;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use crate::memory::Value;

/// A deck whose position has not changed for this long is not playing.
const STILL_AFTER: Duration = Duration::from_millis(250);
/// How long a deck must play before it takes over from a deck that is playing too.
/// Keeps a cue preview on the other deck from pulling the output over.
const TAKEOVER_WHILE_PLAYING: Duration = Duration::from_millis(1000);
/// How long a deck must play before it takes over from a stopped deck.
const TAKEOVER_WHILE_STOPPED: Duration = Duration::from_millis(300);

/// Whether a deck plays, from its sample position: Rekordbox has no play flag we read.
#[derive(Clone, Default)]
struct DeckMotion {
    last_position: Option<i64>,
    last_change: Option<Instant>,
    /// When the current run of position changes began, None while the deck stands still
    moving_since: Option<Instant>,
}

impl DeckMotion {
    fn observe(&mut self, position: i64, now: Instant) {
        if self.last_position.is_some_and(|p| p != position) {
            self.last_change = Some(now);
            self.moving_since.get_or_insert(now);
        }
        self.last_position = Some(position);
        if self.last_change.is_none_or(|t| now - t > STILL_AFTER) {
            self.moving_since = None;
        }
    }

    fn playing(&self) -> bool {
        self.moving_since.is_some()
    }

    fn played_for(&self, now: Instant) -> Duration {
        self.moving_since.map_or(Duration::ZERO, |t| now - t)
    }
}

/// The deck to send as master. Rekordbox keeps MASTER on a deck that is stopped while
/// another one plays (it moves only with sync), so the deck that is heard wins:
/// Rekordbox's master once it plays, else the deck followed now (`current`, usize::MAX for
/// none) while it plays, else the deck that has played longest. With nothing playing the
/// followed deck stays.
fn pick_master(motion: &[DeckMotion], current: usize, rekordbox_master: usize, now: Instant) -> usize {
    let decks = motion.len();
    let current_playing = current < decks && motion[current].playing();
    let takeover = if current_playing { TAKEOVER_WHILE_PLAYING } else { TAKEOVER_WHILE_STOPPED };
    let established = |i: usize| i == current || motion[i].played_for(now) >= takeover;

    if rekordbox_master < decks && motion[rekordbox_master].playing() && established(rekordbox_master) {
        return rekordbox_master;
    }
    if current_playing {
        return current;
    }
    let longest = (0..decks)
        .filter(|&i| motion[i].playing() && established(i))
        .max_by_key(|&i| motion[i].played_for(now));
    match longest {
        Some(deck) => deck,
        None if current < decks => current,
        None => rekordbox_master,
    }
}








pub struct Rekordbox {
    masterdeck_index: Value<u8>,
    current_bpms: Vec<Value<f32>>,
    sample_positions: Vec<Value<i64>>,
    track_infos: Vec<PointerChainValue<[u8; 200]>>,
    anlz_paths: Vec<PointerChainValue<[u8; 500]>>,
    deckcount: usize,
    phraseparser: PhraseParser,
    mem: MemReader,
    offsets: RekordboxOffsets,
}

impl Rekordbox {
    fn new(offsets: RekordboxOffsets, decks: usize) -> Result<Self, MemoryReadError> {
        let mem = crate::memory::MemReader::new()?;

        let current_bpms = mem.new_values(&offsets.current_bpm[0..decks])?;
        let sample_positions = mem.new_values(&offsets.sample_position[0..decks])?;
        let track_infos = mem.new_pointerchain_values(&offsets.track_info[0..decks]);
        let anlz_paths = mem.new_pointerchain_values(&offsets.anlz_path[0..decks]);

        let deckcount = current_bpms.len();

        let masterdeck_index_val: Value<u8> = mem.new_value(&offsets.masterdeck_index)?;

        Ok(Self {
            current_bpms,
            sample_positions,
            masterdeck_index: masterdeck_index_val,
            deckcount,
            track_infos,
            anlz_paths,
            phraseparser: PhraseParser::new(),
            mem,
            offsets,
        })
    }

    /// Walks the pointer chains of the fixed-address values again. They are resolved once at
    /// connect, and Rekordbox moves its deck objects (decks unloaded, idle, reloaded): the old
    /// addresses stay readable but freeze, so a stale value never errors. On failure the old
    /// addresses are kept (Rekordbox may be mid-reload) and the error is returned.
    fn refresh(&mut self) -> Result<(), MemoryReadError> {
        let decks = self.deckcount;
        let current_bpms = self.mem.new_values(&self.offsets.current_bpm[0..decks])?;
        let sample_positions = self.mem.new_values(&self.offsets.sample_position[0..decks])?;
        let masterdeck_index = self.mem.new_value(&self.offsets.masterdeck_index)?;
        self.current_bpms = current_bpms;
        self.sample_positions = sample_positions;
        self.masterdeck_index = masterdeck_index;
        Ok(())
    }

    fn read_timing_data(&self, deck: usize) -> Result<TimingDataRaw, MemoryReadError> {
        let sample_position = self.sample_positions[deck].read(&self.mem)?;
        let current_bpm = self.current_bpms[deck].read(&self.mem)?;

        Ok(TimingDataRaw {
            current_bpm,
            sample_position,
        })
    }

    fn read_masterdeck_index(&self) -> Result<usize, MemoryReadError> {
        Ok(self.masterdeck_index.read(&self.mem)? as usize)
    }

    fn get_track_infos(&self) -> Result<Vec<TrackInfo>, MemoryReadError> {
        (0..self.deckcount)
            .map(|i| {
                let raw = self.track_infos[i]
                    .read(&self.mem)?
                    .into_iter()
                    .take_while(|x| *x != 0x00)
                    .collect::<Vec<u8>>();
                let text = String::from_utf8(raw).unwrap_or_else(|_| "ERR".to_string());
                let mut lines = text
                    .lines()
                    .map(|x| x.split_once(": ").unwrap_or(("", "")).1)
                    .map(|x| x.to_string());

                Ok(TrackInfo {
                    title: lines.next().unwrap_or("".to_string()),
                    artist: lines.next().unwrap_or("".to_string()),
                    album: lines.next().unwrap_or("".to_string()),
                })
            })
            .collect()
    }

    fn get_anlz_paths(&self) -> Result<Vec<String>, MemoryReadError> {
        (0..self.deckcount)
            .map(|i| {
                let raw = self.anlz_paths[i]
                    .read(&self.mem)?
                    .into_iter()
                    .take_while(|x| *x != 0x00)
                    .collect::<Vec<u8>>();
                Ok(String::from_utf8(raw).unwrap_or_else(|_| "ERR".to_string()))
            })
            .collect()
    }
}

#[derive(Debug)]
struct TimingDataRaw {
    current_bpm: f32,
    sample_position: i64,
}

#[derive(Debug, PartialEq, Clone)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
}
impl Default for TrackInfo {
    fn default() -> Self {
        Self {
            title: "".to_string(),
            artist: "".to_string(),
            album: "".to_string(),
        }
    }
}

#[derive(Clone)]
struct ChangeTrackedValue<T> {
    value: T,
}
impl<T: std::cmp::PartialEq> ChangeTrackedValue<T> {
    fn new(value: T) -> Self {
        Self { value }
    }
    fn set(&mut self, value: T) -> bool {
        if self.value != value {
            self.value = value;
            true
        } else {
            false
        }
    }
}

pub struct BeatKeeper {
    /// The deck sent as master: Rekordbox's master deck, or with follow_playing the deck
    /// that is actually playing. usize::MAX until the first update.
    masterdeck_index: ChangeTrackedValue<usize>,
    follow_playing: bool,
    motion: Vec<DeckMotion>,
    offset_samples: i64,
    running_modules: Vec<Box<dyn OutputModule>>,

    track_infos: Vec<ChangeTrackedValue<TrackInfo>>,
    track_trackers: Vec<TrackTracker>,

    anlz_paths: Vec<ChangeTrackedValue<String>>,
    watcher: notify::RecommendedWatcher,
    watcher_rx: mpsc::Receiver<notify::Result<notify::Event>>,

    logger: ScopedLogger,
    last_error: Option<MemoryReadError>,
    keep_warm: bool,
    decks: usize,


    td_trackers: Vec<TrackingDataTracker>,
    master_td_tracker: TrackingDataTracker,
}

struct TrackingDataTracker {
    bpm_changed: ChangeTrackedValue<f32>,
    original_bpm_changed: ChangeTrackedValue<f32>,
    beat_changed: ChangeTrackedValue<f32>,
    pos_changed: ChangeTrackedValue<i64>,
    phrase: ChangeTrackedValue<String>,
    next_phrase: ChangeTrackedValue<String>,
    next_phrase_in: ChangeTrackedValue<i32>,
}

impl TrackingDataTracker {
    fn new() -> Self {
        Self {
            bpm_changed: ChangeTrackedValue::new(0.),
            original_bpm_changed: ChangeTrackedValue::new(0.),
            beat_changed: ChangeTrackedValue::new(0.),
            pos_changed: ChangeTrackedValue::new(0),
            phrase: ChangeTrackedValue::new("".to_string()),
            next_phrase: ChangeTrackedValue::new("".to_string()),
            next_phrase_in: ChangeTrackedValue::new(0),
        }
    }
}

impl BeatKeeper {
    pub fn start(
        offsets: RekordboxOffsets,
        modules: Vec<ModuleDefinition>,
        config: Config,
        logger: ScopedLogger,
    ) {
        let keeper_config = config.reduce_to_namespace("keeper");
        let update_rate = keeper_config.get_or_default("update_rate", 50);
        let slow_update_denominator = keeper_config.get_or_default("slow_update_every_nth", 50);

        let mut running_modules = vec![];

        logger.info("Active modules:");
        for module in modules {
            if !config.get_or_default(&format!("{}.enabled", module.config_name), false) {
                continue;
            }
            logger.info(&format!(" - {}", module.pretty_name));

            let conf = config.reduce_to_namespace(&module.config_name);
            match (module.create)(conf, ScopedLogger::new(&logger.logger, &module.pretty_name)) {
                Ok(module) => {
                    running_modules.push(module);
                }
                Err(()) => {
                    logger.err(&format!("Failed to start module {}", module.pretty_name));
                }
            }
        }

        let (watcher_tx, watcher_rx) = mpsc::channel();
        let watcher = match notify::recommended_watcher(watcher_tx){
            Ok(w) => w,
            Err(e) => {
                logger.err(&format!("Failed to create watcher: {e}"));
                return;
            }
        };

        let mut keeper = BeatKeeper {
            masterdeck_index: ChangeTrackedValue::new(usize::MAX),
            follow_playing: keeper_config.get_or_default("follow_playing", true),
            motion: vec![DeckMotion::default(); 4],
            offset_samples: (keeper_config.get_or_default("delay_compensation", 0.) * 44100. / 1000.) as i64,
            track_infos: vec![ChangeTrackedValue::new(Default::default()); 4],
            running_modules,
            logger: logger.clone(),
            last_error: None,
            track_trackers: (0..4).map(|_| TrackTracker::new()).collect(),
            keep_warm: keeper_config.get_or_default("keep_warm", true),
            decks: keeper_config.get_or_default("decks", 4),
            td_trackers: (0..4).map(|_| TrackingDataTracker::new()).collect(),
            master_td_tracker: TrackingDataTracker::new(),
            anlz_paths: vec![ChangeTrackedValue::new("".to_string()); 4],
            watcher,
            watcher_rx,
        };

        let mut rekordbox = None;

        let period = Duration::from_micros(1000000 / update_rate); // 50Hz
        let mut n = 0;
        // Self-healing: re-resolve the value pointers every REFRESH_EVERY, and attach to
        // Rekordbox again when no master deck has been readable for NO_MASTER_REATTACH.
        const REFRESH_EVERY: Duration = Duration::from_secs(1);
        const NO_MASTER_REATTACH: Duration = Duration::from_secs(5);
        let mut last_refresh = std::time::Instant::now();
        let mut no_master_since: Option<std::time::Instant> = None;
        let mut no_master_logged = false;

        logger.info("Looking for Rekordbox...");
        println!();

        loop {
            if crate::status::shared().reconnect_requested.swap(false, std::sync::atomic::Ordering::SeqCst)
                && rekordbox.is_some()
            {
                logger.info("Reconnecting to Rekordbox (requested)");
                rekordbox = None;
                no_master_since = None;
                no_master_logged = false;
                crate::status::shared().update_status(|s| s.rekordbox_connected = false);
            }
            if let Some(rb) = &mut rekordbox {
                let update_start_time = std::time::Instant::now();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| keeper.update(rb, n == 0)));
                let result = match result {
                    Ok(r) => r,
                    Err(_) => {
                        // The panic hook (main.rs) has logged what and where. Start clean: the
                        // track data is read again after the reconnect.
                        logger.err("Internal error while reading Rekordbox (see the panic above), reconnecting");
                        keeper.reset_tracks();
                        rekordbox = None;
                        no_master_since = None;
                        crate::status::shared().update_status(|s| s.rekordbox_connected = false);
                        keeper.alive(false);
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                match result {
                    Err(e) => {
                        keeper.alive(false);
                        keeper.report_error(e);

                        rekordbox = None;
                        no_master_since = None;
                        crate::status::shared().update_status(|s| s.rekordbox_connected = false);
                        logger.err("Connection to Rekordbox lost");
                        logger.info("Reconnecting in 3s...");
                        thread::sleep(Duration::from_secs(3));
                    }
                    Ok(master_ok) => {
                        keeper.alive(master_ok);
                        if last_refresh.elapsed() >= REFRESH_EVERY {
                            last_refresh = std::time::Instant::now();
                            if let Err(e) = rb.refresh() {
                                logger.debug(&format!("Pointer refresh failed at {:#x}, keeping the old ones", e.address));
                            }
                        }
                        if master_ok {
                            no_master_since = None;
                            no_master_logged = false;
                        } else if no_master_since.is_none() {
                            no_master_since = Some(std::time::Instant::now());
                        }
                        if no_master_since.is_some_and(|t| t.elapsed() >= NO_MASTER_REATTACH) {
                            // Stale pointers, or Rekordbox reloaded its decks: attach again
                            if !no_master_logged {
                                logger.info("No master deck readable for 5 s, attaching to Rekordbox again (repeats quietly while no deck is loaded)");
                                no_master_logged = true;
                            }
                            rekordbox = None;
                            no_master_since = None;
                            thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                        n = (n + 1) % slow_update_denominator;
                        let elapsed = update_start_time.elapsed();
                        if period > elapsed {
                            thread::sleep(period - elapsed);
                        }
                    }
                }
            } else {
                keeper.alive(false);
                match Rekordbox::new(offsets.clone(), config.get_or_default("keeper.decks", 2)) {
                    Ok(rb) => {
                        rekordbox = Some(rb);
                        last_refresh = std::time::Instant::now();
                        if no_master_logged {
                            // A quiet re-attach after "no master deck": not announced each time
                            continue;
                        }
                        println!();
                        logger.good("Connected to Rekordbox!");
                        crate::status::shared().update_status(|s| s.rekordbox_connected = true);
                        keeper.last_error = None;
                    }
                    Err(e) => {
                        keeper.report_error(e);
                        logger.info("...");
                        thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        }
    }

    

    /// Ok(true) when a master deck was read, Ok(false) when none is selected (or readable).
    fn update(
        &mut self,
        rb: &Rekordbox,
        slow_update: bool,
    ) -> Result<bool, MemoryReadError> {
        let rekordbox_master = rb.read_masterdeck_index()?;
        if rekordbox_master >= rb.deckcount {
            return Ok(false); // No master deck selected - rekordbox is not initialised
        }
        let master = if self.follow_playing {
            let now = Instant::now();
            for i in 0..self.decks.min(rb.deckcount) {
                if let Ok(position) = rb.sample_positions[i].read(&rb.mem) {
                    self.motion[i].observe(position, now);
                }
            }
            self.pick_master(rekordbox_master, now)
        } else {
            rekordbox_master
        };
        let masterdeck_index_changed = self.masterdeck_index.set(master);
        if masterdeck_index_changed {
            if master == rekordbox_master {
                self.logger.info(&format!("Following deck {} (Rekordbox master)", master + 1));
            } else {
                self.logger.info(&format!(
                    "Following deck {}, it plays while the Rekordbox master (deck {}) does not",
                    master + 1,
                    rekordbox_master + 1
                ));
            }
        }

        // let mut tracker_data = None;

        for module in &mut self.running_modules {
            module.pre_update();
        }

        for (i, (tracker, td_tracker)) in (self.track_trackers[0..self.decks])
            .iter_mut()
            .zip(self.td_trackers[0..self.decks].iter_mut())
            .enumerate()
        {
            let is_master = i == self.masterdeck_index.value;
            if is_master | self.keep_warm {
                let res =
                    tracker.update(rb, self.offset_samples, i);
                let Ok(res) = res else {
                    continue;
                };

                let bpm_changed = td_tracker.bpm_changed.set(res.timing_data_raw.current_bpm);
                let original_bpm_changed = td_tracker.original_bpm_changed.set(res.original_bpm);
                let beat_changed = td_tracker.beat_changed.set(res.beat);
                let pos_changed = td_tracker.pos_changed.set(res.timing_data_raw.sample_position);
                let phrase_changed = td_tracker.phrase.set(res.phrase.clone());
                let next_phrase_changed = td_tracker.next_phrase.set(res.next_phrase.clone());
                let next_phrase_in_changed = td_tracker.next_phrase_in.set(res.next_phrase_in);

                for module in &mut self.running_modules {
                    if beat_changed {
                        module.beat_update(res.beat, i);
                    }
                    if pos_changed {
                        module.time_update(res.timing_data_raw.sample_position as f32 / 44100., i);
                    }
                    if bpm_changed {
                        module.bpm_changed(res.timing_data_raw.current_bpm, i);
                    }
                    if original_bpm_changed {
                        module.original_bpm_changed(res.original_bpm, i);
                    }
                    if phrase_changed {
                        module.phrase_changed(&res.phrase, i);
                    }
                    if next_phrase_changed {
                        module.next_phrase_changed(&res.next_phrase, i);
                    }
                    if next_phrase_in_changed {
                        module.next_phrase_in(res.next_phrase_in, i);
                    }
                }

                if is_master {
                    let bpm_changed = self
                        .master_td_tracker
                        .bpm_changed
                        .set(res.timing_data_raw.current_bpm);
                    let original_bpm_changed = self
                        .master_td_tracker
                        .original_bpm_changed
                        .set(res.original_bpm);
                    let beat_changed = self.master_td_tracker.beat_changed.set(res.beat);
                    let pos_changed = self
                        .master_td_tracker
                        .pos_changed
                        .set(res.timing_data_raw.sample_position);
                    let phrase_changed = self
                        .master_td_tracker
                        .phrase
                        .set(res.phrase);
                    let next_phrase_changed = self
                        .master_td_tracker
                        .next_phrase
                        .set(res.next_phrase);
                    let next_phrase_in_changed = self
                        .master_td_tracker
                        .next_phrase_in
                        .set(res.next_phrase_in);


                    for module in &mut self.running_modules {
                        if beat_changed {
                            module.beat_update_master(res.beat);
                        }
                        if pos_changed {
                            module.time_update_master(
                                res.timing_data_raw.sample_position as f32 / 44100.,
                            );
                        }
                        if bpm_changed {
                            module.bpm_changed_master(res.timing_data_raw.current_bpm);
                        }
                        if original_bpm_changed {
                            module.original_bpm_changed_master(res.original_bpm);
                        }
                        if phrase_changed {
                            module.phrase_changed_master(&self.master_td_tracker.phrase.value);
                        }
                        if next_phrase_changed {
                            module.next_phrase_changed_master(&self.master_td_tracker.next_phrase.value);
                        }
                        if next_phrase_in_changed {
                            module.next_phrase_in_master(res.next_phrase_in);
                        }
                    }
                }
            }
        }


        let mut masterdeck_track_changed = false;

        if slow_update {
            for (i, track) in rb.get_track_infos()?.into_iter().enumerate() {
                if self.track_infos[i].set(track) {
                    for module in &mut self.running_modules {
                        module.track_changed(&self.track_infos[i].value, i);
                    }
                    self.track_trackers[i].track_changed = true;
                    masterdeck_track_changed |= self.masterdeck_index.value == i;
                }
            }


            let mut anlz_file_updates = [false; 4];
            while let Ok(u) = self.watcher_rx.try_recv(){
                match u {
                    Ok(event) => {
                        if let Some(path) = event.paths.first() {
                            let path = path.to_string_lossy().replace("\\", "/");
                            if let Some(i) = self.anlz_paths.iter().position(|x| x.value == path || x.value.replace(".DAT", ".EXT") == path) {
                                anlz_file_updates[i] = true;
                            }
                        }
                    }
                    Err(e) => {
                        self.logger.err(&format!("Watcher error: {e}"));
                    }
                }
            }

            for (i, path) in rb.get_anlz_paths()?.into_iter().enumerate() {
                if self.anlz_paths[i].value != path || anlz_file_updates[i] {
                    if self.anlz_paths[i].value != path {
                        self.logger.debug(&format!("Deck {i} ANLZ file path changed: {path}"));

                        // Only unwatch if there was a previous path (not empty)
                        if !self.anlz_paths[i].value.is_empty() {
                            self.watcher.unwatch(std::path::Path::new(&self.anlz_paths[i].value)).unwrap_or_else(|e| {
                                self.logger.err(&format!("Deck {i}: Failed to unwatch path {}: {}", &self.anlz_paths[i].value, e));
                            });
                            self.watcher.unwatch(std::path::Path::new(&self.anlz_paths[i].value.replace(".DAT", ".EXT"))).unwrap_or_else(|e| {
                                self.logger.err(&format!("Deck {i}: Failed to unwatch path {}: {}", &self.anlz_paths[i].value.replace(".DAT", ".EXT"), e));
                            });
                        }

                        self.anlz_paths[i].set(path);

                        // Only watch if the new path is not empty
                        if !self.anlz_paths[i].value.is_empty() {
                            if let Err(e) = self.watcher.watch(std::path::Path::new(&self.anlz_paths[i].value), notify::RecursiveMode::NonRecursive) {
                                self.logger.err(&format!("Deck {i}: Failed to watch path {}: {}", &self.anlz_paths[i].value, e));
                            }
                            if let Err(e) = self.watcher.watch(std::path::Path::new(&self.anlz_paths[i].value.replace(".DAT", ".EXT")), notify::RecursiveMode::NonRecursive) {
                                self.logger.err(&format!("Deck {i}: Failed to watch path {}: {}", &self.anlz_paths[i].value.replace(".DAT", ".EXT"), e));
                            }
                        }
                    }

                    // TODO watch out here, there's probably loads of things that can go wrong
                    let Ok(bytes) = std::fs::read(&self.anlz_paths[i].value) else {
                        self.logger.err(&format!("Failed to read anlz file for deck {i}: {}", &self.anlz_paths[i].value));
                        self.logger.err("If you are loading a new streaming track for the first time, eject and load it again.");
                        continue;
                    };
                    let mut reader = Cursor::new(bytes);
                    let anlz = match rekordcrate::anlz::ANLZ::read(&mut reader){
                        Ok(a) => a,
                        Err(e) => {
                            self.logger.err(&format!("Failed to parse DAT file for song {}, path {}: {e}", &self.track_infos[i].value.title, &self.anlz_paths[i].value));
                            continue;
                        }
                    };
                    for section in anlz.sections {
                        #[allow(clippy::single_match)]
                        match section.content {
                            anlz::Content::BeatGrid(grid) => {
                                self.track_trackers[i].beatgrid = Some(grid);
                            }
                            _ => (),
                        }
                    }

                    let bytes = match std::fs::read(self.anlz_paths[i].value.replace(".DAT", ".EXT")) {
                        Ok(b) => b,
                        Err(e) => {
                            self.logger.err(&format!("Failed to read EXT file for song {}, path {}: {e}", &self.track_infos[i].value.title, &self.anlz_paths[i].value));
                            continue;
                        }
                    };

                    let mut reader = Cursor::new(bytes);
                    let anlz = match rekordcrate::anlz::ANLZ::read(&mut reader) {
                        Ok(a) => a,
                        Err(e) => {
                            self.logger.err(&format!("Failed to parse EXT file for song {}, path {}: {e}", &self.track_infos[i].value.title, &self.anlz_paths[i].value.replace(".DAT", ".EXT")));
                            continue;
                        }
                    };
                    for section in anlz.sections {
                        #[allow(clippy::single_match)]
                        match section.content {
                            anlz::Content::SongStructure(phrases) => {
                                self.track_trackers[i].songstructure = Some(phrases.data);
                            }
                            _ => (),
                        }
                    }
                }
            }
            for module in &mut self.running_modules {
                module.slow_update();
            }
        }

        if masterdeck_index_changed || masterdeck_track_changed {
            let track = &self.track_infos[self.masterdeck_index.value].value;
            self.logger
                .debug(&format!("Master track changed: {track:?}"));
            for module in &mut self.running_modules {
                module.track_changed_master(track);
            }
        }

        Ok(true)
    }

    fn pick_master(&self, rekordbox_master: usize, now: Instant) -> usize {
        let decks = self.decks.min(self.motion.len());
        pick_master(&self.motion[0..decks], self.masterdeck_index.value, rekordbox_master, now)
    }

    fn alive(&mut self, rekordbox_ok: bool) {
        for module in &mut self.running_modules {
            module.alive(rekordbox_ok);
        }
    }

    /// Forget every deck's track, beat grid and phrases, so they are read again.
    fn reset_tracks(&mut self) {
        self.track_trackers = (0..4).map(|_| TrackTracker::new()).collect();
        self.td_trackers = (0..4).map(|_| TrackingDataTracker::new()).collect();
        self.master_td_tracker = TrackingDataTracker::new();
        self.motion = vec![DeckMotion::default(); 4];
        for path in &self.anlz_paths {
            if !path.value.is_empty() {
                let _ = self.watcher.unwatch(std::path::Path::new(&path.value));
                let _ = self.watcher.unwatch(std::path::Path::new(&path.value.replace(".DAT", ".EXT")));
            }
        }
        self.anlz_paths = vec![ChangeTrackedValue::new("".to_string()); 4];
        self.track_infos = vec![ChangeTrackedValue::new(Default::default()); 4];
    }

    fn report_error(&mut self, e: MemoryReadError) {
        if let Some(last) = &self.last_error {
            if e == *last {
                return;
            }
        }
        let detail = if let Some(d) = &e.detail{
            &format!(": {d}")
        }else{
            ""
        };
        match e.error_type {
            MemoryReadErrorType::ProcessNotFound => {
                self.logger.err("Rekordbox process not found!");
            }
            MemoryReadErrorType::SnapshotFailed => {
                self.logger.err(&format!("Snapshot failed{detail}"));
                self.logger.info("    Ensure Rekordbox is running!");
            }
            MemoryReadErrorType::ReadMemoryFailed => {
                self.logger.err(&format!("Read memory failed{detail}"));
                self.logger.info("    Try the following:");
                self.logger.info("    - Wait for Rekordbox to start and load a track");
                self.logger.info("    - Ensure you have selected the correct Rekordbox version in the config");
                self.logger.info("    - Check the number of decks in the config");
                self.logger.info("    - Update the offsets and program");
                // self.logger.info("    - NOTE: Memory offsets from Windows may not work on macOS");
                self.logger.info("    If nothing works, wait for an update, or enable Debug in config and submit this entire error message on an Issue on GitHub.");
            }
            MemoryReadErrorType::WriteMemoryFailed => {
                // Should not happen as no memory is ever written
                self.logger.err(&format!("Write memory failed{detail}"));
                self.logger.err("This error should never happen - if it does, something is wrong");
            }
            MemoryReadErrorType::ModuleNotFound => {
                self.logger.err(&format!("Module not found: {detail}"));
                self.logger.err("This new exciting error has never been seen before! Please report this on GitHub!");
            }
        };
        if let Some(p) = &e.pointer {
            self.logger.debug(&format!("Pointer: {p}"));
        }
        if e.address != 0 {
            self.logger.debug(&format!("Address: {:X}", e.address));
        }
        self.last_error = Some(e);
    }
}

struct TrackTrackerResult {
    beat: f32,
    original_bpm: f32,
    timing_data_raw: TimingDataRaw,
    phrase: String,
    next_phrase: String,
    next_phrase_in: i32,
}

struct TrackTracker {
    track_changed: bool, // External flag to indicate that the track has changed
    beatgrid: Option<BeatGrid>,
    songstructure: Option<rekordcrate::anlz::SongStructureData>,
}

impl TrackTracker {
    fn new() -> Self {
        Self {
            track_changed: false,
            beatgrid: None,
            songstructure: None,
        }
    }

    fn update(
        &mut self,
        rb: &Rekordbox,
        offset_samples: i64,
        deck: usize,
    ) -> Result<TrackTrackerResult, MemoryReadError> {
        let mut td = rb.read_timing_data(deck)?;
        if td.current_bpm == 0.0 {
            td.current_bpm = 120.0;
        }

        

        let mut beat = 0.0;
        let mut original_bpm = 120.0;

        let time_now = (td.sample_position + offset_samples) as f32 / 44100.;
        let mut beat_idx: usize = 0;
        if let Some(grid) = self.beatgrid.as_ref().filter(|g| !g.beats.is_empty()) {
            for gridbeat in grid.beats.iter() {
                if gridbeat.time as f32 / 1000. >= time_now {
                    break;
                }
                beat_idx += 1;
            }
            beat_idx = beat_idx.saturating_sub(1);
            let gridbeat = &grid.beats[beat_idx];
            // println!("{} - {}", time, time_now);
            let remainder = time_now - gridbeat.time as f32 / 1000.;
            original_bpm = gridbeat.tempo as f32 / 100.0;
            let spb = 1. / (gridbeat.tempo as f32 / 100. / 60.0);

            let b = (gridbeat.beat_number + 3) % 4;
            // println!("{b} {idx}");
            beat = b as f32 + remainder / spb;
        }


        let beat_num = beat_idx + 1;

        let mut tout = TrackTrackerResult {
            beat,
            original_bpm,
            timing_data_raw: td,
            phrase: "".to_string(),
            next_phrase: "".to_string(),
            next_phrase_in: 0,
        };

        let mut phrase_idx: usize = 0;
        if let Some(songstructure) = self.songstructure.as_ref().filter(|s| !s.phrases.is_empty()) {
            // println!("Song structure: {:?}", songstructure);
            for phrase in songstructure.phrases.iter() {
                // println!("beat {} / {beat_idx}", phrase.beat);
                if phrase.beat as usize > beat_num {
                    break;
                }
                phrase_idx += 1;
            }
            phrase_idx = phrase_idx.saturating_sub(1);
            // println!("{phrase_idx} {beat_idx} {:?}", &songstructure.phrases[phrase_idx].kind);
            // println!("Phrase: {beat_num} {}", rb.phraseparser.get_phrase_name(&songstructure.mood, &songstructure.phrases[phrase_idx]));
            tout.phrase = rb.phraseparser.get_phrase_name(&songstructure.mood, &songstructure.phrases[phrase_idx]);
            if phrase_idx + 1 < songstructure.phrases.len() {
                let next_phrase = &songstructure.phrases[phrase_idx + 1];
                let next_phrase_in = next_phrase.beat as i32 - beat_num as i32;
                tout.next_phrase = rb.phraseparser.get_phrase_name(&songstructure.mood, next_phrase);
                tout.next_phrase_in = next_phrase_in;
                // println!("{}: {next_phrase_in}", rb.phraseparser.get_phrase_name(&songstructure.mood, next_phrase));
            }
        }

        Ok(tout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two decks read every 10 ms. `moving[d]` says whether deck d plays during the run; the
    /// followed deck is carried in and returned.
    struct Sim {
        motion: Vec<DeckMotion>,
        positions: [i64; 2],
        now: Instant,
        current: usize,
    }

    impl Sim {
        fn new() -> Self {
            Self { motion: vec![DeckMotion::default(); 2], positions: [1000, 5000], now: Instant::now(), current: usize::MAX }
        }

        fn run(&mut self, ms: u64, moving: [bool; 2], rekordbox_master: usize) -> usize {
            for _ in 0..ms / 10 {
                self.now += Duration::from_millis(10);
                for d in 0..2 {
                    if moving[d] {
                        self.positions[d] += 441;
                    }
                    self.motion[d].observe(self.positions[d], self.now);
                }
                self.current = pick_master(&self.motion, self.current, rekordbox_master, self.now);
            }
            self.current
        }
    }

    #[test]
    fn nothing_playing_follows_rekordbox_master() {
        let mut sim = Sim::new();
        assert_eq!(sim.run(500, [false, false], 1), 1);
    }

    #[test]
    fn playing_deck_wins_over_a_stopped_master() {
        let mut sim = Sim::new();
        sim.run(200, [false, false], 1);
        // Deck 1 is MASTER but parked, deck 0 plays
        assert_eq!(sim.run(200, [true, false], 1), 1, "not before the takeover time");
        assert_eq!(sim.run(300, [true, false], 1), 0);
        assert_eq!(sim.run(5000, [true, false], 1), 0);
    }

    #[test]
    fn cue_preview_does_not_take_over() {
        let mut sim = Sim::new();
        sim.run(1000, [true, false], 0);
        assert_eq!(sim.run(600, [true, true], 0), 0);
        assert_eq!(sim.run(1000, [true, false], 0), 0);
    }

    #[test]
    fn master_takes_back_once_it_plays() {
        let mut sim = Sim::new();
        sim.run(1000, [true, false], 1);
        assert_eq!(sim.current, 0);
        assert_eq!(sim.run(500, [true, true], 1), 0, "a short run of the master is not enough");
        assert_eq!(sim.run(800, [true, true], 1), 1);
    }

    #[test]
    fn followed_deck_stays_when_everything_stops() {
        let mut sim = Sim::new();
        sim.run(1000, [true, false], 1);
        assert_eq!(sim.run(2000, [false, false], 1), 0);
    }

    #[test]
    fn next_deck_takes_over_quickly_after_a_stop() {
        let mut sim = Sim::new();
        sim.run(1000, [true, false], 1);
        sim.run(1000, [false, false], 1);
        assert_eq!(sim.run(500, [false, true], 1), 1);
    }
}
