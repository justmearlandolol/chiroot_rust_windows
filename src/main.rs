// Chiroot — Windows desktop application.
//
// Rust + Tauri v2. The whole application logic lives in this single file:
// it creates one native window that loads the existing Chiroot website,
// exposes the Supabase publishable configuration to that trusted origin,
// and swaps in a built-in connection error screen (with Retry) whenever the
// website cannot be reached.
//
// Architecture notes
// ------------------
// * The website is never modified: it is loaded as-is from https://chiroot.my.id.
// * The error screen and its artwork are compiled into the binary
//   (`include_bytes!`) and served by a private Tauri URI scheme, so the app
//   stays a single portable .exe with no external files.
// * Connection failures are detected from Rust (TCP reachability probe +
//   page-load monitoring) because WebView2 reports a failed navigation
//   exactly like a successful one (`NavigationCompleted` fires either way).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
  borrow::Cow,
  net::{TcpStream, ToSocketAddrs},
  sync::{Arc, Mutex},
  thread,
  time::{Duration, Instant},
};

use tauri::{
  http::{header::CONTENT_TYPE, Request, Response, StatusCode},
  webview::PageLoadEvent,
  AppHandle, Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_opener::OpenerExt;

/* ────────────────────────────── Supabase ─────────────────────────────────
 * Publishable (public) values only — never a service_role / secret key.
 * Row Level Security on Supabase stays in full control.
 * ──────────────────────────────────────────────────────────────────────── */
const SUPABASE_URL: &str = "https://astuagdezahmuuwjoghf.supabase.co";
const SUPABASE_PUBLISHABLE_KEY: &str = "sb_publishable_NXhR-MquEMwYDrCBPAOgAQ_MZ4BXQlT";

/* ──────────────────────────── Application ─────────────────────────────── */
const WINDOW_LABEL: &str = "main";
const APP_URL: &str = "https://chiroot.my.id/Main.html";

/// Hosts that are considered "the Chiroot website".
const APP_HOSTS: &[&str] = &["chiroot.my.id", "www.chiroot.my.id"];

/// Hosts probed over TCP/443 to decide whether Chiroot is reachable.
const PROBE_HOSTS: &[&str] = &["www.chiroot.my.id", "chiroot.my.id"];
const PROBE_PORT: u16 = 443;

/// Timings (see the connection-failure strategy in `spawn_load_monitor`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIRM_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const LOAD_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const MONITOR_LIMIT: Duration = Duration::from_secs(300);
const SHOW_WINDOW_AFTER: Duration = Duration::from_secs(5);
const EXTERNAL_OPEN_COOLDOWN: Duration = Duration::from_secs(5);

/// Private URI scheme used to serve the built-in error screen.
/// On Windows (WebView2) it is reachable at `http://chiroot.localhost/…`.
const PROTOCOL: &str = "chiroot";

const ERROR_IMAGE: &[u8] = include_bytes!("../assets/error-sign.png");
const EMPTY_BODY: &[u8] = b"";

/* ─────────────────────── Navigation allow-list ──────────────────────────
 * The window may only browse the Chiroot website, the Supabase project and
 * the OAuth providers the site signs in with. Anything else is handed to the
 * user's default browser instead of being loaded inside the app.
 * ──────────────────────────────────────────────────────────────────────── */
const ALLOWED_HOSTS: &[&str] = &[
  // Chiroot
  "chiroot.my.id",
  "www.chiroot.my.id",
  // Supabase (auth / rest / realtime / storage)
  "astuagdezahmuuwjoghf.supabase.co",
  // Google — used by Chiroot's "Login with Google" (Supabase OAuth)
  "accounts.google.com",
  "myaccount.google.com",
  "www.google.com",
  "ssl.gstatic.com",
  "www.gstatic.com",
  "lh3.googleusercontent.com",
  // Other Supabase OAuth providers, kept available for the website
  "github.com",
  "www.github.com",
  "api.github.com",
  "avatars.githubusercontent.com",
  "discord.com",
  "api.discord.com",
  "cdn.discordapp.com",
  "appleid.apple.com",
  "login.microsoftonline.com",
  "login.live.com",
];

const ALLOWED_HOST_SUFFIXES: &[&str] = &[
  ".chiroot.my.id",
  ".supabase.co",
  ".supabase.in",
  ".google.com",
  ".gstatic.com",
  ".googleusercontent.com",
  ".github.com",
  ".githubusercontent.com",
  ".discord.com",
  ".discordapp.com",
  ".apple.com",
  ".microsoftonline.com",
  ".live.com",
];

/* ──────────────────────────── App state ───────────────────────────────── */
#[derive(Default)]
struct AppState {
  /// The Chiroot website finished loading at least once.
  loaded: bool,
  /// The built-in error screen is what the user is currently looking at.
  error_shown: bool,
  /// A retry attempt is already in flight (prevents double retries).
  retrying: bool,
  /// Bumped on every load attempt; older monitor threads bow out.
  generation: u64,
  /// Last URL handed to the system browser (spam guard).
  last_external: Option<(String, Instant)>,
}

/* ───────────────────────────── URL helpers ────────────────────────────── */

/// Base URL of the private `chiroot` protocol for the current platform.
const fn protocol_base() -> &'static str {
  // WebView2 (Windows) and Android serve custom protocols from
  // `http://<protocol>.localhost`; other platforms use `<protocol>://localhost`.
  if cfg!(any(windows, target_os = "android")) {
    "http://chiroot.localhost"
  } else {
    "chiroot://localhost"
  }
}

