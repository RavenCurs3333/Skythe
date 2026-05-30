#[cfg(feature = "gui_egui")]
fn main() {
    // ── Single-instance IPC via Unix domain socket ──────────────────────
    // Collect any file paths passed as command-line arguments.
    // Handles both plain paths and file:// URIs (some file managers
    // pass URIs even with the %F desktop-file field).
    let initial_files: Vec<std::path::PathBuf> = std::env::args()
        .skip(1)
        .map(|arg| {
            if let Some(rest) = arg.strip_prefix("file://") {
                // Percent-decode the URI and convert to a PathBuf.
                percent_decode_to_path(rest)
            } else {
                std::path::PathBuf::from(arg)
            }
        })
        .collect();

    // Try to connect to an existing Skythe instance
    let socket_path = ipc_socket_path();
    let already_running = if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket_path) {
        // Send our files to the running instance
        use std::io::Write;
        for file in &initial_files {
            let path = file.to_string_lossy();
            if writeln!(stream, "{path}").is_err() {
                break;
            }
        }
        true
    } else {
        false
    };

    if already_running {
        // The other instance will handle our files; we exit.
        return;
    }

    // ── We are the main instance. Start the IPC listener. ──────────────
    let _ipc_listener = start_ipc_listener(socket_path);

    eframe::run_native(
        "Skythe",
        eframe::NativeOptions::default(),
        Box::new(move |_cc| Box::new(crate::ui::PlayerApp::new(initial_files))),
    )
    .unwrap();
}

#[cfg(not(feature = "gui_egui"))]
fn main() {
    println!("Run with feature 'gui_egui' to launch the GUI.");
}

// ── IPC helpers ────────────────────────────────────────────────────────────
fn ipc_socket_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_owned());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    std::path::PathBuf::from(&base).join(format!("skythe-{user}.sock"))
}

fn start_ipc_listener(socket_path: std::path::PathBuf) -> Option<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = match std::os::unix::net::UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(_) => return None,
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666));
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(&stream);
            let mut paths = Vec::new();
            for line in reader.lines().flatten() {
                let p = std::path::PathBuf::from(line.trim());
                if p.exists() {
                    paths.push(p);
                }
            }
            if !paths.is_empty() {
                if let Ok(sender) = ipc_channel().lock() {
                    if let Some(ref sender) = *sender {
                        let _ = sender.send(paths);
                    }
                }
            }
        }
    });
    Some(handle)
}

fn ipc_channel() -> &'static std::sync::Mutex<Option<std::sync::mpsc::Sender<Vec<std::path::PathBuf>>>> {
    use std::sync::OnceLock;
    static CHANNEL: OnceLock<std::sync::Mutex<Option<std::sync::mpsc::Sender<Vec<std::path::PathBuf>>>>> = OnceLock::new();
    CHANNEL.get_or_init(|| std::sync::Mutex::new(None))
}

/// Percent-decode a file:// URI into a PathBuf.
/// On Unix, file paths are byte sequences (not necessarily UTF-8), so we
/// decode to raw bytes and use OsString to preserve every byte faithfully.
fn percent_decode_to_path(input: &str) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let mut result = Vec::with_capacity(input.len());
    let mut iter = input.bytes();
    while let Some(b) = iter.next() {
        match b {
            b'%' => {
                let hi = iter.next().unwrap_or(b'0');
                let lo = iter.next().unwrap_or(b'0');
                result.push((hex_val(hi) << 4) | hex_val(lo));
            }
            b'+' => result.push(b' '),
            _ => result.push(b),
        }
    }
    std::ffi::OsString::from_vec(result).into()
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(feature = "gui_egui")]
mod ui {
    use eframe::egui::{self, pos2, vec2, Color32, RichText, Sense, Stroke};
    use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
    use std::fs::{self, File};
    use std::io::BufReader;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MIN_WINDOW: egui::Vec2 = vec2(980.0, 640.0);

    /// Shared ring buffer for capturing live audio samples for EQ visualization.
    struct SampleBuffer {
        samples: Vec<f32>,
        write_pos: usize,
        full: bool,
        sample_rate: u32,
    }

    impl SampleBuffer {
        fn new(sample_rate: u32) -> Self {
            Self {
                samples: vec![0.0; 1024],
                write_pos: 0,
                full: false,
                sample_rate,
            }
        }

        fn push_sample(&mut self, sample: f32) {
            let len = self.samples.len();
            self.samples[self.write_pos] = sample;
            self.write_pos = (self.write_pos + 1) % len;
            if self.write_pos == 0 { self.full = true; }
        }

        /// Compute band energies (0..1) for N_LOG_BANDS logarithmically spaced bands.
        fn compute_bands(&self, n_bands: usize) -> Vec<f32> {
            if !self.full { return vec![0.0; n_bands]; }
            let n = self.samples.len();
            let sr = self.sample_rate as f32;
            // logarithmic center frequencies from 50Hz to 20kHz
            let mut centers = Vec::with_capacity(n_bands);
            for i in 0..n_bands {
                let f = 50.0 * (400.0_f32).powf(i as f32 / (n_bands as f32 - 1.0));
                let k = (f * n as f32 / sr) as usize;
                centers.push(k.min(n / 2));
            }
            // Compute real-valued DFT magnitude at each band center using ±1 bandwidth
            let mut result = Vec::with_capacity(n_bands);
            for &k in &centers {
                let bw = (k / 4).max(2);
                let k0 = k.saturating_sub(bw);
                let k1 = (k + bw).min(n / 2);
                let mut energy = 0.0_f32;
                for ki in k0..=k1 {
                    let mut re = 0.0_f32;
                    let mut im = 0.0_f32;
                    for j in 0..n {
                        let angle = 2.0 * std::f32::consts::PI * ki as f32 * j as f32 / n as f32;
                        re += self.samples[j] * angle.cos();
                        im += self.samples[j] * angle.sin();
                    }
                    energy += (re * re + im * im).sqrt();
                }
                // normalize and apply perceptual scaling
                let mag = (energy / (n as f32 * 3.0)).sqrt().min(1.0);
                result.push(mag);
            }
            result
        }
    }

    // SampleTap wrapper — captures raw PCM samples into the shared buffer for EQ visualization
    struct SampleTap {
        inner: Box<dyn Source<Item = i16> + Send>,
        buffer: Arc<Mutex<SampleBuffer>>,
    }

    impl Source for SampleTap {
        fn current_frame_len(&self) -> Option<usize> { self.inner.current_frame_len() }
        fn channels(&self) -> u16 { self.inner.channels() }
        fn sample_rate(&self) -> u32 { self.inner.sample_rate() }
        fn total_duration(&self) -> Option<Duration> { self.inner.total_duration() }
    }

    impl Iterator for SampleTap {
        type Item = i16;
        fn next(&mut self) -> Option<Self::Item> {
            let sample = self.inner.next()?;
            let f32_val = sample as f32 / 32768.0;
            if let Ok(mut buf) = self.buffer.lock() {
                buf.push_sample(f32_val);
            }
            Some(sample)
        }
    }

    #[derive(Clone)]
    struct TrackEntry {
        path: PathBuf,
        title: String,
        artist: String,
        duration: Option<Duration>,
    }

    pub struct PlayerApp {
        playing: bool,
        repeat_mode: RepeatMode,
        shuffle: bool,
        shuffle_order: Vec<usize>,
        shuffle_pos: usize,
        pending_play: Option<PendingPlay>,
        progress: f32,
        volume: f32,
        muted: bool,
        saved_volume: f32,
        title: String,
        artist: String,
        file_path: Option<PathBuf>,
        duration: Option<Duration>,
        paused_elapsed: Duration,
        started_at: Option<Instant>,
        stream: Option<OutputStream>,
        stream_handle: Option<OutputStreamHandle>,
        sink: Option<Sink>,
        status: String,
        fullscreen: bool,
        settings_open: bool,
        settings_tab: SettingsTab,
        background_theme: usize,
        accent_theme: usize,
        appearance_options: [bool; 10],
        bar_animation: usize,
        audio_buffer: Arc<Mutex<SampleBuffer>>,
        queue: Vec<TrackEntry>,
        current_index: Option<usize>,
        startup_files: Vec<PathBuf>,
        selected_font: String,
        available_fonts: Vec<String>,
        ipc_receiver: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    }

