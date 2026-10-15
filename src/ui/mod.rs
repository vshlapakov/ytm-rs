//! The application UI: state, playback logic and the per-frame layout.
//!
//! Layout: animated album-art background (shader.rs) → sidebar, top bar and page
//! content → player bar → now-playing overlay (slides up) → toasts.

mod jam;
mod notes;
mod nowplaying;
mod pages;
mod shell;
mod transfer;
mod karaoke;
mod lyric_prefs;
mod widgets;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Receiver;
use egui::{Color32, Ui};
use souvlaki::MediaControlEvent;

use crate::audio::{AudioCmd, AudioEvent, AudioHandle};
use crate::backend::{self, ArtistPage, Backend, Collection, Explore, Library, Req, Resp, SearchResults};
use crate::lyrics::{Lyrics, LyricsSet};
use crate::media::Media;
use crate::model::{Card, Chip, Shelf, ShelfKind, Target, Track};
use crate::shader::Background;
use crate::theme;

/// File types YouTube Music accepts for upload.
const UPLOAD_EXTS: &[&str] = &["mp3", "m4a", "flac", "ogg", "wma"];

#[derive(Clone, PartialEq, Hash, Debug)]
pub enum Page {
    Home,
    Explore,
    Library(LibTab),
    Search(String),
    Album(String),
    Playlist(String),
    Artist(String),
    Settings,
    /// Transfer from Spotify.
    Transfer,
    /// Listen together.
    Jam,
}

#[derive(Clone, Copy, PartialEq, Hash, Debug)]
pub enum LibTab {
    Playlists,
    Songs,
    Albums,
    History,
    Uploads,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Repeat {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, PartialEq)]
pub enum NpTab {
    Lyrics,
    UpNext,
    /// Neither panel: just the artwork.
    Art,
}

/// User intents collected while drawing; applied after the frame's UI pass.
pub enum Action {
    Play(Vec<Track>, usize),
    PlayCollection(Target, bool),
    /// Click on a card: songs start a radio, everything else opens its page.
    Open(Target),
    PlayNext(Track),
    Enqueue(Track),
    Radio(Track),
    Go(Page),
    QueueJump(usize),
    QueueRemove(usize),
    Seek(f32),
    Like(String, bool),
    Dislike(String),
    Share(Target),
    AddToPlaylist(Track),
    NewPlaylist(Option<Track>),
    EditPlaylist,
    DeletePlaylist,
    SaveCollection(String, bool),
    HomeChip(Option<String>),
    OpenNowPlaying(bool),
    /// Play/pause what's already loaded (a card's button on the playing item).
    TogglePlay,
}

enum Dialog {
    PickPlaylist(Track),
    NewPlaylist { title: String, track: Option<Track> },
    EditPlaylist { id: String, title: String, description: String, privacy: Option<String> },
    ConfirmDelete { id: String, title: String },
}

pub struct HomeState {
    pub chips: Vec<Chip>,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<String>,
    /// When the next shelves were requested (app time, s), while they load.
    pub loading_more: Option<f64>,
    pub picks_loading: bool,
    pub picks_expanded: bool,
}

pub struct Channels {
    pub resp_rx: Receiver<Resp>,
    pub audio_rx: Receiver<AudioEvent>,
    pub media_rx: Receiver<MediaControlEvent>,
}

pub struct App {
    backend: Backend,
    audio: AudioHandle,
    ch: Channels,
    media: Option<Media>,
    discord: Option<crate::discord::Discord>,
    discord_enabled: bool,
    /// Last state sent to Discord, to send only real changes.
    discord_sent: Option<crate::discord::Presence>,
    /// Last session's song, restored paused: Play starts it from here.
    resume_at: Option<Duration>,
    /// Album / playlist / artist the queue was started from, so its card shows pause.
    queue_source: Option<Target>,
    /// Source of a PlayCollection request until its tracks arrive.
    pending_source: Option<Target>,
    bg: Option<Background>,

    // navigation
    page: Page,
    back: Vec<Page>,
    forward: Vec<Page>,
    page_changed: f64,
    loading: bool,
    /// The current page's request failed with nothing cached: show a retry, not a skeleton.
    load_failed: bool,
    search_text: String,
    suggestions: (String, Vec<String>),
    suggest_due: Option<f64>,

    // page data
    home: Option<HomeState>,
    home_loading: bool,
    explore: Option<Explore>,
    search: Option<SearchResults>,
    collection: Option<Collection>,
    artist: Option<ArtistPage>,
    library: Option<Library>,
    /// When the library was last requested; the sidebar spinner gives up after a while.
    library_requested: std::time::Instant,
    history: Option<Vec<Track>>,
    uploads: Option<Vec<Track>>,
    /// Files chosen in the upload dialog, handed over from its thread.
    picked_uploads: Arc<std::sync::Mutex<Option<Vec<std::path::PathBuf>>>>,
    liked: HashSet<String>,
    account_name: Option<String>,
    account_avatar: Option<String>,
    /// Last launch's playlists, shown in the sidebar until the library arrives.
    sidebar_cache: Option<Vec<Card>>,

    // playback
    queue: Vec<Track>,
    index: Option<usize>,
    generation: u64,
    buffering: bool,
    /// Unplayable songs skipped in a row; a run of them stops playback instead of
    /// racing through the queue.
    skip_streak: u32,
    repeat: Repeat,
    autoplay: bool,
    /// Turn loud tracks down to YouTube's loudness target.
    normalize: bool,
    high_quality: bool,
    radio_pending: bool,
    seek_drag: Option<f32>,

    // visuals
    art_key: Option<String>,
    art_changed: f64,
    accent: Color32,
    default_theme: bool,
    /// Selected theme preset (see `preset.rs`); applies unless `default_theme`.
    theme_id: String,
    /// Preset whose chrome (glass, corner radius) is installed in the egui style.
    chrome_applied: Option<String>,
    anim_bg: bool,
    reactive_bg: bool,
    /// Background animation clock: advances only while music plays, so pausing
    /// freezes the motion instead of jumping when resumed.
    bg_time: f64,
    last_frame: f64,
    search_rect: Option<egui::Rect>,
    search_focused: bool,
    /// Mouse-wheel distance still to be scrolled (smooth scrolling).
    wheel_pending: egui::Vec2,
    wheel_mods: egui::Modifiers,

    // now playing
    np_open: bool,
    np_tab: NpTab,
    queue_open: bool,
    fullscreen: bool,
    /// Every source's lyrics for the current song (by video ID).
    lyrics: Option<(String, LyricsSet)>,
    /// Romanization/translation for one source's lines: (video ID, source key, language, extras).
    lyrics_extras: Option<(String, String, String, crate::translate::Extras)>,
    /// The extras last asked for, so each is requested once.
    extras_asked: Option<(String, String, String)>,
    show_roman: bool,
    show_trans: bool,
    /// Synced lyrics as the animated karaoke view; off shows them as plain text.
    karaoke: bool,
    lyric_prefs: lyric_prefs::Prefs,
    lyric_view: karaoke::View,
    lyric_clock: lyric_prefs::Clock,
    /// Rotation of a "record" cover (radians); advances only while playing.
    record_angle: f64,

    // account / misc
    logged_in: bool,
    /// The Spotify transfer, running on the network runtime.
    transfer: Arc<crate::transfer::Transfer>,
    xfer: transfer::Setup,
    /// Listen together: the room we're in, if any.
    jam: jam::Session,
    signing_in: Arc<AtomicBool>,
    toast: Option<(String, f64)>,

    // updates
    updater: Option<crate::update::Updater>,
    auto_update: bool,
    /// When the last automatic update check ran (app time, s).
    update_checked: Option<f64>,
    update_announced: bool,
    /// Release notes dialog (update ready, or "See all notes").
    notes_dialog: Option<notes::NotesDialog>,
    /// Version whose "Updated to" card is showing, on the first launch after an update.
    whats_new: Option<String>,
    /// The user picked "Restart to update": install on exit, then relaunch.
    restart_for_update: bool,

    // window
    /// Clicking × hides the window to the tray instead of quitting.
    close_to_tray: bool,
    /// The tray icon, while the window is hidden.
    tray: Option<crate::tray::Tray>,
    /// Closing for real (Quit from the tray, restart to update): don't hide.
    exiting: bool,
    /// Minimized, or hidden in the tray: the window's geometry isn't worth saving.
    window_hidden: bool,
    /// eframe's saved window geometry ("window") from the last time it was visible.
    good_window: Option<String>,
    /// The sidebar is a narrow icon rail.
    sidebar_collapsed: bool,
    actions: Vec<Action>,
    dialog: Option<Dialog>,
    share_pending: Option<String>,
    smoke: Option<String>,
}

/// Unplayable songs skipped in a row before playback stops.
const MAX_SKIPS: u32 = 3;

/// When a song failed because of the account (Premium-only, or needs signing in),
/// the message to show instead of skipping: the rest of an album like that fails the
/// same way.
fn account_block(msg: &str, logged_in: bool) -> Option<&'static str> {
    let m = msg.to_ascii_lowercase();
    if m.contains("premium") {
        Some("\"{title}\" needs YouTube Music Premium")
    } else if !logged_in && (m.contains("sign in") || m.contains("age-restricted")) {
        Some("Sign in to play \"{title}\"")
    } else {
        None
    }
}

