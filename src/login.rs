//! "Sign in with Google" and "Sign in to Spotify" windows.
//!
//! Opens the service's normal sign-in page in the system webview (WebView2 on Windows,
//! which ships with the OS), waits until the flow lands on the app's site, then hands the
//! session cookies (HttpOnly ones included) to the app.
//!
//! The window runs in a child process (`ytm-rs --login <dir>`): tao and winit crash
//! when both pump windows in one process (STATUS_FATAL_USER_CALLBACK_EXCEPTION), and a
//! separate process also returns all webview memory to the OS once sign-in ends.

use std::path::PathBuf;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{PageLoadEvent, WebContext, WebViewBuilder};

/// Which sign-in.
#[derive(Clone, Copy)]
pub enum Site {
    /// youtube.com, for YouTube Music.
    Google,
    /// open.spotify.com, for the Spotify transfer.
    Spotify,
}

impl Site {
    fn flag(self) -> &'static str {
        match self {
            Site::Google => FLAG,
            Site::Spotify => SPOTIFY_FLAG,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Site::Google => "Sign in to YouTube Music",
            Site::Spotify => "Sign in to Spotify",
        }
    }

    fn start(self) -> &'static str {
        match self {
            Site::Google => "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fmusic.youtube.com%2F",
            Site::Spotify => "https://accounts.spotify.com/en/login?continue=https%3A%2F%2Fopen.spotify.com%2F",
        }
    }

    /// Where the flow lands once signed in.
    fn home(self) -> &'static str {
        match self {
            Site::Google => "https://music.youtube.com/",
            Site::Spotify => "https://open.spotify.com/",
        }
    }

    /// The cookie that only a signed-in session has.
    fn session_cookie(self) -> &'static str {
        match self {
            Site::Google => "SAPISID",
            Site::Spotify => "sp_dc",
        }
    }
}

pub enum Outcome {
    /// Google: the `Cookie` header value for youtube.com. Spotify: the `sp_dc` cookie's value.
    Cookie(String),
    Cancelled,
    Failed(String),
}

/// Command-line flags that make the binary run only a sign-in window.
pub const FLAG: &str = "--login";
pub const SPOTIFY_FLAG: &str = "--login-spotify";

/// Show the sign-in window in a child process; `done` is called exactly once.
pub fn spawn(site: Site, profile_dir: PathBuf, done: impl FnOnce(Outcome) + Send + 'static) {
    std::thread::Builder::new()
        .name("login".into())
        .spawn(move || {
            let res = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe)
                    .arg(site.flag())
                    .arg(&profile_dir)
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
            });
            done(match res {
                Err(e) => Outcome::Failed(format!("could not start sign-in window: {e}")),
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let line = text.lines().last().unwrap_or_default();
                    if let Some(c) = line.strip_prefix("cookie ") {
                        Outcome::Cookie(c.to_owned())
                    } else if let Some(e) = line.strip_prefix("failed ") {
                        Outcome::Failed(e.to_owned())
                    } else if line == "cancelled" {
                        Outcome::Cancelled
                    } else {
                        Outcome::Failed(format!("sign-in window exited unexpectedly ({})", out.status))
                    }
                }
            })
        })
        .expect("spawn login thread");
}

/// Entry point of the child process: run the window and report on stdout.
pub fn child_main(site: Site, profile_dir: PathBuf) {
    match run(site, profile_dir) {
        Ok(Outcome::Cookie(c)) => println!("cookie {c}"),
        Ok(Outcome::Cancelled) => println!("cancelled"),
        Ok(Outcome::Failed(e)) => println!("failed {e}"),
        Err(e) => println!("failed {e:#}"),
    }
}

/// Delete the webview profile so the next sign-in starts fresh (used on sign-out).
pub fn forget(profile_dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(profile_dir);
}

/// Puts the webview in `window`. On Linux wry only takes X11 window handles, so it goes
/// into the window's GTK box instead, which works on Wayland (WSLg) too.
pub(crate) fn attach<'a>(builder: WebViewBuilder<'a>, window: &'a tao::window::Window) -> wry::Result<wry::WebView> {
    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("tao windows have a GTK box"))
    }
    #[cfg(not(target_os = "linux"))]
    builder.build(window)
}

fn run(site: Site, profile_dir: PathBuf) -> anyhow::Result<Outcome> {
    let mut event_loop = EventLoopBuilder::<()>::with_user_event().build();

    let window = WindowBuilder::new()
        .with_title(site.title())
        .with_inner_size(tao::dpi::LogicalSize::new(480.0, 720.0))
        .build(&event_loop)?;

    let proxy = event_loop.create_proxy();
    let mut context = WebContext::new(Some(profile_dir));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(site.start())
        .with_on_page_load_handler(move |ev, url| {
            if matches!(ev, PageLoadEvent::Finished) && url.starts_with(site.home()) {
                let _ = proxy.send_event(());
            }
        });
    let webview = attach(webview, &window)?;

    let mut outcome = Outcome::Cancelled;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(()) => match webview.cookies_for_url(site.home()) {
                Ok(cookies) => {
                    // Landing there while signed out also fires this; only a signed-in
                    // session has the session cookie (SAPISID, sp_dc).
                    if let Some(c) = cookies.iter().find(|c| c.name() == site.session_cookie()) {
                        outcome = Outcome::Cookie(match site {
                            Site::Spotify => c.value().to_owned(),
                            Site::Google => cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; "),
                        });
                        *flow = ControlFlow::Exit;
                    }
                }
                Err(e) => {
                    outcome = Outcome::Failed(format!("could not read cookies: {e}"));
                    *flow = ControlFlow::Exit;
                }
            },
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *flow = ControlFlow::Exit,
            _ => {}
        }
    });
    drop(webview);
    Ok(outcome)
}