    struct PendingPlay {
        index: usize,
        due_at: Instant,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RepeatMode {
        Off,
        One,
        All,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SettingsTab {
        Appearance,
        Playback,
        Queue,
        Audio,
        Library,
        Shortcuts,
        Integrations,
        Privacy,
        Advanced,
        About,
    }

    impl SettingsTab {
        fn label(self) -> &'static str {
            match self {
                Self::Appearance => "Appearance",
                Self::Playback => "Playback",
                Self::Queue => "Queue",
                Self::Audio => "Audio",
                Self::Library => "Library",
                Self::Shortcuts => "Shortcuts",
                Self::Integrations => "Integrations",
                Self::Privacy => "Privacy",
                Self::Advanced => "Advanced",
                Self::About => "About",
            }
        }
    }

    impl PlayerApp {
        pub fn new(startup_files: Vec<PathBuf>) -> Self {
            let (ipc_sender, ipc_receiver) = std::sync::mpsc::channel();
            if let Ok(mut guard) = crate::ipc_channel().lock() {
                *guard = Some(ipc_sender);
            }

            let mut app = Self {
                playing: false,
                repeat_mode: RepeatMode::All,
                shuffle: false,
                shuffle_order: Vec::new(),
                shuffle_pos: 0,
                pending_play: None,
                progress: 0.0,
                volume: 0.8,
                muted: false,
                saved_volume: 0.8,
                title: "No track selected".to_owned(),
                artist: "Add audio files to the queue".to_owned(),
                file_path: None,
                duration: None,
                paused_elapsed: Duration::ZERO,
                started_at: None,
                stream: None,
                stream_handle: None,
                sink: None,
                status: String::new(),
                fullscreen: false,
                settings_open: false,
                settings_tab: SettingsTab::Appearance,
                background_theme: 0,
                accent_theme: 0,
                appearance_options: [true, true, true, true, true, false, true, true, false, true],
                bar_animation: 0,
                audio_buffer: Arc::new(Mutex::new(SampleBuffer::new(44100))),
                queue: Vec::new(),
                current_index: None,
                startup_files,
                selected_font: "Default".to_owned(),
                available_fonts: Vec::new(),
                ipc_receiver,
            };
            app.reseed_shuffle();
            app.scan_fonts();
            app
        }

        fn reseed_shuffle(&mut self) {
            if self.shuffle && !self.queue.is_empty() {
                let current = self.current_index.unwrap_or(0);
                self.shuffle_order = self.generate_shuffle_order(current);
                self.shuffle_pos = 0;
            } else {
                self.shuffle_order.clear();
                self.shuffle_pos = 0;
            }
        }

        /// Generate a Fisher-Yates shuffle of queue indices starting at the given seed index.
        fn generate_shuffle_order(&self, start_index: usize) -> Vec<usize> {
            let len = self.queue.len();
            if len <= 1 {
                return (0..len).collect();
            }
            let mut order: Vec<usize> = (0..len).collect();
            // Simple LCG for deterministic but well-distributed shuffling
            let seed = start_index.wrapping_mul(314159).wrapping_add(271828) & 0x7FFFFFFF;
            let mut rng_state = seed;
            // Fisher-Yates shuffle from the end
            for i in (1..len).rev() {
                rng_state = rng_state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFFFFFF;
                let j = (rng_state >> 16) % (i as u32 + 1) as usize;
                order.swap(i, j);
            }
            // Ensure the first entry is not start_index (don't repeat the same track)
            if len > 1 && order[0] == start_index {
                order.swap(0, 1);
            }
            order
        }

        fn advance_shuffle(&mut self) -> Option<usize> {
            if self.queue.is_empty() {
                return None;
            }
            // Regenerate shuffle order if we've exhausted it
            if self.shuffle_pos >= self.shuffle_order.len() {
                let current = self.current_index.unwrap_or(0);
                self.shuffle_order = self.generate_shuffle_order(current);
                self.shuffle_pos = 0;
            }
            let next = *self.shuffle_order.get(self.shuffle_pos)?;
            self.shuffle_pos += 1;
            Some(next)
        }