impl App {
    pub fn new(
        backend: Backend,
        audio: AudioHandle,
        ch: Channels,
        media: Option<Media>,
        bg: Option<Background>,
        storage: Option<&dyn eframe::Storage>,
    ) -> Self {
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        let discord_enabled = get("discord_enabled").is_none_or(|v| v == "1");
        let logged_in = backend.is_logged_in();
        // Before any request: a song started at launch (dev hooks) must see this setting.
        backend.set_high_quality(get("high_quality").is_none_or(|v| v == "1"));
        backend.request(Req::Home(None));
        if logged_in {
            backend.request(Req::Library);
            backend.request(Req::Account);
            backend.warm_signed_in();
        }
        let transfer_handle = {
            let b = backend.clone();
            crate::transfer::Transfer::new(backend.data_dir(), Arc::new(move || b.wake()))
        };
        let mut app = Self {
            backend,
            audio,
            ch,
            media,
            discord: discord_enabled.then(crate::discord::Discord::spawn).flatten(),
            discord_enabled,
            discord_sent: None,
            resume_at: None,
            queue_source: None,
            pending_source: None,
            bg,
            page: Page::Home,
            back: Vec::new(),
            forward: Vec::new(),
            page_changed: 0.0,
            loading: false,
            load_failed: false,
            search_text: String::new(),
            suggestions: Default::default(),
            suggest_due: None,
            home: None,
            home_loading: true,
            explore: None,
            search: None,
            collection: None,
            artist: None,
            library: None,
            library_requested: std::time::Instant::now(),
            history: None,
            uploads: None,
            picked_uploads: Default::default(),
            liked: HashSet::new(),
            account_name: None,
            account_avatar: None,
            sidebar_cache: None,
            queue: Vec::new(),
            index: None,
            generation: 0,
            buffering: false,
            skip_streak: 0,
            repeat: match get("repeat").as_deref() {
                Some("all") => Repeat::All,
                Some("one") => Repeat::One,
                _ => Repeat::Off,
            },
            autoplay: get("autoplay").is_none_or(|v| v == "1"),
            normalize: get("normalize").is_none_or(|v| v == "1"),
            high_quality: get("high_quality").is_none_or(|v| v == "1"),
            updater: None,
            auto_update: get("auto_update").is_none_or(|v| v == "1"),
            close_to_tray: get("close_to_tray").is_some_and(|v| v == "1"),
            tray: None,
            exiting: false,
            window_hidden: false,
            good_window: get("window"),
            sidebar_collapsed: get("sidebar_collapsed").is_some_and(|v| v == "1"),
            update_checked: None,
            update_announced: false,
            notes_dialog: None,
            // First launch of a newer version than last time: show what changed.
            // YTM_WHATS_NEW=1 shows the card for this version regardless.
            whats_new: {
                let current = crate::update::current_version();
                let last = get("last_version");
                (std::env::var_os("YTM_WHATS_NEW").is_some() || last.is_some_and(|v| crate::update::newer(&current, &v))).then_some(current)
            },
            restart_for_update: false,
            radio_pending: false,
            seek_drag: None,
            art_key: None,
            art_changed: -10.0,
            accent: theme::RED,
            default_theme: get("default_theme").is_some_and(|v| v == "1") && std::env::var_os("YTM_THEME").is_none(),
            // Dev hook: YTM_THEME=<preset id> tries a theme without saving it.
            theme_id: std::env::var("YTM_THEME").ok().or_else(|| get("theme")).unwrap_or_else(|| crate::preset::DEFAULT_ID.to_owned()),
            chrome_applied: None,
            anim_bg: get("anim_bg").is_none_or(|v| v == "1"),
            reactive_bg: get("reactive_bg").is_none_or(|v| v == "1"),
            bg_time: 0.0,
            last_frame: 0.0,
            search_rect: None,
            search_focused: false,
            wheel_pending: egui::Vec2::ZERO,
            wheel_mods: egui::Modifiers::NONE,
            np_open: false,
            np_tab: if get("np_art").is_some_and(|v| v == "1") { NpTab::Art } else { NpTab::Lyrics },
            queue_open: false,
            fullscreen: false,
            lyrics: None,
            lyrics_extras: None,
            extras_asked: None,
            show_roman: get("show_roman").is_none_or(|v| v == "1"),
            show_trans: get("show_trans").is_none_or(|v| v == "1"),
            karaoke: get("lyrics_karaoke").is_some_and(|v| v == "1"),
            lyric_prefs: lyric_prefs::Prefs::load(&get),
            lyric_view: Default::default(),
            lyric_clock: Default::default(),
            record_angle: 0.0,
            logged_in,
            signing_in: Default::default(),
            transfer: transfer_handle,
            xfer: Default::default(),
            jam: Default::default(),
            toast: None,
            actions: Vec::new(),
            dialog: None,
            share_pending: None,
            smoke: None,
        };
        // Dev hooks: YTM_SMOKE="query" searches and plays the first song ("id:<videoId>[,<videoId>…]"
        // plays that video); YTM_SEARCH
        // only searches; YTM_PAGE opens a page; YTM_NOWPLAYING opens the overlay.
        if let Some(s) = get(SESSION_KEY).filter(|_| std::env::var_os("YTM_SMOKE").is_none()) {
            app.restore_session(&s);
        }
        let smoke = std::env::var("YTM_SMOKE").ok().filter(|q| !q.is_empty());
        if let Some(ids) = smoke.as_deref().and_then(|q| q.strip_prefix("id:")) {
            // "id:a,b,c" queues several.
            let track = |id: &str| Track { id: id.to_owned(), title: id.to_owned(), artists: vec![], album: None, duration: None, thumbs: vec![], track_nr: None, plays: None };
            app.play_list(ids.split(',').map(track).collect(), 0);
        } else if let Some(q) = smoke {
            app.search_text = q.clone();
            app.go(Page::Search(q.clone()));
            app.smoke = Some(q);
        } else if let Some(q) = std::env::var("YTM_SEARCH").ok().filter(|q| !q.is_empty()) {
            app.search_text = q.clone();
            app.go(Page::Search(q));
        }
        // An explicit page replaces the smoke search, so its query shouldn't linger.
        if matches!(std::env::var("YTM_PAGE").as_deref(), Ok("home" | "explore" | "library" | "uploads" | "liked" | "history" | "settings" | "transfer")) {
            app.search_text.clear();
        }
        match std::env::var("YTM_PAGE").as_deref() {
            Ok("home") => app.go(Page::Home),
            Ok("explore") => app.go(Page::Explore),
            Ok("library") => app.go(Page::Library(LibTab::Playlists)),
            Ok("uploads") => app.go(Page::Library(LibTab::Uploads)),
            Ok("liked") => app.go(Page::Library(LibTab::Songs)),
            Ok("history") => app.go(Page::Library(LibTab::History)),
            Ok("settings") => app.go(Page::Settings),
            Ok("transfer") => app.go(Page::Transfer),
            Ok("jam") => app.go(Page::Jam),
            Ok(p) if p.starts_with("album:") => app.go(Page::Album(p[6..].to_owned())),
            Ok(p) if p.starts_with("artist:") => app.go(Page::Artist(p[7..].to_owned())),
            Ok(p) if p.starts_with("playlist:") => app.go(Page::Playlist(p[9..].to_owned())),
            _ => {}
        }
        // YTM_UPLOAD=<file> uploads that file as if it had been picked.
        if let Some(f) = std::env::var_os("YTM_UPLOAD") {
            *app.picked_uploads.lock().unwrap() = Some(vec![f.into()]);
        }
        if std::env::var("YTM_NOWPLAYING").is_ok() {
            app.np_open = true;
        }
        app.jam.name = get("jam_name").unwrap_or_default();
        // Last launch's name, picture and playlists, so they show at once; the account
        // menu and the library replace them when they arrive.
        if app.logged_in
            && let Some(c) = get("account_cache").and_then(|j| serde_json::from_str::<AccountCache>(&j).ok())
        {
            app.account_name = c.name;
            app.account_avatar = c.avatar;
            app.sidebar_cache = Some(c.playlists).filter(|p| !p.is_empty());
        }
        // Dev hook: YTM_QUEUE opens the queue drawer.
        if std::env::var_os("YTM_QUEUE").is_some() {
            app.queue_open = true;
        }
        jam::dev_hooks(&mut app);
        app.audio.set_normalize(app.normalize);
        app
    }

    // ---------------------------------------------------------------- navigation

    fn go(&mut self, page: Page) {
        self.np_open = false;
        if page == self.page {
            // Clicking the page you're on refreshes it.
            self.refresh();
            return;
        }
        let prev = std::mem::replace(&mut self.page, page);
        self.back.push(prev);
        self.forward.clear();
        self.page_loaded();
    }

    fn nav_back(&mut self) {
        if self.np_open {
            self.np_open = false;
        } else if let Some(p) = self.back.pop() {
            self.forward.push(std::mem::replace(&mut self.page, p));
            self.page_loaded();
        }
    }