fn app_url() -> Url {
  Url::parse(APP_URL).expect("APP_URL must be a valid URL")
}

fn error_page_url() -> Url {
  Url::parse(&format!("{}/error.html", protocol_base())).expect("error page URL must be valid")
}

/// True for URLs served by our own private protocol (the error screen).
fn is_own_protocol(url: &Url) -> bool {
  if url.scheme() == PROTOCOL {
    return true;
  }
  matches!(url.host_str(), Some("chiroot.localhost" | "tauri.localhost"))
}

/// True for the real Chiroot website (any of its hosts, HTTPS only).
fn is_website_url(url: &Url) -> bool {
  if url.scheme() != "https" {
    return false;
  }
  match url.host_str() {
    Some(host) => host == "chiroot.my.id" || host.ends_with(".chiroot.my.id"),
    None => false,
  }
}

/* ─────────────────────── Connection reachability ──────────────────────── */

/// Cheap, dependency-free reachability probe: resolve + TCP connect to 443.
/// Covers "no internet", DNS failures, connection refused, timeouts and an
/// unreachable host without pulling in an HTTP/TLS stack.
fn reachable(timeout: Duration) -> bool {
  for host in PROBE_HOSTS {
    let Ok(addresses) = (*host, PROBE_PORT).to_socket_addrs() else {
      continue;
    };
    for address in addresses {
      if TcpStream::connect_timeout(&address, timeout).is_ok() {
        return true;
      }
    }
  }
  false
}

fn is_current(state: &Arc<Mutex<AppState>>, generation: u64) -> bool {
  state.lock().unwrap().generation == generation
}

/* ────────────────────────── Screen switching ──────────────────────────── */

/// Show the built-in error screen (artwork + Retry button).
fn show_error_screen(window: &WebviewWindow, state: &Arc<Mutex<AppState>>) {
  {
    let mut state = state.lock().unwrap();
    state.error_shown = true;
    state.loaded = false;
  }
  let _ = window.show();
  let _ = window.navigate(error_page_url());
}

/// (Re)load the Chiroot website and return the new load generation.
fn load_website(window: &WebviewWindow, state: &Arc<Mutex<AppState>>) -> u64 {
  let generation = {
    let mut state = state.lock().unwrap();
    state.generation += 1;
    state.loaded = false;
    state.error_shown = false;
    state.generation
  };
  let _ = window.navigate(app_url());
  generation
}

/// Watch a load attempt.
///
/// * Fast path — if the server is not reachable at all (DNS failure, offline,
///   refused, timeout) the error screen appears right away.
/// * Slow path — if the server *is* reachable we keep quiet: a slow website
///   must never be reported as a connection error. The route is re-checked
///   periodically until the page loads.
fn spawn_load_monitor(handle: AppHandle, state: Arc<Mutex<AppState>>, generation: u64) {
  thread::spawn(move || {
    let page_loaded = |state: &Arc<Mutex<AppState>>| state.lock().unwrap().loaded;

    if !reachable(PROBE_TIMEOUT)
      && is_current(&state, generation)
      && !page_loaded(&state)
      && !reachable(CONFIRM_PROBE_TIMEOUT)
      && is_current(&state, generation)
      && !page_loaded(&state)
    {
      if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
        show_error_screen(&window, &state);
      }
      return;
    }

    let started = Instant::now();
    let mut last_check = Instant::now();
    while started.elapsed() < MONITOR_LIMIT {
      thread::sleep(Duration::from_secs(1));

      if !is_current(&state, generation) || page_loaded(&state) {
        return;
      }

      if last_check.elapsed() >= LOAD_CHECK_INTERVAL {
        last_check = Instant::now();
        if !reachable(PROBE_TIMEOUT) {
          if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
            show_error_screen(&window, &state);
          }
          return;
        }
      }
    }
  });
}

