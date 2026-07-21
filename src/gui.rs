use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use eframe::egui;
use tracing::{info, warn};
use tsclientlib::Identity;

use crate::config::Config;
use crate::timer::{self, Command, TimerCommand, TimerState};
use crate::ts3::{self, BotEvent, GuiBridge, GuiCommand, WhisperScope};
use crate::{audio, Args};

pub fn run() -> Result<()> {
    // Logs go to stderr as in CLI mode: visible when started from a
    // terminal, harmlessly discarded when double-clicked.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wwmts3gw=info".into()),
        )
        .with_target(false)
        .init();

    let config = Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 580.0])
            .with_min_inner_size([440.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "WWM Jungle Timer",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx, config.dark_mode);
            Ok(Box::new(App::new(config)))
        }),
    )
    .map_err(|e| anyhow!("failed to start GUI: {e}"))
}

/// Pins the theme (instead of following the OS) and uses full-contrast
/// text: pure black on light, pure white on dark.
fn apply_theme(ctx: &egui::Context, dark: bool) {
    let (theme, mut visuals, text) = if dark {
        (egui::Theme::Dark, egui::Visuals::dark(), egui::Color32::WHITE)
    } else {
        (egui::Theme::Light, egui::Visuals::light(), egui::Color32::BLACK)
    };
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke.color = text;
    }
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, visuals);
}

/// The bot connection as seen from the GUI thread.
enum Conn {
    Idle,
    Active {
        cmd_tx: tokio::sync::mpsc::UnboundedSender<GuiCommand>,
        events: std_mpsc::Receiver<BotEvent>,
        handle: JoinHandle<()>,
        /// False while the TeamSpeak handshake is still in progress.
        connected: bool,
        timer: TimerState,
        whispering: bool,
    },
}

struct App {
    config: Config,
    conn: Conn,
    start_at: String,
    error: Option<String>,
}

impl App {
    fn new(config: Config) -> Self {
        Self {
            config,
            conn: Conn::Idle,
            start_at: "30:00".into(),
            error: None,
        }
    }

    fn drain_events(&mut self) {
        let Conn::Active {
            events,
            connected,
            timer,
            whispering,
            ..
        } = &mut self.conn
        else {
            return;
        };

        let mut stopped = None;
        while let Ok(event) = events.try_recv() {
            match event {
                BotEvent::Connected => *connected = true,
                BotEvent::Timer(state) => *timer = state,
                BotEvent::Whispering(active) => *whispering = active,
                BotEvent::Stopped(error) => stopped = Some(error),
            }
        }

        if let Some(error) = stopped {
            self.error = error;
            if let Conn::Active { handle, .. } = std::mem::replace(&mut self.conn, Conn::Idle) {
                let _ = handle.join(); // the bot loop has already returned
            }
        }
    }

    fn send(cmd_tx: &tokio::sync::mpsc::UnboundedSender<GuiCommand>, command: Command) {
        // A send error means the bot thread died; the Stopped event handles that.
        let _ = cmd_tx.send(GuiCommand::Command(command));
    }