    fn nav_forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(std::mem::replace(&mut self.page, p));
            self.page_loaded();
        }
    }

    /// Reload the current page's data in place: no page transition, the page stays as
    /// it is until the new data arrives.
    fn refresh(&mut self) {
        let changed = self.page_changed;
        self.page_loaded();
        self.page_changed = changed;
    }

    /// Start the page transition and (re)load the page's data. Every page refreshes on
    /// each visit (things change outside the app: likes, other devices, new releases),
    /// showing the cached copy meanwhile.
    fn page_loaded(&mut self) {
        self.page_changed = -1.0; // stamped with the frame time on the next pass
        let req = match &self.page {
            Page::Home if !self.home_loading => {
                self.home_loading = true;
                Some(Req::Home(None))
            }
            Page::Explore => Some(Req::Explore(self.preferred_artist_ids())),
            Page::Search(q) if self.search.as_ref().is_none_or(|s| &s.query != q) => Some(Req::Search(q.clone())),
            Page::Album(id) => Some(Req::Album(id.clone())),
            Page::Playlist(id) => Some(Req::Playlist(id.clone())),
            Page::Artist(id) => Some(Req::Artist(id.clone())),
            Page::Library(LibTab::History) if self.logged_in => Some(Req::History),
            Page::Library(LibTab::Uploads) if self.logged_in => Some(Req::Uploads),
            Page::Library(_) if self.logged_in => Some(Req::Library),
            _ => None,
        };
        // The spinner is only for a page with nothing to show yet.
        let cached = match &self.page {
            Page::Home => self.home.is_some(),
            Page::Explore => self.explore.is_some(),
            Page::Album(id) | Page::Playlist(id) => self.collection.as_ref().is_some_and(|c| &c.id == id),
            Page::Artist(id) => self.artist.as_ref().is_some_and(|a| &a.id == id),
            Page::Library(LibTab::History) => self.history.is_some(),
            Page::Library(LibTab::Uploads) => self.uploads.is_some(),
            Page::Library(_) => self.library.is_some(),
            _ => false,
        };
        self.loading = req.is_some() && !cached;
        self.load_failed = false;
        match req {
            Some(Req::Library) => self.request_library(),
            Some(r) => self.backend.request(r),
            None => {}
        }
    }

    fn request_library(&mut self) {
        self.library_requested = std::time::Instant::now();
        self.backend.request(Req::Library);
    }

    fn preferred_artist_ids(&self) -> Vec<String> {
        let mut ids = HashSet::new();
        if let Some(home) = &self.home {
            for shelf in &home.shelves {
                if shelf.title != "Listen again" && shelf.title != "Quick picks" { continue; }
                match &shelf.kind {
                    ShelfKind::Cards(cards) => {
                        for card in cards {
                            ids.extend(card.links.iter().filter_map(|link| link.id.clone()));
                            if let Target::Song(track) = &card.target {
                                ids.extend(track.artists.iter().filter_map(|artist| artist.id.clone()));
                            }
                        }
                    }
                    ShelfKind::Songs { tracks, .. } => {
                        for track in tracks {
                            ids.extend(track.artists.iter().filter_map(|artist| artist.id.clone()));
                        }
                    }
                }
            }
        }
        ids.into_iter().collect()
    }

    // ---------------------------------------------------------------- playback

    pub fn current(&self) -> Option<&Track> {
        self.index.and_then(|i| self.queue.get(i))
    }

    /// The lyrics shown for the current song: the song's pick, then the listener's order.
    fn lyric_choice(&self) -> Option<&Lyrics> {
        let (id, set) = self.lyrics.as_ref()?;
        set.get(self.lyric_prefs.choose(id, set)?)
    }

    /// Lyrics have loaded, and no enabled source has any.
    fn no_lyrics(&self) -> bool {
        self.lyrics.is_some() && self.lyric_choice().is_none()
    }

    /// Ask for romanization/translation of the shown source once, when they're switched on
    /// and some line needs one the lyrics don't ship.
    fn request_lyric_extras(&mut self) {
        if !(self.show_roman || self.show_trans) {
            return;
        }
        let Some(l) = self.lyric_choice() else { return };
        let Some((id, _)) = &self.lyrics else { return };
        let target = self.lyric_prefs.target.clone();
        let want = (id.clone(), l.provider.key().to_owned(), target.clone());
        if self.extras_asked.as_ref() == Some(&want) {
            return;
        }
        let needs = l.lines.iter().any(|x| {
            !x.instrumental
                && !x.text.trim().is_empty()
                && ((self.show_roman && x.romanization.is_none() && crate::translate::non_latin(&x.text))
                    || (self.show_trans && lyric_prefs::shipped_translation(x, &target).is_none()))
        });
        let req = needs.then(|| Req::LyricsExtras {
            video_id: id.clone(),
            source: l.provider.key().to_owned(),
            lines: l.lines.iter().map(|x| if x.instrumental { String::new() } else { x.text.clone() }).collect(),
            lang: l.language.clone(),
            target,
        });
        self.extras_asked = Some(want);
        if let Some(r) = req {
            self.backend.request(r);
        }
    }

    fn current_id(&self) -> Option<&str> {
        self.current().map(|t| t.id.as_str())
    }

    fn playing(&self) -> bool {
        self.audio.status.playing.load(Ordering::Relaxed)
    }

    /// Fetch the current song's art for the background and accent. The default theme
    /// shows neither, so it skips the download and blur until the theme is turned off.
    fn request_art(&mut self) {
        if self.default_theme {
            return;
        }
        let Some(track) = self.current() else { return };
        if self.art_key.as_deref() == Some(track.id.as_str()) {
            return;
        }
        if let Some(url) = backend::art_url(track) {
            let key = track.id.clone();
            self.backend.request(Req::Art { key, url });
        }
    }

    fn accent_color(&self) -> Color32 {
        if self.default_theme { theme::RED } else { self.look().accent(self.accent) }
    }

    /// The active theme preset; the classic red theme uses the built-in look's layout.
    /// Synced once per frame (`sync_look`), so this is a plain index, not a search.
    fn look(&self) -> &'static crate::preset::Preset {
        crate::preset::active()
    }

    fn sync_look(&self) {
        let id = if self.default_theme { crate::preset::DEFAULT_ID } else { &self.theme_id };
        if crate::preset::active().id != id {
            crate::preset::set_active(id);
        }
    }

    /// Height of the bottom player area (a floating bar leaves a margin around it).
    fn bar_height(&self) -> f32 {
        if self.look().chrome.player_bar == crate::preset::PlayerBar::Floating { 100.0 } else { 86.0 }
    }

    fn set_discord_enabled(&mut self, enabled: bool) {
        self.discord_enabled = enabled;
        if enabled {
            if self.discord.is_none() {
                self.discord = crate::discord::Discord::spawn();
            }
        } else {
            if let Some(discord) = self.discord.take() {
                discord.set(None);
            }
            self.discord_sent = None;
        }
    }

    /// Mirror playback into Discord. Cheap enough to run every frame: it only sends on a
    /// new song, play/pause, a seek, or a newly learned duration.
    fn sync_discord(&mut self) {
        let Some(d) = &self.discord else { return };
        let track = self.current().filter(|_| self.playing() && !self.buffering);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        let start_ms = now - self.audio.status.position().as_millis() as i64;
        let dur = self.audio.status.duration_ms.load(Ordering::Relaxed) as i64;
        // Position ticks every 100 ms; only a jump of 2 s or more is a real seek.
        let same = match (track, &self.discord_sent) {
            (Some(t), Some(b)) => b.video_id == t.id && (start_ms - b.start_ms).abs() < 2000 && b.end_ms.is_some() == (dur > 0),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        let want = track.map(|t| crate::discord::Presence {
            video_id: t.id.clone(),
            title: t.title.clone(),
            artist: t.artist_line(),
            album: t.album.as_ref().map(|a| a.name.clone()),
            art: backend::art_url(t),
            start_ms,
            end_ms: (dur > 0).then_some(start_ms + dur),
        });
        d.set(want.clone());
        self.discord_sent = want;
    }

    fn play_list(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.queue = tracks;
        self.index = Some(start.min(self.queue.len() - 1));
        self.play_current();
    }

    /// Put last session's queue back, paused at the saved position, without loading audio.
    fn restore_session(&mut self, json: &str) {
        let Ok(s) = serde_json::from_str::<Session>(json) else { return };
        let Some(track) = s.queue.get(s.index).cloned() else { return };
        self.queue = s.queue;
        self.index = Some(s.index);
        let dur = track.duration.map_or(s.duration_ms, |d| d as u64 * 1000);
        self.audio.status.duration_ms.store(dur, Ordering::Relaxed);
        self.audio.status.position_ms.store(s.position_ms, Ordering::Relaxed);
        self.resume_at = Some(Duration::from_millis(s.position_ms));
        self.request_art();
        self.backend.request(Req::Lyrics(track));
    }

    fn session_json(&self) -> Option<String> {
        let i = self.index?;
        // Keep the saved queue small: liked-songs queues can run to 1000+ tracks.
        let from = i.saturating_sub(50);
        let to = (i + 150).min(self.queue.len());
        serde_json::to_string(&Session {
            queue: self.queue[from..to].to_vec(),
            index: i - from,
            position_ms: self.audio.status.position().as_millis() as u64,
            duration_ms: self.audio.status.duration_ms.load(Ordering::Relaxed),
        })
        .ok()
    }

    fn play_current(&mut self) {
        self.play_current_at(Duration::ZERO);
    }

    /// Start the current track, opened directly at `at` so nothing plays from 0:00 first.
    fn play_current_at(&mut self, at: Duration) {
        let Some(track) = self.current().cloned() else { return };
        self.resume_at = None;
        self.generation += 1;
        self.audio.status.wanted_gen.store(self.generation, Ordering::Relaxed);
        self.audio.status.duration_ms.store(track.duration.map_or(0, |d| d as u64 * 1000), Ordering::Relaxed);
        self.audio.send(AudioCmd::Stop);
        self.buffering = true;
        self.seek_drag = None;
        self.backend.request(Req::Play { video_id: track.id.clone(), generation: self.generation, at });
        if let Some(m) = &mut self.media {
            m.set_track(
                &track.title,
                &track.artist_line(),
                track.album.as_ref().map(|a| a.name.as_str()),
                backend::art_url(&track).as_deref(),
                track.duration,
            );
            m.set_playing(true);
        }
        // Background art and lyrics follow the track.
        self.request_art();
        self.lyrics = None;
        self.lyrics_extras = None;
        self.extras_asked = None;
        self.lyric_view.reset();
        self.backend.request(Req::Lyrics(track.clone()));
        // Keep the queue topped up so autoplay never stalls at the end.
        if self.autoplay && !self.radio_pending && !self.jam.guest() && self.index.is_some_and(|i| i + 1 >= self.queue.len()) {
            self.radio_pending = true;
            self.backend.request(Req::Radio(track.id));
        }
    }

    /// Fetch whatever plays after the current track so skipping is instant.
    fn prefetch_next(&self) {
        let Some(i) = self.index else { return };
        let next = match self.repeat {
            Repeat::One => None,
            Repeat::All => self.queue.get(i + 1).or(self.queue.first()),
            Repeat::Off => self.queue.get(i + 1),
        };
        if let Some(t) = next {
            self.backend.request(Req::Prefetch(t.id.clone()));
        }
    }

    fn next(&mut self) {
        if self.jam.guest() {
            return jam::request(self, crate::jam::Cmd::Skip);
        }
        let Some(i) = self.index else { return };
        if i + 1 < self.queue.len() {
            self.index = Some(i + 1);
        } else if self.repeat == Repeat::All && !self.queue.is_empty() {
            self.index = Some(0);
        } else {
            return;
        }
        self.play_current();
    }

    fn prev(&mut self) {
        if self.jam.guest() {
            return jam::request(self, crate::jam::Cmd::Prev);
        }
        if self.audio.status.position() > Duration::from_secs(3) {
            self.audio.send(AudioCmd::Seek(Duration::ZERO));
            return;
        }
        if let Some(i) = self.index.filter(|&i| i > 0) {
            self.index = Some(i - 1);
            self.play_current();
        }
    }

    fn toggle(&mut self) {
        if self.index.is_none() {
            return;
        }
        if self.jam.guest() {
            return jam::request(self, crate::jam::Cmd::Toggle);
        }
        if let Some(at) = self.resume_at {
            self.play_current_at(at);
            return;
        }
        let playing = self.playing();
        // The media controls catch up in `logic` once the player has switched.
        self.audio.send(if playing { AudioCmd::Pause } else { AudioCmd::Resume });
    }

    fn shuffle_upcoming(&mut self) {
        if self.jam.guest() {
            return self.toast("Only the host can shuffle the jam");
        }
        let start = self.index.map_or(0, |i| i + 1);
        if start < self.queue.len() {
            shuffle(&mut self.queue[start..]);
            self.toast("Shuffled up next");
            self.prefetch_next();
        }
    }

    /// Automatic update checks: shortly after launch, then every six hours.
    fn tick_updates(&mut self, ctx: &egui::Context, now: f64) {
        use crate::update::{State, Updater};
        if self.updater.is_none() {
            let c = ctx.clone();
            self.updater = Some(Updater::new(self.backend.data_dir(), move || c.request_repaint()));
        }
        let Some(u) = self.updater.clone() else { return };
        let due = match self.update_checked {
            None => now > 8.0,
            Some(t) => now - t > 6.0 * 3600.0 && !matches!(u.state(), State::Ready { .. }),
        };
        let enabled = self.auto_update && installed() && dev_var("YTM_BACKGROUND").is_none_or(|_| dev_var("YTM_UPDATE_FORCE").is_some());
        if enabled && due {
            self.update_checked = Some(now);
            u.check();
        } else if enabled {
            // An idle app doesn't repaint on its own: wake up when the next check is due.
            let next = self.update_checked.map_or(8.0, |t| t + 6.0 * 3600.0);
            ctx.request_repaint_after(Duration::from_secs_f64((next - now).max(0.5) + 0.1));
        }
        if let State::Ready { version, .. } = u.state()
            && !self.update_announced
        {
            self.update_announced = true;
            self.notes_dialog = Some(notes::NotesDialog { version, ready: true });
        }
    }

    /// Close the app so the downloaded update installs, then relaunch it.
    fn restart_to_update(&mut self, ctx: &egui::Context) {
        self.restart_for_update = true;
        self.quit(ctx);
    }

    /// Close the app for real, even with "Close to tray" on.
    fn quit(&mut self, ctx: &egui::Context) {
        self.exiting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// "Close to tray": × hides the window behind a tray icon; the icon brings it
    /// back or quits. eframe keeps running `logic` while the window is hidden.
    fn tray_window(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.close_to_tray && !self.exiting {
            let c = ctx.clone();
            match crate::tray::Tray::new(move || c.request_repaint()) {
                Ok(t) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    self.tray = Some(t);
                }
                // Without an icon there'd be no way back: just close.
                Err(e) => log::warn!("tray icon unavailable: {e:#}"),
            }
        }
        if self.tray.is_none() {
            return;
        }
        // The first click decides; the icon goes away with it.
        match crate::tray::take().into_iter().next() {
            Some(crate::tray::Cmd::Show) => self.show_window(ctx),
            Some(crate::tray::Cmd::Quit) => {
                self.tray = None;
                self.quit(ctx);
            }
            None => {}
        }
    }

    /// Bring the window back (from the tray, or minimized) and to the front.
    fn show_window(&mut self, ctx: &egui::Context) {
        self.tray = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), -1.0));
    }

    fn apply(&mut self, a: Action) {
        // In someone else's jam, playback changes become requests to the host.
        let a = if self.jam.guest() {
            match jam::guest_action(self, a) {
                Some(a) => a,
                None => return,
            }
        } else {
            a
        };
        match a {
            // Clicking the song that's already playing pauses/resumes it, not restart it.
            Action::Play(tracks, i) if tracks.get(i).is_some_and(|t| self.current_id() == Some(t.id.as_str())) && self.resume_at.is_none() => {
                self.toggle()
            }
            Action::Play(tracks, i) => {
                // Play buttons and rows live on the page they play from.
                self.queue_source = match &self.page {
                    Page::Album(id) => Some(Target::Album(id.clone())),
                    Page::Playlist(id) => Some(Target::Playlist(id.clone())),
                    Page::Artist(id) => Some(Target::Artist(id.clone())),
                    _ => None,
                };
                self.play_list(tracks, i);
            }
            Action::TogglePlay => self.toggle(),
            Action::PlayCollection(target, shuffle) => {
                self.toast("Starting playback…");
                self.pending_source = Some(target.clone());
                self.backend.request(Req::PlayCollection { target, shuffle });
            }
            Action::Open(Target::Song(t)) | Action::Radio(t) => {
                self.queue_source = None;
                self.play_list(vec![t.clone()], 0);
                if !self.radio_pending {
                    self.radio_pending = true;
                    self.backend.request(Req::Radio(t.id));
                }
            }
            Action::Open(Target::Album(id)) => self.go(Page::Album(id)),
            Action::Open(Target::Playlist(id)) => self.go(Page::Playlist(id)),
            Action::Open(Target::Artist(id)) => self.go(Page::Artist(id)),
            Action::PlayNext(t) => {
                let at = self.index.map_or(0, |i| i + 1).min(self.queue.len());
                self.queue.insert(at, t);
                self.toast("Playing next");
                if self.index.is_none() {
                    self.index = Some(0);
                    self.play_current();
                } else {
                    self.prefetch_next();
                }
            }
            Action::Enqueue(t) => {
                self.queue.push(t);
                self.toast("Added to queue");
                if self.index.is_none() {
                    self.index = Some(self.queue.len() - 1);
                    self.play_current();
                }
            }
            Action::Go(p) => self.go(p),
            Action::QueueJump(i) => {
                self.index = Some(i);
                self.play_current();
            }
            Action::QueueRemove(i) => {
                if i < self.queue.len() && Some(i) != self.index {
                    self.queue.remove(i);
                    if let Some(cur) = self.index.filter(|&c| c > i) {
                        self.index = Some(cur - 1);
                    }
                }
            }
            Action::Seek(secs) => {
                let at = Duration::from_secs_f32(secs.max(0.0));
                if self.resume_at.is_some() {
                    // Restored but not loaded yet: move the start point instead.
                    self.resume_at = Some(at);
                    self.audio.status.position_ms.store(at.as_millis() as u64, Ordering::Relaxed);
                } else {
                    self.audio.send(AudioCmd::Seek(at));
                }
                self.lyric_view.resume();
            }
            Action::Like(id, like) => {
                if !self.logged_in {
                    self.toast("Sign in to like songs");
                    return;
                }
                // Optimistic: update the like state and the cached Liked songs list now.
                if like {
                    self.liked.insert(id.clone());
                    let track = self.queue.iter().find(|t| t.id == id).cloned();
                    if let (Some(lib), Some(t)) = (&mut self.library, track) {
                        if !lib.liked.iter().any(|x| x.id == id) {
                            lib.liked.insert(0, t);
                        }
                    }
                } else {
                    self.liked.remove(&id);
                    if let Some(lib) = &mut self.library {
                        lib.liked.retain(|t| t.id != id);
                    }
                }
                self.backend.request(Req::Like { video_id: id, like });
            }
            Action::Dislike(id) => {
                if !self.logged_in { self.toast("Sign in to rate songs"); return; }
                self.liked.remove(&id);
                if let Some(lib) = &mut self.library { lib.liked.retain(|t| t.id != id); }
                self.backend.request(Req::Dislike(id));
            }
            Action::Share(target) => {
                self.share_pending = Some(share_url(&target));
                self.toast("Link copied");
            }
            Action::AddToPlaylist(track) => {
                if self.logged_in { self.dialog = Some(Dialog::PickPlaylist(track)); }
                else { self.toast("Sign in to edit playlists"); }
            }
            Action::NewPlaylist(track) => {
                if self.logged_in { self.dialog = Some(Dialog::NewPlaylist { title: String::new(), track }); }
                else { self.toast("Sign in to create playlists"); }
            }
            Action::EditPlaylist => {
                if let Some(c) = self.collection.as_ref().filter(|c| !c.is_album) {
                    self.dialog = Some(Dialog::EditPlaylist {
                        id: c.id.clone(), title: c.title.clone(), description: c.description.clone().unwrap_or_default(), privacy: None,
                    });
                }
            }
            Action::DeletePlaylist => {
                if let Some(c) = self.collection.as_ref().filter(|c| !c.is_album) {
                    self.dialog = Some(Dialog::ConfirmDelete { id: c.id.clone(), title: c.title.clone() });
                }
            }
            Action::SaveCollection(id, save) => {
                if self.logged_in { self.backend.request(Req::SaveCollection { id, save }); }
                else { self.toast("Sign in to save to library"); }
            }
            Action::HomeChip(params) => {
                self.home_loading = true;
                if let Some(h) = &mut self.home {
                    h.shelves.clear();
                    for c in &mut h.chips {
                        c.selected = false;
                    }
                }
                self.backend.request(Req::Home(params));
            }
            Action::OpenNowPlaying(open) => {
                if self.current().is_some() {
                    self.np_open = open;
                }
            }
        }
    }

    // ---------------------------------------------------------------- events

    fn pump(&mut self, now: f64) {
        while let Ok(r) = self.ch.resp_rx.try_recv() {
            self.on_resp(r, now);
        }
        while let Ok(ev) = self.ch.audio_rx.try_recv() {
            match ev {
                AudioEvent::Finished { generation } if generation == self.generation && self.jam.guest() => {}
                AudioEvent::Finished { generation } if generation == self.generation => {
                    if self.repeat == Repeat::One {
                        self.play_current();
                    } else {
                        self.next();
                    }
                }
                AudioEvent::Finished { .. } => {}
                AudioEvent::Error(e) => self.toast(e),
            }
        }
        while let Ok(ev) = self.ch.media_rx.try_recv() {
            match ev {
                MediaControlEvent::Play | MediaControlEvent::Pause if self.jam.guest() => self.toggle(),
                MediaControlEvent::Play if self.resume_at.is_some() => self.toggle(),
                MediaControlEvent::Play => self.audio.send(AudioCmd::Resume),
                MediaControlEvent::Pause | MediaControlEvent::Stop => self.audio.send(AudioCmd::Pause),
                MediaControlEvent::Toggle => self.toggle(),
                MediaControlEvent::Next => self.next(),
                MediaControlEvent::Previous => self.prev(),
                _ => {}
            }
        }
        if self.buffering && self.playing() {
            self.buffering = false;
            self.skip_streak = 0;
            self.prefetch_next();
            // Dev hook: YTM_SEEK=<secs> jumps into the first track once it starts.
            if self.generation == 1 {
                if let Some(s) = std::env::var("YTM_SEEK").ok().and_then(|v| v.parse::<f32>().ok()) {
                    self.audio.send(AudioCmd::Seek(Duration::from_secs_f32(s)));
                }
            }
        }
        if self.suggest_due.is_some_and(|d| now >= d) {
            self.suggest_due = None;
            let q = self.search_text.trim().to_owned();
            if !q.is_empty() {
                self.backend.request(Req::Suggest(q));
            }
        }
    }

    fn on_resp(&mut self, r: Resp, now: f64) {
        match r {
            Resp::Home(page) => {
                // Until the account menu answers (Req::Account).
                if self.account_avatar.is_none() {
                    self.account_avatar = page.shelves.iter().find(|s| s.title == "Listen again").and_then(|s| s.strap_thumb.clone());
                }
                let mut chips = page.chips;
                // The feed marks the active mood itself; keep the bar when it doesn't send chips.
                if chips.is_empty() {
                    chips = self.home.as_ref().map(|h| h.chips.clone()).unwrap_or_default();
                }
                self.home = Some(HomeState {
                    chips,
                    shelves: page.shelves,
                    continuation: page.continuation,
                    loading_more: None,
                    picks_loading: false,
                    picks_expanded: false,
                });
                self.home_loading = false;
                if self.page == Page::Home {
                    self.loading = false;
                }
            }
            Resp::HomeMore(page) => {
                if let Some(h) = &mut self.home {
                    h.shelves.extend(page.shelves);
                    h.continuation = page.continuation;
                    h.loading_more = None;
                }
            }
            Resp::Explore(e) => {
                self.explore = Some(e);
                self.loading = false;
            }
            Resp::Search(r) => {
                if self.smoke.as_ref() == Some(&r.query) {
                    self.smoke = None;
                    if !r.tracks.is_empty() {
                        log::info!("smoke: playing {}", r.tracks[0].title);
                        self.play_list(r.tracks.clone(), 0);
                    }
                }
                self.search = Some(r);
                self.loading = false;
            }
            Resp::Suggest { query, terms } => {
                if self.search_text.trim() == query {
                    self.suggestions = (query, terms);
                }
            }
            Resp::Collection(c) => {
                self.collection = Some(c);
                self.loading = false;
            }
            Resp::Artist(a) => {
                self.artist = Some(a);
                self.loading = false;
            }
            Resp::Library(l) => {
                self.liked = l.liked.iter().map(|t| t.id.clone()).collect();
                self.library = Some(l);
                self.loading = false;
            }
            Resp::History(h) => {
                self.history = Some(h);
                self.loading = false;
            }
            Resp::Uploads(t) => {
                self.uploads = Some(t);
                if self.page == Page::Library(LibTab::Uploads) {
                    self.loading = false;
                }
            }
            Resp::Uploaded { done, failed } => {
                let songs = |n: usize| if n == 1 { "1 song".to_owned() } else { format!("{n} songs") };
                self.toast(match (done, failed.len()) {
                    (_, 0) => format!("Uploaded {}. It can take a minute for YouTube Music to process them.", songs(done)),
                    (0, _) => format!("Upload failed: {}", failed.join(", ")),
                    _ => format!("Uploaded {}; failed: {}", songs(done), failed.join(", ")),
                });
            }
            Resp::Lyrics { video_id, lyrics } => {
                if self.current_id() == Some(video_id.as_str()) {
                    let available = self.lyric_prefs.available(&lyrics);
                    let shown = self.lyric_prefs.choose(&video_id, &lyrics);
                    let missing: Vec<&str> = self.lyric_prefs.order.iter().filter(|(p, on)| *on && !available.contains(p)).map(|(p, _)| p.key()).collect();
                    log::info!(
                        "lyrics: showing {} ({}/{}); available: {}; no lyrics from: {}",
                        shown.map_or("nothing", |p| p.key()),
                        shown.and_then(|p| available.iter().position(|x| *x == p)).map_or(0, |i| i + 1),
                        available.len(),
                        available.iter().map(|p| p.key()).collect::<Vec<_>>().join(", "),
                        missing.join(", ")
                    );
                    self.lyrics = Some((video_id, lyrics));
                }
            }
            Resp::LyricsExtras { video_id, source, target, extras } => {
                if self.current_id() == Some(video_id.as_str()) {
                    self.lyrics_extras = Some((video_id, source, target, extras));
                }
            }
            Resp::Art { key, art } => {
                if self.current_id() == Some(key.as_str()) {
                    self.accent = art.accent;
                    if let Some(bg) = &self.bg {
                        bg.set_art(art);
                    }
                    self.art_key = Some(key);
                    self.art_changed = now;
                }
            }
            Resp::Radio { seed, tracks } => {
                self.radio_pending = false;
                let have: HashSet<String> = self.queue.iter().map(|t| t.id.clone()).collect();
                let was_at_end = self.index.is_some_and(|i| i + 1 >= self.queue.len());
                self.queue.extend(tracks.into_iter().filter(|t| t.id != seed && !have.contains(&t.id)));
                if was_at_end && self.index.is_some() && !self.buffering && !self.playing() {
                    // The track ended while radio was loading: continue now.
                    self.next();
                } else if !self.buffering {
                    self.prefetch_next();
                }
            }
            Resp::PlayQueue { tracks } => {
                self.queue_source = self.pending_source.take();
                self.play_list(tracks, 0);
            }
            Resp::Liked { video_id, like } => {
                if like {
                    self.liked.insert(video_id);
                }
            }
            Resp::QuickPicksMore { seed, tracks } => {
                if let Some(h) = &mut self.home {
                    h.picks_loading = false;
                    if let Some(shelf) = h.shelves.iter_mut().find(|s| s.title == "Quick picks") {
                        if let ShelfKind::Songs { tracks: picks, .. } = &mut shelf.kind {
                            if picks.iter().any(|t| t.id == seed) {
                                h.picks_expanded = true;
                                let mut seen: HashSet<String> = picks.iter().map(|t| t.id.clone()).collect();
                                picks.extend(tracks.into_iter().filter(|t| seen.insert(t.id.clone())).take(24));
                            }
                        }
                    }
                }
            }
            Resp::PlaylistCreated { id, had_song } => {
                self.request_library();
                self.toast(if had_song { "Song added to new playlist" } else { "Playlist created" });
                self.go(Page::Playlist(id));
            }
            Resp::PlaylistChanged { id, message } => {
                self.request_library();
                if let Some(id) = id {
                    if self.page == Page::Playlist(id.clone()) { self.backend.request(Req::Playlist(id)); }
                } else if matches!(self.page, Page::Playlist(_)) {
                    self.go(Page::Library(LibTab::Playlists));
                }
                self.toast(message);
            }
            Resp::PlayError { generation, msg } => {
                if generation == self.generation {
                    self.buffering = false;
                    let title = self.current().map(|t| t.title.clone()).unwrap_or_default();
                    let reason = if msg.contains("age-restricted") || msg.contains("confirm your age") {
                        "it's age-restricted".to_owned()
                    } else {
                        msg
                    };
                    let has_next = self.index.is_some_and(|i| i + 1 < self.queue.len());
                    // Skipping can't help when the account is the problem: the rest of
                    // an album like this needs it too. Stop on the song instead.
                    if let Some(why) = account_block(&reason, self.logged_in) {
                        self.skip_streak = 0;
                        self.toast(why.replace("{title}", &title));
                    } else if has_next && self.skip_streak < MAX_SKIPS {
                        // Don't stall the queue on one unplayable song.
                        self.skip_streak += 1;
                        self.toast(format!("Skipped \"{title}\": {reason}"));
                        self.next();
                    } else {
                        self.skip_streak = 0;
                        self.toast(format!("Can't play \"{title}\": {reason}"));
                    }
                }
            }
            Resp::Account { name, photo } => {
                self.account_name = name.or(self.account_name.take());
                self.account_avatar = photo.or(self.account_avatar.take());
            }
            Resp::LoggedIn(v) => {
                self.logged_in = v;
                self.account_name = None;
                self.account_avatar = None;
                self.sidebar_cache = None;
                self.library = None;
                self.history = None;
                self.uploads = None;
                self.home = None;
                self.home_loading = true;
                self.backend.request(Req::Home(None));
                if v {
                    self.request_library();
                    self.backend.request(Req::Account);
                } else {
                    self.liked.clear();
                }
                self.toast(if v { "Signed in" } else { "Signed out" });
            }
            Resp::Error(e) => {
                // A page still waiting for its first data shows a retry instead of loading forever.
                self.load_failed |= self.loading || (self.page == Page::Home && self.home_loading);
                self.loading = false;
                self.home_loading = false;
                self.radio_pending = false;
                if let Some(h) = &mut self.home {
                    h.loading_more = None;
                    h.picks_loading = false;
                }
                self.toast(e);
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let i = ctx.input(|i| {
            let k = |key| !typing && i.key_pressed(key);
            (
                k(egui::Key::Space),
                i.pointer.button_pressed(egui::PointerButton::Extra1)
                    || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
                i.pointer.button_pressed(egui::PointerButton::Extra2)
                    || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)),
                i.modifiers.ctrl && i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.ctrl && i.key_pressed(egui::Key::ArrowLeft),
                k(egui::Key::L),
                k(egui::Key::F),
                i.key_pressed(egui::Key::Escape),
                i.modifiers.ctrl && i.key_pressed(egui::Key::ArrowUp),
                i.modifiers.ctrl && i.key_pressed(egui::Key::ArrowDown),
            )
        });
        let (space, back, fwd, next, prev, lyr, full, esc, vol_up, vol_down) = i;
        if space {
            self.toggle();
        }
        if back {
            self.nav_back();
        }
        if fwd {
            self.nav_forward();
        }
        if next {
            self.next();
        }
        if prev {
            self.prev();
        }
        if lyr && self.current().is_some() {
            self.np_open = !self.np_open;
            self.np_tab = NpTab::Lyrics;
        }
        if full && self.np_open {
            self.set_fullscreen(ctx, !self.fullscreen);
        }
        if esc {
            if self.fullscreen {
                self.set_fullscreen(ctx, false);
            } else {
                self.np_open = false;
            }
        }
        if vol_up || vol_down {
            let v = (self.audio.volume() + if vol_up { 0.05 } else { -0.05 }).clamp(0.0, 1.0);
            self.audio.set_volume(v);
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// Open the system file picker for songs to upload; it runs on its own thread.
    fn pick_uploads(&self, ctx: &egui::Context) {
        let (slot, ctx) = (self.picked_uploads.clone(), ctx.clone());
        std::thread::spawn(move || {
            let files = rfd::FileDialog::new().set_title("Upload music").add_filter("Audio", UPLOAD_EXTS).pick_files();
            if let Some(files) = files {
                *slot.lock().unwrap() = Some(files);
                ctx.request_repaint();
            }
        });
    }

    fn upload_files(&mut self, files: Vec<std::path::PathBuf>) {
        let (ok, skipped): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| {
            f.extension().and_then(|e| e.to_str()).is_some_and(|e| UPLOAD_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        });
        if !self.logged_in {
            self.toast("Sign in to upload music");
        } else if ok.is_empty() {
            if !skipped.is_empty() {
                self.toast("YouTube Music only takes MP3, M4A, FLAC, OGG and WMA files");
            }
        } else {
            self.toast(if ok.len() == 1 { "Uploading 1 song…".to_owned() } else { format!("Uploading {} songs…", ok.len()) });
            self.backend.request(Req::Upload(ok));
        }
    }

    fn start_spotify_sign_in(&self, ctx: &egui::Context) {
        let xfer = self.transfer.clone();
        if xfer.signing_in.swap(true, Ordering::Relaxed) {
            return;
        }
        let (backend, ctx) = (self.backend.clone(), ctx.clone());
        crate::login::spawn(crate::login::Site::Spotify, xfer.spotify_profile(), move |outcome| {
            match outcome {
                crate::login::Outcome::Cookie(c) => xfer.set_cookie(Some(c), &backend),
                crate::login::Outcome::Cancelled => {}
                crate::login::Outcome::Failed(e) => backend.report_error(format!("Spotify sign-in window failed: {e}")),
            }
            xfer.signing_in.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    fn start_sign_in(&self, ctx: &egui::Context) {
        if self.signing_in.swap(true, Ordering::Relaxed) {
            return;
        }
        let (backend, busy, ctx) = (self.backend.clone(), self.signing_in.clone(), ctx.clone());
        crate::login::spawn(crate::login::Site::Google, self.backend.login_profile(), move |outcome| {
            match outcome {
                crate::login::Outcome::Cookie(c) => backend.request(Req::SetCookie(c)),
                crate::login::Outcome::Cancelled => {}
                crate::login::Outcome::Failed(e) => backend.report_error(format!("Sign-in window failed: {e}")),
            }
            busy.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }
}

fn share_url(target: &Target) -> String {
    match target {
        Target::Song(t) => format!("https://music.youtube.com/watch?v={}", t.id),
        Target::Playlist(id) => format!("https://music.youtube.com/playlist?list={id}"),
        Target::Album(id) => format!("https://music.youtube.com/browse/{id}"),
        Target::Artist(id) => format!("https://music.youtube.com/channel/{id}"),
    }
}

impl App {
    fn draw_dialog(&mut self, ctx: &egui::Context) {
        if let Some(Dialog::ConfirmDelete { id, title }) = &self.dialog {
            let (id, title) = (id.clone(), title.clone());
            match confirm_delete(ctx, &title, self.accent_color()) {
                Some(true) => {
                    self.backend.request(Req::DeletePlaylist(id));
                    self.dialog = None;
                }
                Some(false) => self.dialog = None,
                None => {}
            }
            return;
        }
        let Some(mut dialog) = self.dialog.take() else { return };
        let accent = self.accent_color();
        let mut done = false;
        let heading = match &dialog {
            Dialog::PickPlaylist(_) => "Add to playlist",
            Dialog::NewPlaylist { .. } => "New playlist",
            Dialog::EditPlaylist { .. } => "Edit playlist",
            Dialog::ConfirmDelete { .. } => "Delete playlist",
        };
        // The same modal card as "Delete playlist?" and the update dialog.
        let modal = egui::Modal::new("playlist-dialog".into()).frame(notes::surface()).backdrop_color(theme::shade(0.55)).show(ctx, |ui| {
            ui.set_width(440.0_f32.min(ctx.content_rect().width() - 48.0));
            egui::Frame::new().inner_margin(egui::Margin { left: 26, right: 26, top: 24, bottom: 22 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                ui.label(egui::RichText::new(heading).font(theme::bold(22.0)).color(theme::TEXT));
                ui.add_space(18.0);
                match &mut dialog {
                    Dialog::PickPlaylist(track) => {
                        if dialog_row(ui, None, Some(theme::icon::LIST_PLUS), "New playlist") {
                            self.actions.push(Action::NewPlaylist(Some(track.clone())));
                            done = true;
                        }
                        ui.add_space(6.0);
                        pages::divider(ui);
                        ui.add_space(6.0);
                        let cards = self.library.as_ref().map(|l| l.playlists.clone()).unwrap_or_default();
                        egui::ScrollArea::vertical().max_height(340.0).auto_shrink([false, true]).show(ui, |ui| {
                            for card in cards {
                                if let Target::Playlist(id) = card.target {
                                    if !id.starts_with("PL") { continue; }
                                    if dialog_row(ui, Some(&card.thumbs), None, &card.title) {
                                        self.backend.request(Req::AddToPlaylist { playlist_id: id, video_id: track.id.clone() });
                                        done = true;
                                    }
                                }
                            }
                        });
                    }
                    Dialog::NewPlaylist { title, track } => {
                        let name = text_field(ui, "Name", title, false, "My playlist");
                        if ui.memory(|m| m.focused().is_none()) { name.request_focus(); }
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("Private · only you can see it").font(theme::regular(12.5)).color(theme::TEXT_FAINT));
                        ui.add_space(24.0);
                        let enter = name.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let (ok, cancel) = dialog_buttons(ui, "Create", !title.trim().is_empty(), accent);
                        if (ok || enter) && !title.trim().is_empty() {
                            self.backend.request(Req::CreatePlaylist { title: title.trim().to_owned(), video_id: track.as_ref().map(|t| t.id.clone()) });
                            done = true;
                        }
                        done |= cancel;
                    }
                    Dialog::EditPlaylist { id, title, description, privacy } => {
                        text_field(ui, "Name", title, false, "Playlist name");
                        ui.add_space(16.0);
                        text_field(ui, "Description", description, true, "Add an optional description");
                        ui.add_space(16.0);
                        field_label(ui, "Visibility");
                        privacy_picker(ui, privacy);
                        ui.add_space(26.0);
                        let (ok, cancel) = dialog_buttons(ui, "Save", !title.trim().is_empty(), accent);
                        if ok {
                            self.backend.request(Req::EditPlaylist { id: id.clone(), title: title.trim().to_owned(), description: description.clone(), privacy: privacy.clone() });
                            done = true;
                        }
                        done |= cancel;
                    }
                    Dialog::ConfirmDelete { .. } => unreachable!("drawn by confirm_delete"),
                }
            });
        });
        let dismissed = modal.should_close() || ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if !done && !dismissed { self.dialog = Some(dialog); }
    }
}

fn field_label(ui: &mut Ui, label: &str) {
    ui.label(egui::RichText::new(label).font(theme::semibold(13.0)).color(theme::text_dim()));
    ui.add_space(8.0);
}

/// A labelled text box on a glass fill, like the jam and transfer pages' inputs.
fn text_field(ui: &mut Ui, label: &str, text: &mut String, multiline: bool, hint: &str) -> egui::Response {
    use egui::{Sense, vec2};
    field_label(ui, label);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), if multiline { 96.0 } else { 42.0 }), Sense::hover());
    // Painted first, so the text box draws over it.
    let bg = ui.painter().add(egui::Shape::Noop);
    let inner = rect.shrink2(vec2(14.0, if multiline { 10.0 } else { 4.0 }));
    let edit = if multiline { egui::TextEdit::multiline(text).desired_rows(4) } else { egui::TextEdit::singleline(text).vertical_align(egui::Align::Center) };
    let resp = ui.put(
        inner,
        edit.hint_text(egui::RichText::new(hint).color(theme::TEXT_FAINT)).font(theme::regular(14.0)).frame(egui::Frame::NONE).desired_width(inner.width()),
    );
    let focus = theme::anim_bool(ui.ctx(), resp.id.with("focus"), resp.has_focus(), 0.15);
    ui.painter().set(bg, egui::Shape::Vec(vec![
        egui::Shape::rect_filled(rect, 10.0, theme::glass(0.08 + 0.03 * focus)),
        egui::Shape::rect_stroke(rect, 10.0, egui::Stroke::new(1.0, theme::glass(0.06 + 0.18 * focus)), egui::StrokeKind::Inside),
    ]));
    resp
}

/// Private / Unlisted / Public as one segmented control; nothing picked keeps the current setting.
fn privacy_picker(ui: &mut Ui, privacy: &mut Option<String>) {
    use egui::{Sense, vec2};
    let (track, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    ui.painter().rect_filled(track, 10.0, theme::glass(0.08));
    let options = [("PRIVATE", "Private"), ("UNLISTED", "Unlisted"), ("PUBLIC", "Public")];
    let w = (track.width() - 6.0) / options.len() as f32;
    for (i, (value, label)) in options.into_iter().enumerate() {
        let r = egui::Rect::from_min_size(track.min + vec2(3.0 + w * i as f32, 3.0), vec2(w, track.height() - 6.0));
        let resp = widgets::named(ui.interact(r, ui.id().with(("privacy", value)), Sense::click()), label).on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = privacy.as_deref() == Some(value);
        let s = theme::anim_bool(ui.ctx(), resp.id.with("s"), on, 0.15);
        let h = theme::anim_bool(ui.ctx(), resp.id.with("h"), resp.hovered(), 0.12);
        let fill = theme::lerp_color(theme::glass(0.06 * h), Color32::from_gray(245), s);
        if fill.a() > 0 { ui.painter().rect_filled(r, 8.0, fill); }
        let color = theme::lerp_color(theme::with_alpha(theme::TEXT, 0.7 + 0.3 * h), Color32::from_gray(15), s);
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, label, theme::semibold(13.0), color);
        if resp.clicked() {
            *privacy = if on { None } else { Some(value.to_owned()) };
        }
    }
    if privacy.is_none() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Unchanged. Pick one to change who can see it.").font(theme::regular(12.5)).color(theme::TEXT_FAINT));
    }
}

/// Cancel and a primary button, right-aligned: (primary clicked, cancel clicked).
fn dialog_buttons(ui: &mut Ui, primary: &str, enabled: bool, accent: Color32) -> (bool, bool) {
    use egui::{Sense, vec2};
    let (mut ok, mut cancel) = (false, false);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let fg = if accent.r() as u32 + accent.g() as u32 + accent.b() as u32 > 480 { Color32::from_gray(15) } else { Color32::WHITE };
        let g = ui.painter().layout_no_wrap(primary.to_owned(), theme::semibold(14.0), fg);
        let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 40.0, 38.0), if enabled { Sense::click() } else { Sense::hover() });
        let resp = widgets::named(resp, primary);
        let resp = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
        let h = theme::anim_bool(ui.ctx(), resp.id.with("h"), resp.hovered() && enabled, 0.15);
        let fill = theme::lerp_color(accent, Color32::WHITE, 0.12 * h);
        ui.painter().rect_filled(rect, 19.0, theme::with_alpha(fill, if enabled { 1.0 } else { 0.35 }));
        ui.painter().galley_with_override_text_color(rect.center() - g.size() / 2.0, g, theme::with_alpha(fg, if enabled { 1.0 } else { 0.6 }));
        ok = resp.clicked();
        cancel = widgets::pill(ui, "Cancel", None, false).clicked();
    });
    (ok, cancel)
}