/// Retry: probe once, then either restore the website or keep the error screen.
fn retry_now(handle: &AppHandle, state: &Arc<Mutex<AppState>>) -> bool {
  {
    let mut state = state.lock().unwrap();
    if state.retrying {
      return false;
    }
    state.retrying = true;
  }

  let server_up = reachable(PROBE_TIMEOUT) || reachable(CONFIRM_PROBE_TIMEOUT);
  let mut recovered = false;

  if let Some(window) = handle.get_webview_window(WINDOW_LABEL) {
    if server_up {
      let generation = load_website(&window, state);
      spawn_load_monitor(handle.clone(), state.clone(), generation);
      recovered = true;
    } else {
      show_error_screen(&window, state);
    }
  }

  state.lock().unwrap().retrying = false;
  recovered
}

/// IPC entry point used by the Retry button on the error screen.
#[tauri::command]
async fn chiroot_retry(
  app: AppHandle,
  state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<bool, String> {
  let app_state = state.inner().clone();

  {
    let mut state = app_state.lock().unwrap();
    if state.retrying {
      return Ok(false);
    }
    state.retrying = true;
  }

  // Keep the blocking connect off the UI thread.
  let server_up = tauri::async_runtime::spawn_blocking(move || {
    reachable(PROBE_TIMEOUT) || reachable(CONFIRM_PROBE_TIMEOUT)
  })
  .await
  .map_err(|error| error.to_string())?;

  let recovered = match app.get_webview_window(WINDOW_LABEL) {
    Some(window) => {
      if server_up {
        let generation = load_website(&window, &app_state);
        spawn_load_monitor(app.clone(), app_state.clone(), generation);
        true
      } else {
        show_error_screen(&window, &app_state);
        false
      }
    }
    None => false,
  };

  app_state.lock().unwrap().retrying = false;
  Ok(recovered)
}

/* ──────────────────────────── Navigation ──────────────────────────────── */

fn navigation_allowed(url: &Url) -> bool {
  if is_own_protocol(url) {
    return true;
  }

  match url.scheme() {
    "http" | "https" => {
      let host = match url.host_str() {
        Some(host) => host,
        None => return false,
      };

      // OAuth / PKCE callbacks may land on a loopback address.
      if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        return true;
      }
      if ALLOWED_HOSTS.contains(&host) {
        return true;
      }
      ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(*suffix))
    }
    // Inline media used by the website and by the error screen.
    "data" | "blob" | "about" => true,
    _ => false,
  }
}

/// Hand a non-allow-listed URL to the user's default browser.
fn open_externally(handle: &AppHandle, state: &Arc<Mutex<AppState>>, url: &Url) {
  if !matches!(url.scheme(), "http" | "https" | "mailto" | "tel") {
    return;
  }

  let target = url.to_string();
  {
    let mut state = state.lock().unwrap();
    let now = Instant::now();
    let recently_opened = state
      .last_external
      .as_ref()
      .map(|(url, at)| *url == target && now.duration_since(*at) < EXTERNAL_OPEN_COOLDOWN)
      .unwrap_or(false);
    if recently_opened {
      return;
    }
    state.last_external = Some((target.clone(), now));
  }

  let _ = handle.opener().open_url(target, None::<String>);
}

/* ───────────────────── Supabase bridge (init script) ──────────────────── */

