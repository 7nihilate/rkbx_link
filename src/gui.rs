// Status window: Rekordbox connection, the master deck, where OSC goes, and the log.
// Runs on the main thread; the BeatKeeper runs on a worker thread and talks to
// this window only through crate::status::shared().

use std::fs;
use std::net::Ipv4Addr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::status::{shared, LogKind, OscMode, OscTarget};

const CONFIG_PATH: &str = "config";

struct LanInterface {
    name: String,
    ip: Ipv4Addr,
    broadcast: Ipv4Addr,
}

fn lan_interfaces() -> Vec<LanInterface> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return vec![];
    };
    interfaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some(LanInterface {
                name: i.name.clone(),
                ip: v4.ip,
                broadcast: Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)),
            }),
            _ => None,
        })
        .collect()
}

pub struct App {
    mode: OscMode,
    port: u16,
    lan_destination: String,
    interfaces: Vec<LanInterface>,
    message: String,
    last_packets: u64,
    last_sample: Instant,
    packets_per_second: f64,
}

impl App {
    fn new() -> Self {
        let target = shared().osc_target().unwrap_or(OscTarget {
            mode: OscMode::Local,
            port: 4460,
            lan_destination: "192.168.50.255:4460".to_string(),
        });
        App {
            mode: target.mode,
            port: target.port,
            lan_destination: target.lan_destination,
            interfaces: lan_interfaces(),
            message: String::new(),
            last_packets: 0,
            last_sample: Instant::now(),
            packets_per_second: 0.0,
        }
    }

    fn edited_target(&self) -> OscTarget {
        OscTarget {
            mode: self.mode,
            port: self.port,
            lan_destination: self.lan_destination.trim().to_string(),
        }
    }

    fn apply(&mut self) {
        let target = self.edited_target();
        match target.resolve() {
            Ok(addr) => {
                shared().set_osc_target(target);
                self.message = format!("Sending to {addr}");
            }
            Err(e) => self.message = e,
        }
    }

    fn save(&mut self) {
        let target = self.edited_target();
        if let Err(e) = target.resolve() {
            self.message = e;
            return;
        }
        match save_target_to_config(&target) {
            Ok(()) => {
                shared().set_osc_target(target);
                self.message = "Applied and saved to config".to_string();
            }
            Err(e) => self.message = format!("Could not save config: {e}"),
        }
    }

    fn sample_packet_rate(&mut self) {
        let elapsed = self.last_sample.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let packets = shared().packets_sent.load(Ordering::Relaxed);
            self.packets_per_second =
                (packets - self.last_packets) as f64 / elapsed.as_secs_f64();
            self.last_packets = packets;
            self.last_sample = Instant::now();
        }
    }
}

/// Rewrites osc.mode, osc.port and osc.destination in place, keeping the rest
/// of the file (comments included). Missing keys go after osc.enabled.
fn save_target_to_config(target: &OscTarget) -> std::io::Result<()> {
    let src = fs::read_to_string(CONFIG_PATH).unwrap_or_default();
    let newline = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let values = [
        ("osc.mode", target.mode.as_str().to_string()),
        ("osc.port", target.port.to_string()),
        ("osc.destination", target.lan_destination.clone()),
    ];
    let mut written = [false; 3];
    let mut lines: Vec<String> = src
        .lines()
        .map(|line| {
            let key = line.trim().split(' ').next().unwrap_or("");
            match values.iter().position(|(k, _)| *k == key) {
                Some(i) => {
                    written[i] = true;
                    format!("{} {}", values[i].0, values[i].1)
                }
                None => line.to_string(),
            }
        })
        .collect();
    let insert_at = lines
        .iter()
        .position(|l| l.trim().starts_with("osc.enabled"))
        .map(|i| i + 1)
        .unwrap_or(lines.len());
    for (i, (key, value)) in values.iter().enumerate().rev() {
        if !written[i] {
            lines.insert(insert_at, format!("{key} {value}"));
        }
    }
    let mut out = lines.join(newline);
    out.push_str(newline);
    fs::write(CONFIG_PATH, out)
}