/// A playlist (or "New playlist") row in the Add to playlist dialog.
fn dialog_row(ui: &mut Ui, thumbs: Option<&[crate::model::Thumb]>, glyph: Option<char>, label: &str) -> bool {
    use egui::{Sense, pos2, vec2};
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::click());
    let resp = widgets::named(resp, label).on_hover_cursor(egui::CursorIcon::PointingHand);
    let h = theme::anim_bool(ui.ctx(), resp.id.with("h"), resp.hovered(), 0.12);
    if h > 0.0 { ui.painter().rect_filled(rect, 10.0, theme::glass(0.08 * h)); }
    let art = egui::Rect::from_center_size(pos2(rect.left() + 28.0, rect.center().y), vec2(40.0, 40.0));
    match (thumbs, glyph) {
        (Some(t), _) => widgets::paint_cover(ui, art, t, 6.0, 1.0),
        (None, Some(g)) => {
            ui.painter().rect_filled(art, 6.0, theme::glass(0.1));
            widgets::icon_at(ui, art.center(), g, 18.0, theme::TEXT);
        }
        _ => {}
    }
    let x = art.right() + 14.0;
    widgets::text_at(ui, pos2(x, rect.center().y - 9.0), label, theme::semibold(14.0), theme::TEXT, rect.right() - x - 10.0);
    resp.clicked()
}