/// Exposes the Supabase publishable configuration to the trusted Chiroot
/// origins only, and never overwrites a configuration the website already has.
fn supabase_bridge_script() -> String {
  let origins: Vec<String> = APP_HOSTS.iter().map(|host| format!("https://{host}")).collect();

  format!(
    r##"(function () {{
  try {{
    var trustedOrigins = {origins};
    if (trustedOrigins.indexOf(location.origin) === -1) {{
      return;
    }}

    // Namespaced, read-only and non-destructive: if the website already
    // configured Supabase itself, its values win.
    var bridge = window.__CHIROOT__ || {{}};
    if (!bridge.supabaseUrl) {{
      bridge.supabaseUrl = {supabase_url};
    }}
    if (!bridge.supabaseKey) {{
      bridge.supabaseKey = {supabase_key};
    }}
    window.__CHIROOT__ = bridge;
  }} catch (error) {{}}
}})();"##,
    origins = serde_json::to_string(&origins).expect("origins are serialisable"),
    supabase_url = serde_json::to_string(SUPABASE_URL).expect("url is serialisable"),
    supabase_key = serde_json::to_string(SUPABASE_PUBLISHABLE_KEY).expect("key is serialisable"),
  )
}

/* ─────────────────── Connection error screen (built-in) ───────────────── */

const ERROR_SCREEN_HTML: &str = r##"<!doctype html>
<html lang="id">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Chiroot</title>
    <style>
      :root { color-scheme: dark; }
      * { box-sizing: border-box; }
      html, body { height: 100%; }
      body {
        margin: 0;
        display: flex;
        align-items: center;
        justify-content: center;
        background: #12100f;
        color: #f3e7c8;
        font-family: "Segoe UI", "Segoe UI Variable Display", system-ui, sans-serif;
        -webkit-user-select: none;
        user-select: none;
        overflow: hidden;
      }
      main { padding: 40px 32px; text-align: center; max-width: 720px; }
      img.sign {
        display: block;
        width: min(520px, 74vw);
        height: auto;
        margin: 0 auto 32px;
        -webkit-user-drag: none;
      }
      h1 { margin: 0 0 10px; font-size: 20px; font-weight: 600; letter-spacing: 0.01em; }
      p { margin: 0 0 26px; font-size: 14px; line-height: 1.5; color: #c9b78c; }
      button {
        appearance: none;
        border: 0;
        border-radius: 8px;
        padding: 11px 26px;
        font: inherit;
        font-weight: 600;
        color: #1c1708;
        background: #f2c14e;
        cursor: pointer;
        transition: filter 0.15s ease, opacity 0.15s ease;
      }
      button:hover:not(:disabled) { filter: brightness(1.08); }
      button:disabled { opacity: 0.55; cursor: progress; }
      .status { margin: 18px 0 0; min-height: 18px; font-size: 13px; color: #c9b78c; }
    </style>
  </head>
  <body>
    <main>
      <img
        class="sign"
        src="/error-sign.png"
        alt="something went wrong. Pls try again later alright :&gt;"
        draggable="false"
      />
      <h1>Chiroot belum bisa terhubung</h1>
      <p>
        Koneksi ke server Chiroot gagal. Pastikan perangkat kamu terhubung ke
        internet, lalu coba lagi.
      </p>
      <button id="retry" type="button">Coba Lagi</button>
      <p class="status" id="status"></p>
    </main>

    <script>
      (function () {
        var button = document.getElementById('retry');
        var status = document.getElementById('status');
        var busy = false;

        function invoke() {
          // Tauri injects the IPC internals into every document of every webview.
          if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
            return window.__TAURI_INTERNALS__.invoke;
          }
          if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
            return window.__TAURI__.core.invoke;
          }
          return null;
        }

        function retry() {
          if (busy) {
            return;
          }
          busy = true;
          button.disabled = true;
          status.textContent = 'Menyambungkan ulang…';

          var call = invoke();
          if (!call) {
            // Last resort: let the app handle it through its own protocol.
            window.location.href = '/retry';
            return;
          }

          call('chiroot_retry')
            .then(function (recovered) {
              if (recovered) {
                status.textContent = 'Tersambung! Menunggu halaman dimuat…';
              } else {
                busy = false;
                button.disabled = false;
                status.textContent = 'Masih belum bisa terhubung. Coba lagi sebentar ya.';
              }
            })
            .catch(function () {
              busy = false;
              button.disabled = false;
              status.textContent = 'Gagal mencoba ulang. Tekan Coba Lagi sekali lagi.';
            });
        }

        button.addEventListener('click', retry);
      })();
    </script>
  </body>
</html>
"##;

/* ─────────────────────── Private protocol handler ─────────────────────── */