        fn scan_fonts(&mut self) {
            let mut fonts = vec!["Default".to_owned()];
            let home = std::env::var("HOME").unwrap_or_default();
            let user_fonts = format!("{}/.fonts", &home);
            let user_local_fonts = format!("{}/.local/share/fonts", &home);
            let font_dirs = vec![
                "/usr/share/fonts",
                "/usr/local/share/fonts",
                user_fonts.as_str(),
                user_local_fonts.as_str(),
            ];
            let mut seen = std::collections::HashSet::new();
            for dir in font_dirs {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let ext = path.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_lowercase())
                            .unwrap_or_default();
                        if ext == "ttf" || ext == "otf" {
                            if let Some(name) = path.file_stem()
                                .and_then(|n| n.to_str())
                                .map(|n| n.to_string())
                            {
                                if seen.insert(name.clone()) {
                                    fonts.push(name);
                                }
                            }
                        }
                    }
                }
            }
            self.available_fonts = fonts;
        }

        fn apply_font(&mut self, ctx: &egui::Context) {
            if self.selected_font == "Default" {
                ctx.set_fonts(egui::FontDefinitions::default());
                return;
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let user_fonts = format!("{}/.fonts", &home);
            let user_local_fonts = format!("{}/.local/share/fonts", &home);
            let font_dirs = vec![
                "/usr/share/fonts",
                "/usr/local/share/fonts",
                user_fonts.as_str(),
                user_local_fonts.as_str(),
            ];
            let target_name = self.selected_font.to_lowercase();
            for dir in font_dirs {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let ext = path.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_lowercase())
                            .unwrap_or_default();
                        if ext == "ttf" || ext == "otf" {
                            if let Some(name) = path.file_stem()
                                .and_then(|n| n.to_str())
                                .map(|n| n.to_lowercase())
                            {
                                if name == target_name {
                                    if let Ok(data) = std::fs::read(&path) {
                                        let mut fonts = egui::FontDefinitions::default();
                                        fonts.font_data.insert(
                                            "user_font".into(),
                                            egui::FontData::from_owned(data),
                                        );
                                        fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap()
                                            .insert(0, "user_font".into());
                                        fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap()
                                            .insert(0, "user_font".into());
                                        ctx.set_fonts(fonts);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }

        fn check_ipc_messages(&mut self) {
            while let Ok(paths) = self.ipc_receiver.try_recv() {
                let first_new = self.add_paths_and_folder_audio(paths);
                if first_new < self.queue.len() {
                    if let Err(err) = self.play_queue_index(first_new, Duration::ZERO, true) {
                        self.status = format!("Could not play file: {err}");
                    }
                }
            }
        }

        fn load_startup_files(&mut self) {
            if self.startup_files.is_empty() {
                return;
            }
            let files = std::mem::take(&mut self.startup_files);
            let first_new = self.add_paths_and_folder_audio(files);
            if first_new < self.queue.len() {
                if let Err(err) = self.play_queue_index(first_new, Duration::ZERO, true) {
                    self.status = format!("Could not play file: {err}");
                }
            }
        }

        fn open_file_dialog(&mut self) {
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter("Audio", &["flac", "mp3", "ogg", "wav"])
                .add_filter("All files", &["*"])
                .pick_files()
            {
                let first_new = self.add_paths_and_folder_audio(paths);
                if self.current_index.is_none() && first_new < self.queue.len() {
                    if let Err(err) = self.play_queue_index(first_new, Duration::ZERO, true) {
                        self.status = format!("Could not play file: {err}");
                    }
                } else if first_new < self.queue.len() {
                    self.status = format!("Added {} track(s)", self.queue.len() - first_new);
                }
            }
        }

        fn add_paths_and_folder_audio(&mut self, paths: Vec<PathBuf>) -> usize {
            let first_new = self.queue.len();
            let mut candidates = Vec::new();
            for path in paths {
                if is_audio_file(&path) {
                    candidates.push(path.clone());
                }
                if let Some(parent) = path.parent() {
                    if let Ok(entries) = fs::read_dir(parent) {
                        let mut folder_paths = entries
                            .filter_map(|entry| entry.ok())
                            .map(|entry| entry.path())
                            .filter(|path| is_audio_file(path))
                            .collect::<Vec<_>>();
                        folder_paths.sort_by_key(|path| display_title(path).to_lowercase());
                        candidates.extend(folder_paths);
                    }
                }
            }
            for path in candidates {
                if !self.queue.iter().any(|track| track.path == path) {
                    self.queue.push(track_from_path(path));
                }
            }
            first_new
        }

        fn play_queue_index(
            &mut self,
            index: usize,
            offset: Duration,
            play_immediately: bool,
        ) -> anyhow::Result<()> {
            self.pending_play = None;
            let Some(entry) = self.queue.get(index).cloned() else {
                self.status = "No queued track at that position".to_owned();
                return Ok(());
            };
            self.load_audio_file(entry.path.clone(), offset, play_immediately)?;
            self.current_index = Some(index);
            if let Some(queue_entry) = self.queue.get_mut(index) {
                queue_entry.duration = self.duration;
            }
            Ok(())
        }

        fn load_audio_file(&mut self, path: PathBuf, offset: Duration, play_immediately: bool) -> anyhow::Result<()> {
            self.stop_sink();
            if self.stream_handle.is_none() {
                let (stream, stream_handle) = OutputStream::try_default()?;
                self.stream = Some(stream);
                self.stream_handle = Some(stream_handle);
            }
            let (raw_source, duration) = open_source(&path, offset)?;
            self.duration = duration;
            // Wrap source with SampleTap to capture live samples for EQ visualization
            {
                if let Ok(mut buf) = self.audio_buffer.lock() {
                    let sr = raw_source.sample_rate();
                    *buf = SampleBuffer::new(sr);
                }
            }
            let tapped_source = SampleTap { inner: raw_source, buffer: Arc::clone(&self.audio_buffer) };
            let handle = self.stream_handle.as_ref().ok_or_else(|| anyhow::anyhow!("audio output was not initialized"))?;
            let sink = Sink::try_new(handle)?;
            sink.set_volume(self.volume);
            sink.append(tapped_source);
            if play_immediately {
                sink.play();
                self.started_at = Some(Instant::now());
            } else {
                sink.pause();
                self.started_at = None;
            }
            self.title = display_title(&path);
            self.artist = parent_name(&path);
            self.file_path = Some(path);
            self.paused_elapsed = offset;
            self.progress = 0.0;
            self.playing = play_immediately;
            self.status.clear();
            self.sink = Some(sink);
            self.sync_progress();
            Ok(())
        }

        fn stop_sink(&mut self) {
            if let Some(sink) = self.sink.take() {
                sink.stop();
            }
            self.playing = false;
            self.started_at = None;
        }

        fn stop_current(&mut self) {
            self.pending_play = None;
            self.stop_sink();
            self.paused_elapsed = Duration::ZERO;
            self.progress = 0.0;
        }

        fn toggle_playback(&mut self) {
            if self.pending_play.is_some() {
                self.pending_play = None;
            }
            let Some(sink) = self.sink.as_ref() else {
                if let Some(index) = self.current_index {
                    if let Err(err) = self.play_queue_index(index, Duration::ZERO, true) {
                        self.status = format!("Could not play track: {err}");
                    }
                } else {
                    self.status = "Add a track with + first".to_owned();
                }
                return;
            };
            if self.playing {
                sink.pause();
                self.paused_elapsed = self.elapsed().min_duration(self.duration);
                self.started_at = None;
                self.playing = false;
            } else {
                if self.progress >= 0.999 || self.duration.is_some_and(|duration| self.elapsed() >= duration) {
                    if let Some(index) = self.current_index {
                        if let Err(err) = self.play_queue_index(index, Duration::ZERO, true) {
                            self.status = format!("Could not restart track: {err}");
                        }
                    }
                    return;
                }
                sink.play();
                self.started_at = Some(Instant::now());
                self.playing = true;
            }
        }

        fn restart_track(&mut self) {
            if let Some(index) = self.current_index {
                if let Err(err) = self.play_queue_index(index, Duration::ZERO, true) {
                    self.status = format!("Could not restart track: {err}");
                }
            } else {
                self.status = "Add a track with + first".to_owned();
            }
        }

        fn seek_to_progress(&mut self, progress: f32) {
            let Some(index) = self.current_index else {
                self.progress = 0.0;
                self.status = "Add a track with + first".to_owned();
                return;
            };
            let Some(duration) = self.duration else { return; };
            let offset = duration.mul_f32(progress.clamp(0.0, 1.0));
            let should_play = self.playing;
            if let Err(err) = self.play_queue_index(index, offset, should_play) {
                self.status = format!("Could not seek: {err}");
            }
        }

        fn toggle_repeat(&mut self) {
            self.repeat_mode = match self.repeat_mode {
                RepeatMode::Off => RepeatMode::One,
                RepeatMode::One => RepeatMode::All,
                RepeatMode::All => RepeatMode::Off,
            };
        }

        fn toggle_shuffle(&mut self) {
            self.shuffle = !self.shuffle;
            if self.shuffle {
                if !self.queue.is_empty() {
                    let current = self.current_index.unwrap_or(0);
                    self.shuffle_order = self.generate_shuffle_order(current);
                    self.shuffle_pos = 0;
                }
            } else {
                self.shuffle_order.clear();
                self.shuffle_pos = 0;
            }
        }

        fn previous_track(&mut self) {
            if self.elapsed() > Duration::from_secs(3) {
                self.restart_track();
                return;
            }
            let Some(index) = self.current_index else {
                self.status = "Add a track with + first".to_owned();
                return;
            };
            let previous = index.saturating_sub(1);
            if let Err(err) = self.play_queue_index(previous, Duration::ZERO, true) {
                self.status = format!("Could not play previous track: {err}");
            }
        }

        fn next_track(&mut self) {
            let next = if self.shuffle && self.queue.len() > 1 {
                self.advance_shuffle()
            } else {
                self.next_index()
            };
            let Some(next) = next else {
                self.stop_current();
                self.status = "End of queue".to_owned();
                return;
            };
            if let Err(err) = self.play_queue_index(next, Duration::ZERO, true) {
                self.status = format!("Could not play next track: {err}");
            }
        }

        fn next_index(&self) -> Option<usize> {
            let current = self.current_index?;
            if self.queue.is_empty() { return None; }
            if self.shuffle && self.queue.len() > 1 {
                // Use the pre-generated shuffle order to determine next track
                if let Some(&next) = self.shuffle_order.get(self.shuffle_pos) {
                    return Some(next);
                }
                // If we've exhausted the shuffle order, reshuffle (advance_shuffle handles this)
                return None;
            }
            let next = current + 1;
            (next < self.queue.len()).then_some(next)
        }

        fn share_path(&mut self, ctx: &egui::Context) {
            if let Some(path) = self.file_path.as_ref() {
                ctx.output_mut(|output| output.copied_text = path.display().to_string());
                self.status = "Track path copied".to_owned();
            } else {
                self.status = "Add a track with + first".to_owned();
            }
        }

        fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        fn toggle_settings_page(&mut self) {
            self.settings_open = !self.settings_open;
            self.status.clear();
        }

        fn background_color(&self) -> Color32 {
            BACKGROUND_THEMES.get(self.background_theme).map(|theme| theme.color).unwrap_or(BACKGROUND_THEMES[0].color)
        }

        fn accent_color(&self) -> Color32 {
            ACCENT_THEMES.get(self.accent_theme).map(|theme| theme.color).unwrap_or(ACCENT_THEMES[0].color)
        }

        fn elapsed(&self) -> Duration {
            match (self.playing, self.started_at) {
                (true, Some(started_at)) => self.paused_elapsed + started_at.elapsed(),
                _ => self.paused_elapsed,
            }
        }

        fn sync_state(&mut self) {
            if let Some(pending) = self.pending_play.as_ref() {
                if Instant::now() >= pending.due_at {
                    let index = pending.index;
                    self.pending_play = None;
                    if let Err(err) = self.play_queue_index(index, Duration::ZERO, true) {
                        self.status = format!("Could not continue queue: {err}");
                    }
                    return;
                }
            }
            if let Some(sink) = self.sink.as_ref() {
                sink.set_volume(self.volume);
            }
            if self.playing {
                if let Some(duration) = self.duration {
                    if duration > Duration::ZERO && self.elapsed() >= duration {
                        self.handle_track_finished();
                        return;
                    }
                }
                if self.sink.as_ref().is_some_and(Sink::empty) {
                    self.handle_track_finished();
                    return;
                }
            }
            self.sync_progress();
        }

        fn handle_track_finished(&mut self) {
            match self.repeat_mode {
                RepeatMode::One => {
                    if let Some(index) = self.current_index {
                        self.schedule_play(index);
                    }
                }
                RepeatMode::All => {
                    if self.shuffle && self.queue.len() > 1 {
                        if let Some(next) = self.advance_shuffle() {
                            self.schedule_play(next);
                            return;
                        }
                        // wrap around
                        if !self.queue.is_empty() {
                            self.schedule_play(0);
                        } else {
                            self.stop_current();
                        }
                    } else {
                        if let Some(next) = self.next_index() {
                            self.schedule_play(next);
                        } else {
                            // wrap around to first track
                            if !self.queue.is_empty() {
                                self.schedule_play(0);
                            } else {
                                self.stop_current();
                            }
                        }
                    }
                }
                RepeatMode::Off => {
                    if self.shuffle && self.queue.len() > 1 {
                        if let Some(next) = self.advance_shuffle() {
                            self.schedule_play(next);
                        } else {
                            if let Some(sink) = self.sink.take() { sink.stop(); }
                            self.playing = false;
                            self.started_at = None;
                            self.paused_elapsed = self.duration.unwrap_or(self.paused_elapsed);
                            self.progress = 1.0;
                        }
                    } else {
                        if let Some(next) = self.next_index() {
                            self.schedule_play(next);
                        } else {
                            if let Some(sink) = self.sink.take() { sink.stop(); }
                            self.playing = false;
                            self.started_at = None;
                            self.paused_elapsed = self.duration.unwrap_or(self.paused_elapsed);
                            self.progress = 1.0;
                        }
                    }
                }
            }
        }

        fn schedule_play(&mut self, index: usize) {
            if let Some(sink) = self.sink.take() { sink.stop(); }
            self.playing = false;
            self.started_at = None;
            self.paused_elapsed = Duration::ZERO;
            self.progress = 0.0;
            // Reduced from 300ms to 200ms for snappier track transitions
            self.pending_play = Some(PendingPlay { index, due_at: Instant::now() + Duration::from_millis(200) });
        }

        fn sync_progress(&mut self) {
            if let Some(duration) = self.duration {
                let duration_secs = duration.as_secs_f32();
                if duration_secs > 0.0 {
                    self.progress = (self.elapsed().as_secs_f32() / duration_secs).clamp(0.0, 1.0);
                }
            } else {
                self.progress = 0.0;
            }
        }

        fn draw_left_panel(&mut self, ui: &mut egui::Ui, rect: egui::Rect, accent: Color32, faded: Color32) {
            let inset = panel_inset(rect);
            let button_rect = egui::Rect::from_min_size(
                pos2(rect.left() + inset, rect.top() + inset),
                vec2(44.0, 44.0),
            );
            ui.allocate_ui_at_rect(button_rect, |ui| {
                if icon_button(ui, "☰", false, accent, "Settings").clicked() {
                    self.toggle_settings_page();
                }
            });

            let cover_size = (rect.width() * 0.62).min(rect.height() * 0.36).clamp(180.0, 320.0);
            let cover_rect = egui::Rect::from_center_size(
                pos2(rect.center().x, rect.top() + rect.height() * 0.31),
                vec2(cover_size, cover_size),
            );
            ui.allocate_ui_at_rect(cover_rect, |ui| self.draw_cover(ui, cover_size, accent));

            let title_rect = egui::Rect::from_min_size(
                pos2(cover_rect.left(), cover_rect.bottom() + rect.height() * 0.026),
                vec2(cover_size, 52.0),
            );
            ui.allocate_ui_at_rect(title_rect, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(&self.title).size(22.0).color(Color32::WHITE));
                    ui.label(RichText::new(&self.artist).size(14.0).color(faded));
                });
            });

            let seek_rect = egui::Rect::from_min_size(
                pos2(cover_rect.left(), title_rect.bottom() + rect.height() * 0.018),
                vec2(cover_size, 44.0),
            );
            ui.allocate_ui_at_rect(seek_rect, |ui| {
                ui.set_width(seek_rect.width());
                let before = self.progress;
                if line_slider(ui, &mut self.progress, cover_size, 28.0, accent,
                    Color32::from_rgba_unmultiplied(230, 230, 245, 65),
                ) && (self.progress - before).abs() > f32::EPSILON {
                    self.seek_to_progress(self.progress);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format_duration(self.elapsed())).size(12.0).color(faded));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(
                            self.duration.map(format_duration).unwrap_or_else(|| "--:--".to_owned()),
                        ).size(12.0).color(faded));
                    });
                });
            });

            let bottom_inset = inset + 46.0;
            let controls_rect = egui::Rect::from_min_size(
                pos2(cover_rect.left(), seek_rect.bottom() + rect.height() * 0.028),
                vec2(cover_size, 78.0),
            );
            let bottom_bar_top = rect.bottom() - bottom_inset;
            let controls_max_bottom = (bottom_bar_top - 8.0).max(controls_rect.top() + 48.0);
            let controls_rect = egui::Rect::from_min_max(
                controls_rect.left_top(),
                pos2(controls_rect.right(), controls_max_bottom),
            );
            ui.allocate_ui_at_rect(controls_rect, |ui| {
                let center = controls_rect.center();
                let prev_rect = egui::Rect::from_center_size(center + vec2(-74.0, 0.0), vec2(48.0, 48.0));
                let play_rect = egui::Rect::from_center_size(center, vec2(76.0, 76.0));
                let next_rect = egui::Rect::from_center_size(center + vec2(74.0, 0.0), vec2(48.0, 48.0));

                if control_icon_button(ui, prev_rect, ControlIcon::Previous, false, accent, "Previous").clicked() {
                    self.previous_track();
                }
                if control_icon_button(ui, play_rect,
                    if self.playing { ControlIcon::Pause } else { ControlIcon::Play },
                    false, accent, "Play / pause",
                ).clicked() {
                    self.toggle_playback();
                }
                if control_icon_button(ui, next_rect, ControlIcon::Next, false, accent, "Next").clicked() {
                    self.next_track();
                }
            });

            let bottom_rect = egui::Rect::from_min_size(
                pos2(cover_rect.left(), rect.bottom() - bottom_inset),
                vec2(cover_size, 44.0),
            );
            ui.allocate_ui_at_rect(bottom_rect, |ui| {
                let center = bottom_rect.center();
                let shuffle_rect = egui::Rect::from_center_size(center + vec2(-84.0, 0.0), vec2(44.0, 44.0));
                let repeat_rect = egui::Rect::from_center_size(center, vec2(44.0, 44.0));
                let full_rect = egui::Rect::from_center_size(center + vec2(84.0, 0.0), vec2(44.0, 44.0));

                if control_icon_button(ui, shuffle_rect, ControlIcon::Shuffle, self.shuffle, accent, "Shuffle").clicked() {
                    self.toggle_shuffle();
                }

                // Repeat button: black outline OFF, yellow outline ONE, blue outline ALL
                let (repeat_active, repeat_color, repeat_tooltip) = match self.repeat_mode {
                    RepeatMode::Off => (false, Color32::BLACK, "Repeat: Off"),
                    RepeatMode::One => (true, Color32::from_rgb(255, 255, 0), "Repeat: One"),
                    RepeatMode::All => (true, Color32::from_rgb(50, 130, 255), "Repeat: All"),
                };
                if repeat_icon_button(ui, repeat_rect, repeat_active, repeat_color, repeat_tooltip).clicked() {
                    self.toggle_repeat();
                }

                if control_icon_button(ui, full_rect, ControlIcon::Fullscreen, self.fullscreen, accent, "Fullscreen").clicked() {
                    self.toggle_fullscreen(ui.ctx());
                }
            });
        }

        fn draw_right_panel(&mut self, ui: &mut egui::Ui, rect: egui::Rect, accent: Color32, faded: Color32) {
            // ── Header ──────────────────────────────────────────────────
            let header_rect = egui::Rect::from_min_size(
                pos2(rect.left() + 42.0, rect.top() + 48.0),
                vec2(rect.width() - 84.0, 52.0),
            );
            ui.allocate_ui_at_rect(header_rect, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Queued Tracks").size(28.0).color(Color32::WHITE).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if header_icon_button(ui, HeaderIcon::Add, "Add audio files").clicked() {
                            self.open_file_dialog();
                        }
                        if header_icon_button(ui, HeaderIcon::Copy, "Copy current track path").clicked() {
                            self.share_path(ui.ctx());
                        }
                        if header_icon_button(ui, HeaderIcon::Search, "Search").clicked() {
                            self.status = "Search will arrive with library support".to_owned();
                        }
                    });
                });
            });

            // ── Volume (bottom) ─────────────────────────────────────────
            let vol_h = 38.0;
            let vol_y = rect.bottom() - 42.0 - vol_h;
            ui.allocate_ui_at_rect(
                egui::Rect::from_min_size(pos2(rect.left() + 42.0, vol_y), vec2(rect.width() - 84.0, vol_h)),
                |ui| {
                    ui.horizontal(|ui| {
                        // Mute button
                        let (mute_rect, mute_resp) = ui.allocate_exact_size(vec2(24.0, vol_h), Sense::click());
                        if mute_resp.clicked() {
                            if self.muted {
                                self.volume = self.saved_volume;
                                self.muted = false;
                            } else {
                                self.saved_volume = self.volume;
                                self.volume = 0.0;
                                self.muted = true;
                            }
                            if let Some(ref sink) = self.sink { sink.set_volume(self.volume); }
                        }
                        let painter = ui.painter_at(mute_rect);
                        draw_speaker_icon(&painter, mute_rect, accent);
                        // Slider
                        let slider_w = (rect.width() - 84.0 - 72.0).max(120.0);
                        line_slider(ui, &mut self.volume, slider_w, vol_h, accent,
                            Color32::from_rgba_unmultiplied(230, 230, 245, 65));
                        // Max button
                        let (max_rect, max_resp) = ui.allocate_exact_size(vec2(24.0, vol_h), Sense::click());
                        if max_resp.clicked() {
                            if self.muted { self.volume = self.saved_volume; }
                            else { self.saved_volume = self.volume; self.volume = 1.0; }
                            self.muted = false;
                            if let Some(ref sink) = self.sink { sink.set_volume(self.volume); }
                        }
                        let painter = ui.painter_at(max_rect);
                        draw_speaker_icon(&painter, max_rect, faded);
                    });
                },
            );

            // ── Track list (fills remaining space) ──────────────────────
            let list_rect = egui::Rect::from_min_max(
                pos2(rect.left() + 42.0, header_rect.bottom() + 12.0),
                pos2(rect.right() - 42.0, vol_y - 12.0),
            );
            ui.allocate_ui_at_rect(list_rect, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(list_rect.width());
                    if self.queue.is_empty() {
                        ui.add_space(24.0);
                        ui.label(RichText::new("Use + to add local audio files").size(16.0).color(faded));
                        return;
                    }
                    let mut clicked_index = None;
                    for index in 0..self.queue.len() {
                        let entry = &self.queue[index];
                        let active = self.current_index == Some(index);
                        let (row_rect, response) = ui.allocate_exact_size(vec2(list_rect.width(), 68.0), Sense::click());
                        let painter = ui.painter_at(row_rect);
                        if active {
                            painter.rect_filled(row_rect.shrink2(vec2(0.0, 4.0)), 8.0,
                                Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 20));
                        }
                        let thumb = egui::Rect::from_min_size(
                            pos2(row_rect.left(), row_rect.top() + 8.0), vec2(52.0, 52.0));
                        painter.rect_filled(thumb, 8.0, Color32::from_rgb(25, 27, 34));
                        painter.rect_stroke(thumb, 8.0, Stroke::new(
                            if active { 2.0 } else { 1.0 },
                            if active { accent } else { Color32::from_rgba_unmultiplied(230, 230, 245, 42) },
                        ));
                        painter.text(thumb.center(), egui::Align2::CENTER_CENTER,
                            entry.title.chars().next().unwrap_or('S').to_string(),
                            egui::FontId::proportional(18.0), if active { accent } else { faded });
                        painter.text(pos2(row_rect.left() + 72.0, row_rect.top() + 17.0),
                            egui::Align2::LEFT_CENTER, &entry.title,
                            egui::FontId::proportional(19.0), Color32::WHITE);
                        painter.text(pos2(row_rect.left() + 72.0, row_rect.top() + 42.0),
                            egui::Align2::LEFT_CENTER, &entry.artist,
                            egui::FontId::proportional(13.0), faded);
                        // Right-align time with 8px padding from right edge
                        painter.text(pos2(row_rect.right() - 8.0, row_rect.top() + 31.0),
                            egui::Align2::RIGHT_CENTER,
                            entry.duration.map(format_duration).unwrap_or_else(|| "--:--".to_owned()),
                            egui::FontId::proportional(14.0), faded);
                        if response.clicked() { clicked_index = Some(index); }
                    }
                    if let Some(index) = clicked_index {
                        if let Err(err) = self.play_queue_index(index, Duration::ZERO, true) {
                            self.status = format!("Could not play queued track: {err}");
                        }
                    }
                });
            });
        }

        fn draw_settings_page(&mut self, ui: &mut egui::Ui, rect: egui::Rect, border: Color32, faded: Color32, accent: Color32, _bg: Color32) {
            let inset = panel_inset(rect);
            let menu_rect = egui::Rect::from_min_size(
                pos2(rect.left() + inset, rect.top() + inset),
                vec2(44.0, 44.0),
            );
            ui.allocate_ui_at_rect(menu_rect, |ui| {
                if icon_button(ui, "☰", true, accent, "Back to player").clicked() {
                    self.toggle_settings_page();
                }
            });

            // Use the full shell rect for settings — same size as the main player
            // Push content below the hamburger button so they don't overlap
            let content_top = (menu_rect.bottom() + inset).max(rect.top() + inset + 44.0 + 12.0);
            let content = egui::Rect::from_min_max(
                pos2(rect.left() + inset + 8.0, content_top),
                pos2(rect.right() - inset - 8.0, rect.bottom() - inset - 8.0),
            );
            let tabs_rect = egui::Rect::from_min_max(
                content.left_top(),
                pos2(content.left() + 176.0, content.bottom()),
            );
            let detail_rect = egui::Rect::from_min_max(
                pos2(tabs_rect.right() + 26.0, content.top()),
                content.right_bottom(),
            );

            ui.allocate_ui_at_rect(tabs_rect, |ui| {
                ui.label(RichText::new("Settings").size(28.0).color(Color32::WHITE).strong());
                ui.add_space(20.0);
                for tab in SETTINGS_TABS {
                    if settings_tab_button(ui, tab, self.settings_tab == tab, border, accent).clicked() {
                        self.settings_tab = tab;
                    }
                }
            });

            let painter = ui.painter_at(rect);
            painter.line_segment(
                [pos2(tabs_rect.right() + 12.0, content.top()), pos2(tabs_rect.right() + 12.0, content.bottom())],
                Stroke::new(1.0, border),
            );

            // Scrollable detail area — constrain height so ScrollArea scrolls
            ui.allocate_ui_at_rect(detail_rect, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(detail_rect.height())
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                    ui.set_width(detail_rect.width() - 12.0);
                    match self.settings_tab {
                        SettingsTab::Appearance => {
                            ui.label(RichText::new("Appearance").size(24.0).color(Color32::WHITE).strong());
                            ui.add_space(14.0);
                            ui.label(RichText::new("Background color").size(14.0).color(faded));
                            ui.add_space(8.0);
                            draw_theme_choices(ui, &mut self.background_theme, &BACKGROUND_THEMES, border, accent);
                            ui.add_space(18.0);
                            ui.label(RichText::new("Accent color").size(14.0).color(faded));
                            ui.add_space(8.0);
                            draw_theme_choices(ui, &mut self.accent_theme, &ACCENT_THEMES, border, accent);
                            ui.add_space(18.0);
                            ui.label(RichText::new("Font").size(14.0).color(faded));
                            ui.add_space(8.0);
                            let font_options: Vec<_> = self.available_fonts.clone();
                            let current_font = self.selected_font.clone();
                            egui::ComboBox::from_id_source("font_selector")
                                .selected_text(&current_font).width(200.0).show_ui(ui, |ui| {
                                    for font in &font_options {
                                        let selected = *font == current_font;
                                        if ui.selectable_label(selected, font).clicked() {
                                            self.selected_font = font.clone();
                                            self.apply_font(ui.ctx());
                                        }
                                    }
                                });
                            ui.add_space(18.0);
                            ui.label(RichText::new("Bar Animation Style").size(14.0).color(faded));
                            ui.add_space(8.0);
                            let anim_styles = ["Wave", "Pulse", "Breathe", "Random"];
                            let current_anim = anim_styles.get(self.bar_animation).copied().unwrap_or("Wave");
                            egui::ComboBox::from_id_source("bar_animation")
                                .selected_text(current_anim).width(200.0).show_ui(ui, |ui| {
                                    for (idx, style) in anim_styles.iter().enumerate() {
                                        if ui.selectable_label(self.bar_animation == idx, *style).clicked() {
                                            self.bar_animation = idx;
                                        }
                                    }
                                });
                            ui.add_space(18.0);
                            ui.label(RichText::new("Options").size(14.0).color(faded));
                            ui.add_space(8.0);
                            let half = (APPEARANCE_OPTIONS.len() + 1) / 2;
                            ui.columns(2, |columns| {
                                for (col_idx, col) in columns.iter_mut().enumerate() {
                                    col.vertical(|col| {
                                        let start = col_idx * half;
                                        let end = ((col_idx + 1) * half).min(APPEARANCE_OPTIONS.len());
                                        for index in start..end {
                                            col.checkbox(&mut self.appearance_options[index], APPEARANCE_OPTIONS[index]);
                                        }
                                    });
                                }
                            });
                        }
                        SettingsTab::About => {
                            ui.label(RichText::new("About").size(24.0).color(Color32::WHITE).strong());
                            ui.add_space(14.0);
                            let app_version = option_env!("CARGO_PKG_VERSION").unwrap_or("0.8.6");
                            ui.label(RichText::new(format!("Skythe v{app_version}")).size(20.0).color(accent));
                            ui.add_space(8.0);
                            ui.label(RichText::new("A modern music player built with Rust.").size(14.0).color(faded));
                            ui.add_space(4.0);
                            ui.label(RichText::new(format!("Version: {app_version}")).size(14.0).color(faded));
                            ui.add_space(4.0);
                            ui.label(RichText::new("License: MIT OR Apache-2.0").size(14.0).color(faded));
                            ui.add_space(20.0);
                            ui.label(RichText::new("This version is automatically read from Cargo.toml").size(12.0).color(
                                Color32::from_rgba_unmultiplied(200, 200, 200, 150)));
                        }
                        tab => {
                            ui.label(RichText::new(tab.label()).size(24.0).color(Color32::WHITE).strong());
                            ui.add_space(14.0);
                            ui.label(RichText::new("Options coming soon").size(15.0).color(faded));
                        }
                    }
                });
            });
        }

        fn draw_cover(&self, ui: &mut egui::Ui, size: f32, accent: Color32) {
            let (rect, _response) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
            let painter = ui.painter_at(rect);
            let time = ui.input(|input| input.time) as f32;
            painter.rect_filled(rect, 8.0, Color32::from_rgb(17, 18, 23));
            painter.circle_stroke(rect.center(), size * 0.19, Stroke::new(size * 0.014, accent));
            painter.circle_stroke(rect.center() + vec2(size * 0.022, -size * 0.018), size * 0.19,
                Stroke::new(size * 0.008, Color32::from_rgba_unmultiplied(255, 255, 255, 185)));

            let num_bars = 26;
            let gap = size * 0.01;
            let bar_w = ((size * 0.58) - gap * (num_bars as f32 - 1.0)) / num_bars as f32;
            let base_y = rect.bottom() - size * 0.11;
            let left = rect.center().x - (bar_w * num_bars as f32 + gap * (num_bars as f32 - 1.0)) / 2.0;
            let motion = if self.playing { 1.0 } else { 0.2 };

            // Get real EQ band energies from captured audio samples
            let eq_bands = if self.appearance_options[0] {
                self.audio_buffer.lock().map(|b| b.compute_bands(num_bars)).unwrap_or_else(|_| vec![0.0; num_bars])
            } else {
                vec![0.0; num_bars]
            };

            for i in 0..num_bars {
                let height = if self.appearance_options[0] && !eq_bands.is_empty() && eq_bands[i % eq_bands.len()] > 0.0 {
                    // Real EQ mode: use actual audio energy data
                    eq_bands[i % eq_bands.len()] * size * 0.25
                } else if motion > 0.0 {
                    // Fallback animation modes when EQ is disabled or no audio
                    let height_val = match self.bar_animation {
                        0 => { // Wave animation (default)
                            let phase = time * (2.0 + i as f32 * 0.04) + i as f32 * 0.5;
                            (phase.sin() * 0.5 + 0.5) * size * 0.16 * motion
                        }
                        1 => { // Pulse
                            let pulse = (time * 3.0 + i as f32 * 0.3).sin() * 0.5 + 0.5;
                            let wave = (time * (1.5 + i as f32 * 0.15)).sin() * 0.5 + 0.5;
                            (pulse * 0.5 + wave * 0.5) * size * 0.18 * motion
                        }
                        2 => { // Breathe
                            let wave = (time * 1.5).sin() * 0.5 + 0.5;
                            let offset = (i as f32 / num_bars as f32 * std::f32::consts::PI).sin() * 0.3;
                            (wave + offset).clamp(0.0, 1.0) * size * 0.18 * motion
                        }
                        3 => { // Random
                            let seed = (time * 8.0 + i as f32 * 7.3) as u32;
                            let r = (seed.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFFFFFF) as f32 / 2147483648.0;
                            r * size * 0.2 * motion
                        }
                        _ => {
                            // Fallback: simple wave
                            let phase = time * (2.0 + i as f32 * 0.04) + i as f32 * 0.5;
                            (phase.sin() * 0.5 + 0.5) * size * 0.16 * motion
                        }
                    };
                    height_val
                } else {
                    size * 0.05
                };

                let height = height.max(size * 0.02);
                let x = left + i as f32 * (bar_w + gap);
                painter.rect_filled(
                    egui::Rect::from_min_max(pos2(x, base_y - height), pos2(x + bar_w, base_y)),
                    bar_w * 0.5, Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 160));
            }
            painter.rect_stroke(rect, 8.0, Stroke::new(1.0, Color32::from_rgba_unmultiplied(240, 240, 255, 62)));
        }
    }

    impl eframe::App for PlayerApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.check_ipc_messages();
            if !self.startup_files.is_empty() {
                self.load_startup_files();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(MIN_WINDOW));
            self.sync_state();

            let bg = self.background_color();
            let card = bg;
            let border = Color32::from_rgba_unmultiplied(238, 238, 255, 54);
            let accent = self.accent_color();
            let faded = Color32::from_rgba_unmultiplied(222, 222, 236, 175);

            ctx.set_visuals(egui::Visuals::dark());
            egui::CentralPanel::default().frame(egui::Frame::none().fill(bg)).show(ctx, |ui| {
                let available = ui.available_rect_before_wrap();
                let painter = ui.painter_at(available);
                painter.rect_filled(available, 0.0, bg);

                let shell = available.shrink2(vec2(
                    (available.width() * 0.055).clamp(34.0, 70.0),
                    (available.height() * 0.08).clamp(34.0, 66.0),
                ));
                painter.rect_filled(shell, 20.0, card);
                painter.rect_stroke(shell, 20.0, Stroke::new(1.0, border));

                if self.settings_open {
                    ui.allocate_ui_at_rect(shell, |ui| {
                        self.draw_settings_page(ui, shell, border, faded, accent, card)
                    });
                    return;
                }

                let split_x = shell.left() + shell.width() * 0.43;
                painter.line_segment(
                    [pos2(split_x, shell.top()), pos2(split_x, shell.bottom())],
                    Stroke::new(1.0, border),
                );

                let left = egui::Rect::from_min_max(shell.left_top(), pos2(split_x, shell.bottom()));
                let right = egui::Rect::from_min_max(pos2(split_x, shell.top()), shell.right_bottom());
                ui.allocate_ui_at_rect(left, |ui| self.draw_left_panel(ui, left, accent, faded));
                ui.allocate_ui_at_rect(right, |ui| self.draw_right_panel(ui, right, accent, faded));

                if !self.status.is_empty() {
                    let status_rect = egui::Rect::from_min_size(
                        pos2(shell.left() + 28.0, shell.bottom() - 30.0),
                        vec2(shell.width() - 56.0, 22.0),
                    );
                    ui.allocate_ui_at_rect(status_rect, |ui| {
                        ui.label(RichText::new(&self.status).size(12.0).color(Color32::from_rgb(255, 170, 170)));
                    });
                }
            });
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    trait DurationClamp {
        fn min_duration(self, max: Option<Duration>) -> Duration;
    }
    impl DurationClamp for Duration {
        fn min_duration(self, max: Option<Duration>) -> Duration {
            max.map(|duration| self.min(duration)).unwrap_or(self)
        }
    }

    struct BackgroundTheme { name: &'static str, color: Color32 }

    const SETTINGS_TABS: [SettingsTab; 10] = [
        SettingsTab::Appearance, SettingsTab::Playback, SettingsTab::Queue,
        SettingsTab::Audio, SettingsTab::Library, SettingsTab::Shortcuts,
        SettingsTab::Integrations, SettingsTab::Privacy, SettingsTab::Advanced, SettingsTab::About,
    ];

    const APPEARANCE_OPTIONS: [&str; 10] = [
        "Animated EQ bars", "Rounded app shell", "Show album frame",
        "Show queue durations", "Highlight active queue row", "Compact queue rows",
        "Large playback button", "Show volume bar", "Use dense spacing", "Dim inactive buttons",
    ];

    const BACKGROUND_THEMES: [BackgroundTheme; 12] = [
        BackgroundTheme { name: "#1F1F1F", color: Color32::from_rgb(31, 31, 31) },
        BackgroundTheme { name: "Near black", color: Color32::from_rgb(17, 17, 24) },
        BackgroundTheme { name: "Graphite", color: Color32::from_rgb(25, 27, 34) },
        BackgroundTheme { name: "Deep slate", color: Color32::from_rgb(22, 26, 31) },
        BackgroundTheme { name: "Charcoal", color: Color32::from_rgb(36, 33, 36) },
        BackgroundTheme { name: "Midnight", color: Color32::from_rgb(13, 15, 30) },
        BackgroundTheme { name: "Ebony", color: Color32::from_rgb(26, 22, 20) },
        BackgroundTheme { name: "Obsidian", color: Color32::from_rgb(18, 20, 26) },
        BackgroundTheme { name: "Steel", color: Color32::from_rgb(28, 30, 35) },
        BackgroundTheme { name: "Dark mauve", color: Color32::from_rgb(30, 25, 35) },
        BackgroundTheme { name: "Pine", color: Color32::from_rgb(21, 30, 27) },
        BackgroundTheme { name: "Cocoa", color: Color32::from_rgb(30, 24, 20) },
    ];

    const ACCENT_THEMES: [BackgroundTheme; 12] = [
        BackgroundTheme { name: "Cyan", color: Color32::from_rgb(79, 237, 225) },
        BackgroundTheme { name: "Ice", color: Color32::from_rgb(142, 210, 255) },
        BackgroundTheme { name: "Mint", color: Color32::from_rgb(128, 232, 173) },
        BackgroundTheme { name: "Rose", color: Color32::from_rgb(255, 128, 168) },
        BackgroundTheme { name: "Lavender", color: Color32::from_rgb(190, 150, 255) },
        BackgroundTheme { name: "Gold", color: Color32::from_rgb(255, 203, 82) },
        BackgroundTheme { name: "Coral", color: Color32::from_rgb(255, 120, 100) },
        BackgroundTheme { name: "Sky blue", color: Color32::from_rgb(100, 180, 255) },
        BackgroundTheme { name: "Lime", color: Color32::from_rgb(160, 230, 100) },
        BackgroundTheme { name: "Peach", color: Color32::from_rgb(255, 180, 130) },
        BackgroundTheme { name: "Teal", color: Color32::from_rgb(80, 220, 200) },
        BackgroundTheme { name: "Magenta", color: Color32::from_rgb(230, 100, 220) },
    ];

    fn track_from_path(path: PathBuf) -> TrackEntry {
        // Probe the file for duration so it shows immediately in the queue
        let duration = estimate_duration(&path);
        TrackEntry { title: display_title(&path), artist: parent_name(&path), path, duration }
    }

    /// Estimate audio duration from file size — fast and non-blocking.
    fn estimate_duration(path: &Path) -> Option<Duration> {
        let file = File::open(path).ok()?;
        estimate_duration_from_size(file, path)
    }

    /// Estimate audio duration from file size based on typical bitrates per format
    fn estimate_duration_from_size(file: File, path: &Path) -> Option<Duration> {
        let len = file.metadata().ok()?.len();
        if len == 0 { return None; }
        let ext = path.extension().and_then(|e| e.to_str())?.to_ascii_lowercase();
        // Typical bitrates in bytes per second (approximate)
        let bytes_per_sec: u64 = match ext.as_str() {
            "mp3" => 192_000 / 8,       // 192 kbps MP3
            "ogg" => 192_000 / 8,        // ~192 kbps Ogg Vorbis
            "m4a" | "aac" => 256_000 / 8, // 256 kbps AAC
            "flac" => 800_000 / 8,       // ~800 kbps FLAC (lossless)
            "wav" => {
                // Try to read WAV header for accurate duration
                use std::io::Read;
                let mut reader = std::io::BufReader::new(file);
                let mut header = [0u8; 44];
                if reader.read_exact(&mut header).is_ok() {
                    let channels = u16::from_le_bytes([header[22], header[23]]) as u64;
                    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]) as u64;
                    let bytes_per_sample = (u16::from_le_bytes([header[34], header[35]]) / 8) as u64;
                    if channels > 0 && sample_rate > 0 && bytes_per_sample > 0 {
                        let data_size = len.saturating_sub(44);
                        let total_samples = data_size / (channels * bytes_per_sample);
                        let secs = total_samples / sample_rate;
                        return Some(Duration::from_secs(secs));
                    }
                }
                1_411_200 / 8 // 1411 kbps CD-quality WAV (fallback)
            }
            _ => return None,
        };
        if bytes_per_sec == 0 { return None; }
        let secs = len / bytes_per_sec;
        if secs < 3600 * 12 { // Sanity: less than 12 hours
            Some(Duration::from_secs(secs))
        } else {
            None
        }
    }

    fn is_audio_file(path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()).map(|e| {
            matches!(e.to_ascii_lowercase().as_str(), "flac" | "mp3" | "ogg" | "wav" | "m4a" | "aac")
        }).unwrap_or(false)
    }

    fn panel_inset(rect: egui::Rect) -> f32 {
        (rect.width().min(rect.height()) * 0.055).clamp(24.0, 38.0)
    }

    enum HeaderIcon { Search, Copy, Add }
    enum ControlIcon { Previous, Next, Play, Pause, Shuffle, Fullscreen }

    fn settings_tab_button(ui: &mut egui::Ui, tab: SettingsTab, active: bool, border: Color32, accent: Color32) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(vec2(160.0, 34.0), Sense::click());
        let painter = ui.painter_at(rect);
        if active {
            painter.rect_filled(rect, 7.0, Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 26));
            painter.rect_stroke(rect, 7.0, Stroke::new(1.0, border));
        }
        painter.text(pos2(rect.left() + 12.0, rect.center().y), egui::Align2::LEFT_CENTER, tab.label(),
            egui::FontId::proportional(14.0),
            if active { accent } else { Color32::from_rgba_unmultiplied(238, 238, 250, 205) });
        response
    }

    fn draw_theme_choices(ui: &mut egui::Ui, selected: &mut usize, themes: &[BackgroundTheme], border: Color32, accent: Color32) {
        ui.horizontal_wrapped(|ui| {
            for (index, theme) in themes.iter().enumerate() {
                let (swatch, response) = ui.allocate_exact_size(vec2(118.0, 38.0), Sense::click());
                let painter = ui.painter_at(swatch);
                painter.rect_filled(swatch, 8.0, Color32::from_rgb(28, 29, 36));
                painter.rect_stroke(swatch, 8.0, Stroke::new(
                    if *selected == index { 2.0 } else { 1.0 },
                    if *selected == index { accent } else { border },
                ));
                let color_rect = egui::Rect::from_min_size(pos2(swatch.left() + 10.0, swatch.center().y - 8.0), vec2(16.0, 16.0));
                painter.rect_filled(color_rect, 4.0, theme.color);
                painter.text(pos2(swatch.left() + 34.0, swatch.center().y), egui::Align2::LEFT_CENTER,
                    theme.name, egui::FontId::proportional(13.0),
                    Color32::from_rgba_unmultiplied(238, 238, 250, 220));
                if response.clicked() { *selected = index; }
            }
        });
    }

    fn repeat_icon_button(ui: &mut egui::Ui, rect: egui::Rect, active: bool, outline_color: Color32, tooltip: &str) -> egui::Response {
        let response = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);
        let c = rect.center();

        if active {
            // Active outline
            painter.circle_stroke(c, rect.width().min(rect.height()) * 0.42, Stroke::new(2.0, outline_color));
        }

        // Simple repeat loop icon
        painter.line_segment([c + vec2(-10.0, -6.0), c + vec2(8.0, -6.0)], Stroke::new(2.2, if active { outline_color } else { Color32::from_rgba_unmultiplied(238, 238, 250, 220) }));
        painter.add(egui::Shape::line(
            vec![c + vec2(10.0, -4.0), c + vec2(10.0, 4.0), c + vec2(-8.0, 4.0)],
            Stroke::new(2.2, if active { outline_color } else { Color32::from_rgba_unmultiplied(238, 238, 250, 220) }),
        ));
        draw_arrow_head(&painter, c + vec2(8.0, -6.0), if active { outline_color } else { Color32::from_rgba_unmultiplied(238, 238, 250, 220) }, true);
        draw_arrow_head(&painter, c + vec2(-8.0, 4.0), if active { outline_color } else { Color32::from_rgba_unmultiplied(238, 238, 250, 220) }, false);

        response.on_hover_text(tooltip)
    }

    fn control_icon_button(ui: &mut egui::Ui, rect: egui::Rect, icon: ControlIcon, active: bool, accent: Color32, tooltip: &str) -> egui::Response {
        let response = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);
        let color = if active || response.hovered() { accent } else { Color32::from_rgba_unmultiplied(238, 238, 250, 220) };
        let c = rect.center();

        if matches!(icon, ControlIcon::Play | ControlIcon::Pause) {
            painter.circle_stroke(c, rect.width().min(rect.height()) * 0.47, Stroke::new(1.6, accent));
        }

        match icon {
            ControlIcon::Previous => {
                painter.line_segment([c + vec2(9.0, -10.0), c + vec2(9.0, 10.0)], Stroke::new(2.5, color));
                painter.add(egui::Shape::convex_polygon(
                    vec![c + vec2(6.0, -11.0), c + vec2(-8.0, 0.0), c + vec2(6.0, 11.0)], color, Stroke::NONE));
                painter.add(egui::Shape::convex_polygon(
                    vec![c + vec2(-2.0, -11.0), c + vec2(-16.0, 0.0), c + vec2(-2.0, 11.0)], color, Stroke::NONE));
            }
            ControlIcon::Next => {
                painter.line_segment([c + vec2(-9.0, -10.0), c + vec2(-9.0, 10.0)], Stroke::new(2.5, color));
                painter.add(egui::Shape::convex_polygon(
                    vec![c + vec2(-6.0, -11.0), c + vec2(8.0, 0.0), c + vec2(-6.0, 11.0)], color, Stroke::NONE));
                painter.add(egui::Shape::convex_polygon(
                    vec![c + vec2(2.0, -11.0), c + vec2(16.0, 0.0), c + vec2(2.0, 11.0)], color, Stroke::NONE));
            }
            ControlIcon::Play => {
                painter.add(egui::Shape::convex_polygon(
                    vec![c + vec2(-8.0, -15.0), c + vec2(15.0, 0.0), c + vec2(-8.0, 15.0)], color, Stroke::NONE));
            }
            ControlIcon::Pause => {
                painter.rect_filled(egui::Rect::from_center_size(c + vec2(-6.0, 0.0), vec2(5.0, 26.0)), 2.0, color);
                painter.rect_filled(egui::Rect::from_center_size(c + vec2(6.0, 0.0), vec2(5.0, 26.0)), 2.0, color);
            }
            ControlIcon::Shuffle => {
                painter.add(egui::Shape::line(vec![
                    c + vec2(-15.0, -8.0), c + vec2(-5.0, -8.0), c + vec2(6.0, 8.0), c + vec2(15.0, 8.0),
                ], Stroke::new(2.0, color)));
                painter.add(egui::Shape::line(vec![
                    c + vec2(-15.0, 8.0), c + vec2(-5.0, 8.0), c + vec2(6.0, -8.0), c + vec2(15.0, -8.0),
                ], Stroke::new(2.0, color)));
                draw_arrow_head(&painter, c + vec2(15.0, 8.0), color, true);
                draw_arrow_head(&painter, c + vec2(15.0, -8.0), color, true);
            }
            // Repeat is handled by dedicated repeat_icon_button
            ControlIcon::Fullscreen => {
                let d = 12.0;
                for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                    let corner = c + vec2(sx * d, sy * d);
                    painter.line_segment([corner, corner + vec2(-sx * 7.0, 0.0)], Stroke::new(2.0, color));
                    painter.line_segment([corner, corner + vec2(0.0, -sy * 7.0)], Stroke::new(2.0, color));
                }
            }
        }
        response.on_hover_text(tooltip)
    }

    fn draw_arrow_head(painter: &egui::Painter, tip: egui::Pos2, color: Color32, right: bool) {
        let dir = if right { -1.0 } else { 1.0 };
        painter.line_segment([tip, tip + vec2(dir * 6.0, -4.0)], Stroke::new(2.0, color));
        painter.line_segment([tip, tip + vec2(dir * 6.0, 4.0)], Stroke::new(2.0, color));
    }

    fn header_icon_button(ui: &mut egui::Ui, icon: HeaderIcon, tooltip: &str) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
        let painter = ui.painter_at(rect);
        let color = if response.hovered() { Color32::from_rgb(79, 237, 225) } else { Color32::from_rgba_unmultiplied(238, 238, 250, 205) };
        let c = rect.center();
        match icon {
            HeaderIcon::Search => {
                painter.circle_stroke(c + vec2(-3.0, -3.0), 7.0, Stroke::new(2.0, color));
                painter.line_segment([c + vec2(3.0, 3.0), c + vec2(10.0, 10.0)], Stroke::new(2.0, color));
            }
            HeaderIcon::Copy => {
                let back = egui::Rect::from_center_size(c + vec2(-3.0, 3.0), vec2(13.0, 13.0));
                let front = egui::Rect::from_center_size(c + vec2(3.0, -3.0), vec2(13.0, 13.0));
                painter.rect_stroke(back, 1.0, Stroke::new(1.6, color));
                painter.rect_stroke(front, 1.0, Stroke::new(1.6, color));
            }
            HeaderIcon::Add => {
                painter.line_segment([c + vec2(-8.0, 0.0), c + vec2(8.0, 0.0)], Stroke::new(2.0, color));
                painter.line_segment([c + vec2(0.0, -8.0), c + vec2(0.0, 8.0)], Stroke::new(2.0, color));
            }
        }
        response.on_hover_text(tooltip)
    }

    fn draw_speaker_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
        let mid = rect.center().y;
        let left = rect.left() + 4.0;
        let body = [
            pos2(left, mid - 4.0), pos2(left + 5.0, mid - 4.0), pos2(left + 10.0, mid - 9.0),
            pos2(left + 10.0, mid + 9.0), pos2(left + 5.0, mid + 4.0), pos2(left, mid + 4.0)];
        painter.add(egui::Shape::closed_line(body.to_vec(), Stroke::new(1.8, color)));
        painter.line_segment([pos2(left + 13.0, mid - 6.0), pos2(left + 17.0, mid - 2.0)], Stroke::new(1.5, color));
        painter.line_segment([pos2(left + 17.0, mid - 2.0), pos2(left + 17.0, mid + 2.0)], Stroke::new(1.5, color));
        painter.line_segment([pos2(left + 17.0, mid + 2.0), pos2(left + 13.0, mid + 6.0)], Stroke::new(1.5, color));
    }

    fn icon_button(ui: &mut egui::Ui, label: &str, active: bool, accent: Color32, tooltip: &str) -> egui::Response {
        let default_color = Color32::from_rgba_unmultiplied(238, 238, 250, 205);
        let text = if active {
            RichText::new(label).size(25.0).color(accent)
        } else {
            RichText::new(label).size(25.0).color(default_color)
        };
        ui.add_sized(vec2(44.0, 44.0), egui::Button::new(text).fill(Color32::TRANSPARENT).stroke(Stroke::NONE))
            .on_hover_text(tooltip)
    }

    fn line_slider(ui: &mut egui::Ui, value: &mut f32, width: f32, height: f32, accent: Color32, muted: Color32) -> bool {
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
        let mut changed = false;
        let track = rect.shrink2(vec2(10.0, 0.0));
        if let Some(pos) = response.interact_pointer_pos() {
            if response.clicked() || response.dragged() {
                *value = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
                changed = true;
            }
        }
        let painter = ui.painter_at(rect);
        let y = track.center().y;
        let start = pos2(track.left(), y);
        let end = pos2(track.right(), y);
        let filled_x = egui::lerp(track.left()..=track.right(), (*value).clamp(0.0, 1.0));
        painter.line_segment([start, end], Stroke::new(4.0, muted));
        painter.line_segment([start, pos2(filled_x, y)], Stroke::new(4.0, accent));
        painter.circle_filled(pos2(filled_x, y), 9.0, Color32::from_rgb(24, 25, 33));
        painter.circle_stroke(pos2(filled_x, y), 10.0, Stroke::new(2.0, accent));
        changed
    }

    fn open_source(path: &Path, offset: Duration) -> anyhow::Result<(Box<dyn Source<Item = i16> + Send>, Option<Duration>)> {
        let file = File::open(path)?;
        let source = Decoder::new(BufReader::new(file))?;
        let duration = source.total_duration()
            .or_else(|| estimate_duration(path));
        Ok((Box::new(source.skip_duration(offset)), duration))
    }

    fn display_title(path: &Path) -> String {
        path.file_stem().and_then(|name| name.to_str()).filter(|name| !name.is_empty()).unwrap_or("Untitled").to_owned()
    }

    fn parent_name(path: &Path) -> String {
        path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or("Local file").to_owned()
    }

    fn format_duration(duration: Duration) -> String {
        let total_secs = duration.as_secs();
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let secs = total_secs % 60;
        if hours > 0 {
            format!("{}:{:02}:{:02}", hours, mins, secs)
        } else {
            format!("{}:{:02}", mins, secs)
        }
    }
}