    fn connect(&mut self, ctx: egui::Context) {
        self.error = None;
        match self.spawn_bot(ctx) {
            Ok(conn) => self.conn = conn,
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn spawn_bot(&mut self, ctx: egui::Context) -> Result<Conn> {
        let cfg = &mut self.config;

        let server = cfg.server.trim().to_string();
        if server.is_empty() {
            bail!("server address is required");
        }
        let mut name = cfg.name.trim().to_string();
        if name.is_empty() {
            name = Config::default().name;
        }
        let channel_id = if cfg.use_channel_id && !cfg.channel_id.trim().is_empty() {
            Some(
                cfg.channel_id
                    .trim()
                    .parse::<u64>()
                    .context("channel id must be a number")?,
            )
        } else {
            None
        };
        let channel = (!cfg.use_channel_id && !cfg.channel_name.trim().is_empty())
            .then(|| cfg.channel_name.trim().to_string());
        let whisper_server_group_id = if cfg.whisper_enabled {
            let text = cfg.whisper_group_id.trim();
            if text.is_empty() {
                bail!("whisper group id is required when whisper is enabled");
            }
            Some(
                text.parse::<u64>()
                    .context("whisper group id must be a number")?,
            )
        } else {
            None
        };

        if cfg.identity.is_none() {
            info!("generating a new TeamSpeak identity (kept in the config file)");
            cfg.identity = Some(Identity::create());
        }
        let identity = cfg.identity.clone();

        if let Err(err) = cfg.save() {
            warn!("failed to save config: {err:#}");
        }

        let clips = load_clips(cfg)?;

        let args = Args {
            server: server.clone(),
            // Unused in GUI mode: clips are decoded above, from the embedded
            // data or the configured overrides.
            warn_60s: Default::default(),
            warn_30s: Default::default(),
            warn_15s: Default::default(),
            name,
            channel,
            channel_id,
            server_password: (!cfg.server_password.is_empty())
                .then(|| cfg.server_password.clone()),
            channel_password: (!cfg.channel_password.is_empty())
                .then(|| cfg.channel_password.clone()),
            identity_file: None,
            volume: cfg.volume,
            whisper_server_group_id,
            whisper_scope: cfg.whisper_scope,
        };

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = std_mpsc::channel();

        let sink_tx = event_tx.clone();
        let sink_ctx = ctx.clone();
        let events: ts3::EventSink = Box::new(move |event| {
            let _ = sink_tx.send(event);
            sink_ctx.request_repaint();
        });

        let handle = std::thread::spawn(move || {
            let bridge = GuiBridge {
                commands: cmd_rx,
                events,
            };
            let result = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to start async runtime")
                .and_then(|rt| rt.block_on(ts3::run(&args, identity, &clips, Some(bridge))));
            let _ = event_tx.send(BotEvent::Stopped(result.err().map(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });

        Ok(Conn::Active {
            cmd_tx,
            events: event_rx,
            handle,
            connected: false,
            timer: TimerState::Stopped,
            whispering: whisper_server_group_id.is_some(),
        })
    }

    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let idle = matches!(self.conn, Conn::Idle);
        ui.add_enabled_ui(idle, |ui| {
            ui.heading("Connection");
            egui::Grid::new("connection")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Server address");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.server)
                            .hint_text("ts.example.com or 1.2.3.4:9987"),
                    );
                    ui.end_row();

                    ui.label("Nickname");
                    ui.text_edit_singleline(&mut self.config.name);
                    ui.end_row();

                    ui.label("Server password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.server_password)
                            .password(true)
                            .hint_text("optional"),
                    );
                    ui.end_row();

                    ui.label("Channel");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.config.use_channel_id, false, "by name");
                        ui.radio_value(&mut self.config.use_channel_id, true, "by id");
                    });
                    ui.end_row();

                    ui.label("");
                    if self.config.use_channel_id {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.channel_id)
                                .hint_text("numeric channel id"),
                        );
                    } else {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.channel_name)
                                .hint_text("empty = server default channel"),
                        );
                    }
                    ui.end_row();

                    ui.label("Channel password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.channel_password)
                            .password(true)
                            .hint_text("optional"),
                    );
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.heading("Whisper");
            ui.checkbox(
                &mut self.config.whisper_enabled,
                "Whisper to a server group",
            );
            ui.add_enabled_ui(self.config.whisper_enabled, |ui| {
                egui::Grid::new("whisper")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Group ID");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.whisper_group_id)
                                .desired_width(100.0),
                        );
                        ui.end_row();

                        ui.label("Scope");
                        ui.horizontal(|ui| {
                            ui.radio_value(
                                &mut self.config.whisper_scope,
                                WhisperScope::AllChannels,
                                "all channels",
                            );
                            ui.radio_value(
                                &mut self.config.whisper_scope,
                                WhisperScope::CurrentChannel,
                                "bot's channel only",
                            );
                        });
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);
            ui.heading("Sounds");
            ui.add(egui::Slider::new(&mut self.config.volume, 0.0..=2.0).text("volume"));
            ui.add_space(4.0);
            egui::Grid::new("clips")
                .num_columns(3)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    for (label, path) in [
                        ("60 s warning", &mut self.config.clip_60),
                        ("30 s warning", &mut self.config.clip_30),
                        ("15 s warning", &mut self.config.clip_15),
                    ] {
                        ui.label(label);
                        if ui.button("Browse…").clicked() {
                            if let Some(file) = rfd::FileDialog::new()
                                .add_filter("audio", &["wav", "mp3", "ogg", "flac", "m4a"])
                                .pick_file()
                            {
                                *path = file.display().to_string();
                            }
                        }
                        let field = ui.add(
                            egui::TextEdit::singleline(path).desired_width(f32::INFINITY),
                        );
                        if !path.is_empty() {
                            field.on_hover_text(path.as_str());
                        }
                        ui.end_row();
                    }
                });
        });

        ui.add_space(10.0);
        match &self.conn {
            Conn::Idle => {
                let button = egui::Button::new(
                    egui::RichText::new("▶  Connect & start bot").size(15.0),
                );
                if ui.add_sized([ui.available_width(), 34.0], button).clicked() {
                    self.connect(ui.ctx().clone());
                }
            }
            Conn::Active {
                cmd_tx, connected, ..
            } => {
                ui.horizontal(|ui| {
                    let status = if *connected {
                        format!("● connected to {}", self.config.server.trim())
                    } else {
                        "● connecting …".into()
                    };
                    let color = if *connected {
                        egui::Color32::from_rgb(80, 190, 80)
                    } else {
                        egui::Color32::from_rgb(220, 180, 60)
                    };
                    ui.colored_label(color, status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Disconnect").clicked() {
                            let _ = cmd_tx.send(GuiCommand::Disconnect);
                        }
                    });
                });
            }
        }
    }

    fn timer_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Timer");

        // Copy what the UI needs out of self.conn so the closures below can
        // freely borrow the input fields.
        let (cmd_tx, connected, timer, whispering) = match &self.conn {
            Conn::Active {
                cmd_tx,
                connected,
                timer,
                whispering,
                ..
            } => (Some(cmd_tx.clone()), *connected, *timer, *whispering),
            Conn::Idle => (None, false, TimerState::Stopped, false),
        };

        if !connected {
            ui.label("Connect first — then control the timer here or with '!jungle' chat commands.");
            return;
        }
        let Some(cmd_tx) = cmd_tx else { return };

        ui.label(timer::status_message(&timer));
        ui.add_space(4.0);

        let mut parse_error = None;
        ui.horizontal(|ui| {
            if ui.button("▶ Start").clicked() {
                match parse_start(&self.start_at, &self.config.start_delay) {
                    Ok(command) => Self::send(&cmd_tx, command),
                    Err(err) => parse_error = Some(err),
                }
            }
            ui.label("at game time");
            ui.add(egui::TextEdit::singleline(&mut self.start_at).desired_width(56.0));
            ui.label("delayed by");
            ui.add(
                egui::TextEdit::singleline(&mut self.config.start_delay)
                    .desired_width(56.0)
                    .hint_text("0:00"),
            );
            if ui.button("■ Stop").clicked() {
                Self::send(&cmd_tx, Command::Timer(TimerCommand::Stop));
            }
        });

        if self.config.whisper_enabled {
            ui.horizontal(|ui| {
                ui.label("Output:");
                let mut whisper_now = whispering;
                ui.radio_value(&mut whisper_now, true, "group whisper");
                ui.radio_value(&mut whisper_now, false, "current channel");
                if whisper_now != whispering {
                    let command = if whisper_now {
                        Command::Group
                    } else {
                        Command::Channel
                    };
                    Self::send(&cmd_tx, command);
                }
            });
        }

        if let Some(err) = parse_error {
            self.error = Some(err);
        }

        // Keep the countdown/remaining display ticking.
        if !matches!(timer, TimerState::Stopped) {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// Builds a start command from the two text fields, mirroring
/// `!jungle start [MM:SS] [at MM:SS]`.
fn parse_start(at: &str, delay: &str) -> Result<Command, String> {
    let at = at.trim();
    let elapsed = if at.is_empty() {
        Duration::ZERO
    } else {
        timer::parse_gametime(at)?
    };
    let delay = delay.trim();
    let delay = if delay.is_empty() {
        Duration::ZERO
    } else {
        timer::parse_mmss(delay)?
    };
    Ok(Command::Timer(TimerCommand::Start { elapsed, delay }))
}

/// Decodes the three configured announcement clips.
fn load_clips(cfg: &Config) -> Result<Vec<(u64, Vec<f32>)>> {
    let sources = [
        (60u64, &cfg.clip_60),
        (30, &cfg.clip_30),
        (15, &cfg.clip_15),
    ];
    let mut clips = Vec::with_capacity(sources.len());
    for (offset, path) in sources {
        let path = path.trim();
        if path.is_empty() {
            bail!("no sound file set for the {offset} s warning");
        }
        let pcm = audio::decode_clip(Path::new(path), cfg.volume)
            .with_context(|| format!("failed to load the {offset}s clip from {path}"))?;
        clips.push((offset, pcm));
    }
    Ok(clips)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        let mut dark = self.config.dark_mode;
                        if ui
                            .selectable_label(dark, "🌙 dark")
                            .on_hover_text("dark mode")
                            .clicked()
                        {
                            dark = true;
                        }
                        if ui
                            .selectable_label(!dark, "☀ light")
                            .on_hover_text("light mode")
                            .clicked()
                        {
                            dark = false;
                        }
                        if dark != self.config.dark_mode {
                            self.config.dark_mode = dark;
                            apply_theme(ui.ctx(), dark);
                            if let Err(err) = self.config.save() {
                                warn!("failed to save config: {err:#}");
                            }
                        }
                    });
                    self.form_ui(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    self.timer_ui(ui);

                    if let Some(err) = self.error.clone() {
                        ui.add_space(8.0);
                        ui.colored_label(egui::Color32::from_rgb(220, 70, 70), err);
                    }
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Persist whatever is in the form, so edits survive a close without
        // an intervening connect.
        if let Err(err) = self.config.save() {
            warn!("failed to save config: {err:#}");
        }

        // Ask the bot to disconnect cleanly, but don't hang the window on a
        // stuck connection: wait at most ~4 s (the bot itself gives up on an
        // unacknowledged disconnect after 3 s).
        if let Conn::Active { cmd_tx, handle, .. } =
            std::mem::replace(&mut self.conn, Conn::Idle)
        {
            let _ = cmd_tx.send(GuiCommand::Disconnect);
            for _ in 0..40 {
                if handle.is_finished() {
                    let _ = handle.join();
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