/// "Delete playlist?" as a modal card: Some(true) delete, Some(false) cancel, None still open.
fn confirm_delete(ctx: &egui::Context, title: &str, _accent: Color32) -> Option<bool> {
    let mut answer = None;
    let modal = egui::Modal::new("confirm-delete".into()).frame(notes::surface()).backdrop_color(theme::shade(0.55)).show(ctx, |ui| {
        ui.set_width(400.0_f32.min(ctx.content_rect().width() - 48.0));
        egui::Frame::new().inner_margin(egui::Margin { left: 26, right: 26, top: 24, bottom: 22 }).show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            ui.label(egui::RichText::new("Delete playlist?").font(theme::bold(22.0)).color(theme::TEXT));
            ui.add_space(10.0);
            let body = format!("\u{201c}{title}\u{201d} will be removed from your YouTube Music library. This can't be undone.");
            ui.add(egui::Label::new(egui::RichText::new(body).font(theme::regular(14.0)).color(theme::text_dim())).wrap());
            ui.add_space(22.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // Destructive action in red, white text.
                let label = "Delete";
                let g = ui.painter().layout_no_wrap(label.to_owned(), theme::semibold(14.0), Color32::WHITE);
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(g.size().x + 40.0, 38.0), egui::Sense::click());
                let resp = widgets::named(resp, "Delete playlist").on_hover_cursor(egui::CursorIcon::PointingHand);
                let h = theme::anim_bool(ui.ctx(), resp.id.with("h"), resp.hovered(), 0.15);
                ui.painter().rect_filled(rect, 19.0, theme::lerp_color(Color32::from_rgb(214, 40, 57), Color32::from_rgb(236, 56, 72), h));
                ui.painter().galley(rect.center() - g.size() / 2.0, g, Color32::WHITE);
                if resp.clicked() {
                    answer = Some(true);
                }
                if widgets::pill(ui, "Cancel", None, false).clicked() {
                    answer = Some(false);
                }
            });
        });
    });
    if answer.is_none() && (modal.should_close() || ctx.input(|i| i.key_pressed(egui::Key::Escape))) {
        answer = Some(false);
    }
    answer
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        dev_pace();
        self.lyric_view.release_hidden(ctx.cumulative_pass_nr());
        let now = ctx.input(|i| i.time);
        self.sync_look();
        self.pump(now);
        // System media controls and taskbar buttons follow the player however it changed.
        let playing = self.playing() || self.buffering;
        if let Some(m) = &mut self.media {
            m.set_playing(playing);
        }
        self.request_lyric_extras();
        // Upload: files from the picker, or dropped onto the window.
        let mut files = self.picked_uploads.lock().unwrap().take().unwrap_or_default();
        files.extend(ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_owned()).filter(|p| !p.as_os_str().is_empty()).collect::<Vec<_>>()));
        if !files.is_empty() {
            self.upload_files(files);
        }
        transfer::poll(self, ctx, now);
        for code in crate::deeplink::take() {
            self.show_window(ctx);
            jam::open_link(self, code);
        }
        for a in std::mem::take(&mut self.actions) {
            self.apply(a);
        }
        jam::tick(self);
        jam::dev_script(self, ctx, now);
        if self.page_changed < 0.0 {
            self.page_changed = now;
        }
        // Dev hook: YTM_NP_TOGGLE_AT=<secs>[,<secs>…] clicks the Lyrics chip at those times.
        if let Some(v) = dev_var("YTM_NP_TOGGLE_AT") {
            let times: Vec<f64> = v.split(',').filter_map(|t| t.parse().ok()).collect();
            let id = egui::Id::new("dev-np-toggles");
            let done: usize = ctx.data(|d| d.get_temp(id)).unwrap_or(0);
            if let Some(&t) = times.get(done) {
                if now >= t {
                    ctx.data_mut(|d| d.insert_temp(id, done + 1));
                    self.np_tab = if self.np_tab == NpTab::Lyrics { NpTab::Art } else { NpTab::Lyrics };
                } else {
                    ctx.request_repaint_after(Duration::from_secs_f64(t - now));
                }
            }
        }
        fit_window_to_monitor(ctx);
        self.tray_window(ctx);
        self.window_hidden = self.tray.is_some() || ctx.input(|i| i.viewport().minimized == Some(true));
        self.sync_discord();
        self.tick_updates(ctx, now);
        let np = widgets::NowPlaying {
            song: self.current_id().map(str::to_owned),
            source: self.queue_source.clone(),
            playing: self.playing() || self.buffering,
        };
        ctx.data_mut(|d| d.insert_temp(widgets::NowPlaying::id(), np));
        if let Some((_, t)) = &mut self.toast {
            if *t < 0.0 {
                *t = now;
            } else if now - *t > 4.5 {
                self.toast = None;
            }
        }
        // Frame pacing: idle costs nothing. The background drifts slowly, so ~15 fps
        // reads as smooth on pages; the now-playing view runs at 30 fps, and the synced
        // lyrics ask for every frame while they animate (the karaoke sweep must be
        // smooth). Each egui frame re-lays-out the UI, so frame rate is the main CPU cost.
        let playing = self.playing();
        if self.tray.is_some() {
            // Hidden: nothing to draw, only the player bookkeeping.
            if playing || self.buffering {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        } else if self.np_open && playing {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if playing && self.anim_bg && !self.default_theme && shell::pages_bg_moves(self.look()) {
            ctx.request_repaint_after(Duration::from_millis(66));
        } else if playing || self.buffering {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        let now = ctx.input(|i| i.time);
        let screen = ctx.content_rect();

        self.sync_look();
        let look = self.look();
        let accent = self.accent_color();
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("accent"), accent));
        if self.chrome_applied.as_deref() != Some(look.id.as_str()) {
            if self.chrome_applied.is_none() {
                ctx.add_texture_loader(Arc::new(crate::images::Textures::default()));
            }
            theme::apply_chrome(&ctx, look);
            self.chrome_applied = Some(look.id.clone());
        }
        let app_style = &look.app;
        // Once now playing has slid all the way up it hides everything but the player
        // bar, so the pages and the full-screen background under it aren't drawn.
        let covered = !self.fullscreen && theme::anim_bool(&ctx, "np-open", self.np_open && self.current().is_some(), 0.42) >= 1.0;
        let bar_strip = egui::Rect::from_min_max(egui::pos2(screen.left(), screen.bottom() - self.bar_height()), screen.max);
        shell::background(self, ui, screen, now, covered.then_some(bar_strip));
        if !self.fullscreen {
            let floating = look.chrome.player_bar == crate::preset::PlayerBar::Floating;
            let bar_alpha = app_style.bar_alpha.clamp(0.0, 1.0);
            let frame = if floating {
                egui::Frame::new().inner_margin(egui::Margin { left: 14, right: 14, top: 0, bottom: 14 })
            } else {
                let mut f = egui::Frame::new().fill(theme::shade(bar_alpha));
                if app_style.bar_border {
                    f = f.stroke(egui::Stroke::new(1.0, theme::glass(0.1)));
                }
                f
            };
            egui::Panel::bottom("player").resizable(false)
                .exact_size(self.bar_height())
                .frame(frame)
                .show_separator_line(false)
                .show(ui, |ui| {
                    if floating {
                        let r = ui.max_rect();
                        let radius = look.chrome.radius.clamp(10.0, 26.0);
                        widgets::soft_shadow(ui, r, radius, 24.0, 0.45);
                        ui.painter().rect_filled(r, radius, theme::shade((bar_alpha + 0.07).min(1.0)));
                        let edge = if app_style.bar_border { 0.14 } else { 0.08 };
                        ui.painter().rect_stroke(r, radius, egui::Stroke::new(1.0, theme::glass(edge)), egui::StrokeKind::Inside);
                        ui.set_clip_rect(r);
                    }
                    shell::player_bar(self, ui, now)
                });
        }
        if !self.fullscreen && !covered {
            use crate::preset::SidebarStyle;
            let side_alpha = app_style.sidebar_alpha.clamp(0.0, 1.0);
            let side_frame = match app_style.sidebar {
                SidebarStyle::Glass => egui::Frame::new().fill(theme::shade(side_alpha)).inner_margin(egui::Margin::symmetric(14, 16)),
                SidebarStyle::Transparent => egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 16)),
                SidebarStyle::Solid => egui::Frame::new().fill(look.solid()).inner_margin(egui::Margin::symmetric(14, 16)),
                SidebarStyle::Floating => egui::Frame::new()
                    .fill(theme::shade(side_alpha.max(0.3)))
                    .stroke(egui::Stroke::new(1.0, theme::glass(0.08)))
                    .corner_radius(egui::CornerRadius::same(theme::panel_radius().max(12)))
                    .outer_margin(egui::Margin { left: 10, right: 4, top: 10, bottom: 10 })
                    .inner_margin(egui::Margin::symmetric(12, 14)),
            };
            egui::Panel::left("nav").resizable(false)
                .exact_size(shell::SIDEBAR_W - (shell::SIDEBAR_W - shell::RAIL_W) * shell::sidebar_collapse(self, &ctx))
                .frame(side_frame)
                .show_separator_line(false)
                .show(ui, |ui| shell::sidebar(self, ui));
            let q = theme::anim_bool(&ctx, "queue-drawer", self.queue_open && !self.np_open, 0.25);
            if q > 0.01 {
                egui::Panel::right("queue").resizable(false)
                    .exact_size(340.0 * q)
                    .frame(egui::Frame::new().fill(theme::shade(0.42)).inner_margin(egui::Margin::symmetric(14, 16)))
                    .show_separator_line(false)
                    .show(ui, |ui| shell::queue_drawer(self, ui));
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().inner_margin(egui::Margin { left: 28, right: 0, top: 14, bottom: 0 }))
                .show(ui, |ui| {
                    shell::top_bar(self, ui);
                    ui.add_space(6.0);
                    pages::page(self, ui, now);
                });
        }
        nowplaying::overlay(self, &ctx, now);
        self.draw_dialog(&ctx);
        notes::card(self, &ctx);
        notes::dialog(self, &ctx);
        if let Some(url) = self.share_pending.take() { ctx.copy_text(url); }
        shell::suggestions(self, &ctx);
        shell::toast(self, &ctx, now);

        if dev_var("YTM_THEME_TOUR").is_some() {
            self.theme_tour(&ctx, now);
        } else {
            dev_screenshot(&ctx, now);
        }
    }

    /// Dev hook: `YTM_FAKE_POINTER=x,y` parks a synthetic pointer there, for testing
    /// hover behaviour without touching the real mouse.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Dev hook: YTM_FAKE_WHEEL="secs,notches" injects one wheel event, for testing.
        if let Some((at, n)) = dev_var("YTM_FAKE_WHEEL").and_then(|v| {
            let (a, n) = v.split_once(',')?;
            Some((a.parse::<f64>().ok()?, n.parse::<f32>().ok()?))
        }) {
            let fired = egui::Id::new("dev-wheel-fired");
            let t = raw.time.unwrap_or(0.0);
            if t >= at && !ctx.data(|d| d.get_temp::<bool>(fired).unwrap_or(false)) {
                ctx.data_mut(|d| d.insert_temp(fired, true));
                raw.events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Line,
                    delta: egui::vec2(0.0, -n),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                });
            } else if t < at {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        smooth_wheel(self, ctx, raw);
        if let Some((x, y)) = dev_var("YTM_FAKE_POINTER").and_then(|v| {
            let (x, y) = v.split_once(',')?;
            Some((x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?))
        }) {
            raw.events.retain(|e| !matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerGone));
            raw.events.push(egui::Event::PointerMoved(egui::pos2(x, y)));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if std::env::var_os("YTM_MUTE").is_none() {
            storage.set_string("volume", self.audio.volume().to_string());
        }
        let repeat = match self.repeat {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        };
        storage.set_string("repeat", repeat.into());
        let b = |v: bool| if v { "1" } else { "0" }.to_owned();
        storage.set_string("autoplay", b(self.autoplay));
        storage.set_string("jam_name", self.jam.name.clone());
        let cache = self.logged_in.then(|| AccountCache {
            name: self.account_name.clone(),
            avatar: self.account_avatar.clone(),
            playlists: self.library.as_ref().map(|l| l.playlists.clone()).or_else(|| self.sidebar_cache.clone()).unwrap_or_default(),
        });
        storage.set_string("account_cache", cache.and_then(|c| serde_json::to_string(&c).ok()).unwrap_or_default());
        storage.set_string("normalize", b(self.normalize));
        storage.set_string("high_quality", b(self.high_quality));
        storage.set_string("auto_update", b(self.auto_update));
        // eframe has just saved the window's geometry. Minimized or in the tray that's
        // 0×0 at -32000, which would reopen as a speck: keep the last real one instead.
        if self.window_hidden {
            if let Some(w) = &self.good_window {
                storage.set_string("window", w.clone());
            }
        } else {
            self.good_window = storage.get_string("window");
        }
        storage.set_string("close_to_tray", b(self.close_to_tray));
        storage.set_string("sidebar_collapsed", b(self.sidebar_collapsed));
        // Pretend versions (dev hook) mustn't decide the real "what's new".
        if std::env::var_os("YTM_UPDATE_AS").is_none() {
            storage.set_string("last_version", crate::update::current_version());
        }
        storage.set_string("discord_enabled", b(self.discord_enabled));
        storage.set_string("default_theme", b(self.default_theme));
        if std::env::var_os("YTM_THEME").is_none() {
            storage.set_string("theme", self.theme_id.clone());
        }
        storage.set_string("anim_bg", b(self.anim_bg));
        storage.set_string("reactive_bg", b(self.reactive_bg));
        storage.set_string("show_roman", b(self.show_roman));
        storage.set_string("show_trans", b(self.show_trans));
        storage.set_string("lyrics_karaoke", b(self.karaoke));
        // Background test runs mustn't change the listener's sources or per-song picks.
        if std::env::var_os("YTM_BACKGROUND").is_none() || std::env::var_os("YTM_SAVE_SESSION").is_some() {
            self.lyric_prefs.save(storage);
        }
        storage.set_string("np_art", b(self.np_tab == NpTab::Art));
        // Background test runs leave the user's real "last song" alone unless asked.
        let test_run = std::env::var_os("YTM_BACKGROUND").is_some() && std::env::var_os("YTM_SAVE_SESSION").is_none();
        if let Some(s) = self.session_json().filter(|_| !test_run) {
            storage.set_string(SESSION_KEY, s);
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let (Some(bg), Some(gl)) = (&self.bg, gl) {
            bg.destroy(gl);
        }
        // A downloaded update installs as the app closes (and relaunches it when the
        // user asked to restart).
        if let Some(u) = &self.updater
            && (self.restart_for_update || self.auto_update)
        {
            u.install(self.restart_for_update);
        }
    }
}