fn dot(ui: &mut egui::Ui, on: bool, colour: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let fill = if on { colour } else { ui.visuals().widgets.inactive.bg_fill };
    ui.painter().circle_filled(rect.center(), 5.0, fill);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sample_packet_rate();
        let status = shared().status.lock().map(|s| s.clone()).unwrap_or_default();

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                dot(ui, status.rekordbox_connected, egui::Color32::from_rgb(80, 200, 120));
                ui.strong(if status.rekordbox_connected {
                    "Rekordbox connected"
                } else {
                    "Looking for Rekordbox..."
                });
                if !status.target_version.is_empty() {
                    ui.weak(format!("(offsets for {})", status.target_version));
                }
            });
            ui.separator();

            ui.label(egui::RichText::new(if status.title.is_empty() {
                "No track".to_string()
            } else {
                status.title.clone()
            })
            .size(16.0)
            .strong());
            ui.label(&status.artist);
            ui.horizontal(|ui| {
                ui.label(format!("{:.1} bpm", status.bpm));
                ui.add_space(12.0);
                let beat_in_bar = (status.beat.floor() as i64).rem_euclid(4);
                for i in 0..4 {
                    dot(ui, i == beat_in_bar, egui::Color32::from_rgb(255, 170, 60));
                }
            });
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Phrase: {}",
                    if status.phrase.is_empty() { "-" } else { &status.phrase }
                ));
                if !status.next_phrase.is_empty() {
                    ui.weak(format!("next {} in {} beats", status.next_phrase, status.countin));
                }
            });
            ui.separator();

            ui.strong("OSC output");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode, OscMode::Local, "This PC");
                ui.radio_value(&mut self.mode, OscMode::Lan, "Network");
            });
            match self.mode {
                OscMode::Local => {
                    ui.horizontal(|ui| {
                        ui.label("127.0.0.1 port");
                        ui.add(egui::DragValue::new(&mut self.port).range(1..=65535));
                    });
                }
                OscMode::Lan => {
                    ui.horizontal(|ui| {
                        ui.label("Target");
                        ui.text_edit_singleline(&mut self.lan_destination);
                    });
                    ui.horizontal(|ui| {
                        let port = self
                            .lan_destination
                            .rsplit(':')
                            .next()
                            .and_then(|p| p.parse::<u16>().ok())
                            .unwrap_or(self.port);
                        egui::ComboBox::from_id_salt("interfaces")
                            .selected_text("Broadcast on...")
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for i in &self.interfaces {
                                    let text = format!("{} ({}) -> {}", i.name, i.ip, i.broadcast);
                                    if ui.selectable_label(false, text).clicked() {
                                        self.lan_destination = format!("{}:{port}", i.broadcast);
                                    }
                                }
                            });
                        if ui.button("Refresh").clicked() {
                            self.interfaces = lan_interfaces();
                        }
                    });
                }
            }
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    self.apply();
                }
                if ui.button("Save to config").clicked() {
                    self.save();
                }
                if ui.button("Resend state").clicked() {
                    shared().resend_requested.store(true, Ordering::SeqCst);
                }
                if ui.button("Reconnect to Rekordbox").clicked() {
                    shared().reconnect_requested.store(true, Ordering::SeqCst);
                }
            });
            let current = shared()
                .osc_target()
                .and_then(|t| t.resolve().ok())
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".to_string());
            ui.label(format!(
                "Sending to {current}, {:.0} packets/s",
                self.packets_per_second
            ));
            if !status.last_osc_error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(230, 90, 80), &status.last_osc_error);
            }
            if !self.message.is_empty() {
                ui.weak(&self.message);
            }
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.strong("Log");
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Ok(log) = shared().log.lock() {
                        for (kind, line) in log.iter() {
                            let colour = match kind {
                                LogKind::Debug => egui::Color32::from_rgb(90, 180, 200),
                                LogKind::Good => egui::Color32::from_rgb(80, 200, 120),
                                LogKind::Info => ui.visuals().text_color(),
                                LogKind::Warning => egui::Color32::from_rgb(230, 190, 60),
                                LogKind::Error => egui::Color32::from_rgb(230, 90, 80),
                            };
                            ui.label(egui::RichText::new(line).monospace().color(colour));
                        }
                    }
                });
        });

        ctx.request_repaint_after(Duration::from_millis(66));
    }
}

/// Blocks until the window is closed.
pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rkbx_link")
            .with_inner_size([460.0, 620.0])
            .with_min_inner_size([360.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("rkbx_link", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