fn build_response(
  status: StatusCode,
  content_type: &'static str,
  body: &'static [u8],
) -> Response<Cow<'static, [u8]>> {
  Response::builder()
    .status(status)
    .header(CONTENT_TYPE, content_type)
    .body(Cow::Borrowed(body))
    .unwrap_or_else(|_| Response::new(Cow::Borrowed(EMPTY_BODY)))
}

fn handle_protocol_request(
  app: &AppHandle,
  state: &Arc<Mutex<AppState>>,
  request: Request<Vec<u8>>,
) -> Response<Cow<'static, [u8]>> {
  let uri = request.uri().to_string();
  let path = Url::parse(&uri)
    .map(|url| url.path().to_string())
    .unwrap_or_else(|_| uri.clone());

  match path.as_str() {
    "/error-sign.png" => build_response(StatusCode::OK, "image/png", ERROR_IMAGE),
    "/error.html" | "/index.html" | "/" | "" => {
      build_response(StatusCode::OK, "text/html; charset=utf-8", ERROR_SCREEN_HTML.as_bytes())
    }
    // Fallback retry trigger, used only if the page cannot reach the IPC layer.
    "/retry" => {
      let app = app.clone();
      let state = state.clone();
      thread::spawn(move || {
        retry_now(&app, &state);
      });
      build_response(StatusCode::OK, "text/html; charset=utf-8", ERROR_SCREEN_HTML.as_bytes())
    }
    _ => build_response(StatusCode::NOT_FOUND, "text/plain; charset=utf-8", EMPTY_BODY),
  }
}

/* ──────────────────────────────── Entrypoint ──────────────────────────── */

fn main() {
  let state = Arc::new(Mutex::new(AppState::default()));

  let protocol_state = state.clone();

  tauri::Builder::default()
    .plugin(tauri_plugin_opener::init())
    .invoke_handler(tauri::generate_handler![chiroot_retry])
    .register_uri_scheme_protocol(PROTOCOL, move |context, request| {
      handle_protocol_request(context.app_handle(), &protocol_state, request)
    })
    .setup(move |app| {
      let handle = app.handle().clone();
      app.manage(state.clone());

      let navigation_handle = handle.clone();
      let navigation_state = state.clone();
      let page_load_state = state.clone();

      // The window is created here (instead of in tauri.conf.json) so the
      // navigation / page-load hooks can be attached to it.
      WebviewWindowBuilder::new(&handle, WINDOW_LABEL, WebviewUrl::External(app_url()))
        .title("Chiroot")
        .inner_size(1200.0, 820.0)
        .min_inner_size(960.0, 640.0)
        .resizable(true)
        .decorations(true)
        .center()
        .visible(false)
        .initialization_script(supabase_bridge_script())
        .on_navigation(move |url| {
          if navigation_allowed(url) {
            return true;
          }
          open_externally(&navigation_handle, &navigation_state, url);
          false
        })
        .on_page_load(move |window, payload| {
          let url = payload.url().clone();
          match payload.event() {
            PageLoadEvent::Started => {
              // A new attempt to load the website: watch it.
              if is_website_url(&url) {
                let generation = {
                  let mut state = page_load_state.lock().unwrap();
                  state.generation += 1;
                  state.generation
                };
                spawn_load_monitor(
                  window.app_handle().clone(),
                  page_load_state.clone(),
                  generation,
                );
              }
            }
            PageLoadEvent::Finished => {
              if is_website_url(&url) {
                {
                  let mut state = page_load_state.lock().unwrap();
                  state.loaded = true;
                  state.error_shown = false;
                }
                let _ = window.show();
              } else if is_own_protocol(&url) {
                let _ = window.show();
              }
            }
          }
        })
        .build()?;

      // Watch the very first load.
      let generation = {
        let mut state = state.lock().unwrap();
        state.generation += 1;
        state.generation
      };
      spawn_load_monitor(handle.clone(), state.clone(), generation);

      // Failsafe: never leave the user staring at an invisible window.
      let show_handle = handle.clone();
      let show_state = state.clone();
      thread::spawn(move || {
        thread::sleep(SHOW_WINDOW_AFTER);
        if let Some(window) = show_handle.get_webview_window(WINDOW_LABEL) {
          if !show_state.lock().unwrap().loaded {
            let _ = window.show();
          }
        }
      });

      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running Chiroot");
}