const SESSION_KEY: &str = "session";

/// What the sidebar and the account button show, kept for the next launch.
#[derive(serde::Serialize, serde::Deserialize)]
struct AccountCache {
    name: Option<String>,
    avatar: Option<String>,
    playlists: Vec<Card>,
}

/// What's saved between launches so the player picks up where it left off.
#[derive(serde::Serialize, serde::Deserialize)]
struct Session {
    queue: Vec<Track>,
    index: usize,
    position_ms: u64,
    duration_ms: u64,
}

/// Dev hook: `YTM_SCREENSHOT=out.png` saves the app's own framebuffer after
/// `YTM_SCREENSHOT_AT` seconds (default 8), independent of OS window capture.
/// `YTM_SCREENSHOT_BURST=<n>` instead keeps the next n frames from `YTM_SCREENSHOT_AT`
/// (in memory, so saving doesn't slow them) and then writes `<stem>_<i>_<ms>.png`,
/// `ms` being each frame's time: a filmstrip of an animation and its frame pacing.
fn dev_screenshot(ctx: &egui::Context, now: f64) {
    let Some(path) = dev_var("YTM_SCREENSHOT") else { return };
    let at: f64 = dev_var("YTM_SCREENSHOT_AT").and_then(|v| v.parse().ok()).unwrap_or(8.0);
    if let Some(n) = dev_var("YTM_SCREENSHOT_BURST").and_then(|v| v.parse::<usize>().ok()) {
        static FRAMES: std::sync::Mutex<Vec<(f64, Arc<egui::ColorImage>)>> = std::sync::Mutex::new(Vec::new());
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.load(Ordering::Relaxed) {
            return;
        }
        let mut frames = FRAMES.lock().unwrap();
        if let Some(img) = ctx.input(|i| i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })) {
            frames.push((now, img));
        }
        if now >= at && frames.len() < n {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            ctx.request_repaint();
        } else if now < at {
            ctx.request_repaint_after(Duration::from_secs_f64(at - now));
        } else if frames.len() == n {
            let path = std::path::PathBuf::from(path);
            let shots = std::mem::take(&mut *frames);
            DONE.store(true, Ordering::Relaxed);
            std::thread::spawn(move || {
                let t0 = shots[0].0;
                for (i, (t, img)) in shots.iter().enumerate() {
                    let [w, h] = img.size;
                    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                    let out = path.with_file_name(format!("{}_{i:03}_{:04}.png", path.file_stem().unwrap_or_default().to_string_lossy(), ((t - t0) * 1000.0) as u32));
                    if let Some(img) = image::RgbaImage::from_raw(w as u32, h as u32, bytes) {
                        let _ = img.save(out);
                    }
                }
                log::info!("screenshot saved ({} frames)", shots.len());
            });
        }
        return;
    }
    let requested = egui::Id::new("dev-shot-requested");
    if now >= at && !ctx.data(|d| d.get_temp::<bool>(requested).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(requested, true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
    } else if now < at {
        ctx.request_repaint_after(Duration::from_secs_f64(at - now));
    }
    let shot = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    if let Some(img) = shot {
        let [w, h] = img.size;
        let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        match image::RgbaImage::from_raw(w as u32, h as u32, bytes).map(|i| i.save(&path)) {
            Some(Ok(())) => log::info!("screenshot saved"),
            other => log::warn!("screenshot failed: {other:?}"),
        }
    }
}

impl App {
    /// Dev hook: `YTM_THEME_TOUR=<secs>` steps through every theme preset from
    /// `YTM_SCREENSHOT_AT`, saving `<YTM_SCREENSHOT stem>_<id>.png` for each, then exits.
    fn theme_tour(&mut self, ctx: &egui::Context, now: f64) {
        let Some(path) = dev_var("YTM_SCREENSHOT").map(std::path::PathBuf::from) else { return };
        let at: f64 = dev_var("YTM_SCREENSHOT_AT").and_then(|v| v.parse().ok()).unwrap_or(8.0);
        let step: f64 = dev_var("YTM_THEME_TOUR").and_then(|v| v.parse().ok()).unwrap_or(3.0);
        // YTM_TOUR_ONLY=id,id limits the tour to those presets.
        let only = dev_var("YTM_TOUR_ONLY").unwrap_or_default();
        let all: Vec<&crate::preset::Preset> = crate::preset::all().iter().filter(|p| only.is_empty() || only.split(',').any(|o| o == p.id)).collect();
        ctx.request_repaint_after(Duration::from_millis(100));
        if now < at {
            return;
        }
        let n = ((now - at) / step) as usize;
        if n >= all.len() {
            // Let the last screenshot land, then quit.
            if now - at > step * all.len() as f64 + 1.5 {
                self.quit(ctx);
            }
        } else if self.theme_id != all[n].id {
            self.theme_id = all[n].id.clone();
            self.default_theme = false;
        }
        let shot_id = egui::Id::new("tour-shot");
        let requested: usize = ctx.data(|d| d.get_temp(shot_id)).unwrap_or(usize::MAX);
        let in_slot = (now - at) - n as f64 * step;
        if n < all.len() && requested != n && in_slot > step * 0.8 {
            ctx.data_mut(|d| d.insert_temp(shot_id, n));
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(n)));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, user_data, .. } => Some((image.clone(), user_data.data.as_ref().and_then(|d| d.downcast_ref::<usize>().copied()))),
                _ => None,
            })
        });
        if let Some((img, Some(i))) = shot {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let out = path.with_file_name(format!("{stem}_{:02}_{}.png", i, all[i].id));
            match image::RgbaImage::from_raw(w as u32, h as u32, bytes).map(|im| im.save(&out)) {
                Some(Ok(())) => log::info!("tour screenshot saved: {}", all[i].id),
                other => log::warn!("tour screenshot failed: {other:?}"),
            }
        }
    }
}

/// A restored window size can exceed the monitor (e.g. saved after maximizing),
/// leaving the player bar partly off-screen. Once at startup, maximize instead.
fn fit_window_to_monitor(ctx: &egui::Context) {
    let done = egui::Id::new("fit-window-checked");
    if ctx.data(|d| d.get_temp::<bool>(done).unwrap_or(false)) {
        return;
    }
    if std::env::var_os("YTM_BACKGROUND").is_some() {
        // eframe re-applies the saved (often maximized) window state regardless of the
        // builder, so push the test window off-screen here, before it's ever shown.
        ctx.data_mut(|d| d.insert_temp(done, true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(-4000.0, 0.0)));
        // YTM_SIZE=WxH tests other window sizes (e.g. the narrow layout).
        let size = std::env::var("YTM_SIZE").ok().and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some(egui::vec2(w.parse().ok()?, h.parse().ok()?))
        });
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.unwrap_or(egui::vec2(1920.0, 1057.0))));
        return;
    }
    let (outer, monitor, maximized) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().monitor_size, i.viewport().maximized));
    let (Some(outer), Some(monitor)) = (outer, monitor) else { return };
    ctx.data_mut(|d| d.insert_temp(done, true));
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, monitor);
    // A saved size below the minimum (closed while minimized saves a tiny one, which
    // eframe restores as-is) would open a window too small to see: start over centred.
    let min = egui::Vec2::from(crate::MIN_SIZE);
    if maximized != Some(true) && (outer.width() < min.x || outer.height() < min.y) {
        let size = egui::Vec2::from(crate::DEFAULT_SIZE).min(monitor * 0.9).max(min);
        log::info!("window {outer:?} is below the minimum size; resetting to {size:?}");
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(((monitor - size) / 2.0).max(egui::Vec2::ZERO).to_pos2()));
        return;
    }
    // A little slack for the invisible resize borders Windows adds around the frame.
    if maximized != Some(true) && !screen.expand(12.0).contains_rect(outer) {
        log::info!("window {outer:?} exceeds monitor {monitor:?}; maximizing");
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }
}

/// Smooth scrolling: mouse-wheel notches (line units) are collected and replayed as
/// small pixel steps with an exponential ease-out (~0.3 s glide), like browsers do.
/// Touchpads already report smooth pixel deltas and pass through untouched, as does
/// Ctrl+wheel zoom.
fn smooth_wheel(app: &mut App, ctx: &egui::Context, raw: &mut egui::RawInput) {
    const NOTCH_PX: f32 = 70.0;
    const GLIDE: f32 = 0.06; // time constant, seconds (settles in ~0.18 s)
    let viewport_h = raw.screen_rect.map_or(800.0, |r| r.height());
    raw.events.retain(|e| match e {
        egui::Event::MouseWheel { unit, delta, modifiers, .. } if !modifiers.command && !modifiers.ctrl => {
            let px = match unit {
                egui::MouseWheelUnit::Line => *delta * NOTCH_PX,
                egui::MouseWheelUnit::Page => *delta * viewport_h * 0.8,
                egui::MouseWheelUnit::Point => return true,
            };
            // Reversing direction cancels the remaining glide instead of fighting it.
            if app.wheel_pending.y * px.y < 0.0 || app.wheel_pending.x * px.x < 0.0 {
                app.wheel_pending = egui::Vec2::ZERO;
            }
            app.wheel_pending += px;
            app.wheel_mods = *modifiers;
            false
        }
        _ => true,
    });
    if app.wheel_pending.length() < 0.5 {
        app.wheel_pending = egui::Vec2::ZERO;
        return;
    }
    let dt = raw.predicted_dt.clamp(0.001, 0.05);
    let step = app.wheel_pending * (1.0 - (-dt / GLIDE).exp());
    app.wheel_pending -= step;
    raw.events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: step,
        phase: egui::TouchPhase::Move,
        modifiers: app.wheel_mods,
    });
    ctx.request_repaint();
}

/// Fisher-Yates with a clock-seeded xorshift; shuffling doesn't need a real RNG crate.
pub fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

/// Whether this is the installed app (updates only apply there). It can't change while
/// running, and the check touches the disk, so it runs once.
fn installed() -> bool {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(crate::update::installed)
}

/// A `YTM_*` dev hook, read once: several are checked every frame.
fn dev_var(name: &str) -> Option<&'static str> {
    static VARS: std::sync::OnceLock<std::collections::HashMap<String, String>> = std::sync::OnceLock::new();
    VARS.get_or_init(|| {
        std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).filter(|(k, _)| k.starts_with("YTM_")).collect()
    })
    .get(name)
    .map(String::as_str)
}

pub fn fmt_time(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod share_tests {
    use super::*;

    #[test]
    fn shares_youtube_music_urls() {
        let song = Track { id: "abc123".into(), title: String::new(), artists: vec![], album: None, duration: None, thumbs: vec![], track_nr: None, plays: None };
        assert_eq!(share_url(&Target::Song(song)), "https://music.youtube.com/watch?v=abc123");
        assert_eq!(share_url(&Target::Playlist("PL123".into())), "https://music.youtube.com/playlist?list=PL123");
        assert_eq!(share_url(&Target::Album("MPRE123".into())), "https://music.youtube.com/browse/MPRE123");
        assert_eq!(share_url(&Target::Artist("UC123".into())), "https://music.youtube.com/channel/UC123");
    }
}

#[cfg(test)]
mod account_block_tests {
    use super::account_block;

    #[test]
    fn account_errors_stop_instead_of_skipping() {
        assert!(account_block("sign in to play this song", false).is_some());
        assert!(account_block("sign in to play this song", true).is_none());
        assert!(account_block("This video is only available to Music Premium members", true).is_some());
        assert!(account_block("decode: end of stream", false).is_none());
    }
}

/// Dev hook: YTM_FPS=<hz> holds each frame to 1/<hz> s, as vsync does on a visible window
/// of that refresh rate (test windows sit off-screen, where Windows gives them ~21 fps);
/// YTM_FPS_LOG logs the rate reached every 5 s.
fn dev_pace() {
    use std::sync::Mutex;
    use std::time::Instant;
    static HZ: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    static STATE: Mutex<Option<(Instant, Instant, u32)>> = Mutex::new(None);
    let Some(hz) = *HZ.get_or_init(|| std::env::var("YTM_FPS").ok().and_then(|v| v.parse().ok())) else { return };
    let period = std::time::Duration::from_secs_f64(1.0 / hz);
    let mut st = STATE.lock().unwrap();
    let now = Instant::now();
    let (last, window, frames) = st.get_or_insert((now, now, 0));
    let next = *last + period;
    if next > now {
        std::thread::sleep(next - now);
    }
    *last = Instant::now();
    *frames += 1;
    let span = last.duration_since(*window).as_secs_f64();
    if span >= 5.0 {
        if std::env::var_os("YTM_FPS_LOG").is_some() {
            log::info!("fps {:.1}", *frames as f64 / span);
        }
        *window = *last;
        *frames = 0;
    }
}
