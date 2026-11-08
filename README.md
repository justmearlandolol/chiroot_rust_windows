# Chiroot — Windows Desktop Application

Portable Windows desktop client for the existing Chiroot website
(<https://chiroot.my.id/Main.html>), built with **Rust + Tauri v2**.

The application does **not** recreate, redesign or rewrite the website. It opens
the original site inside a native window, keeps Supabase auth / database /
realtime exactly as they are today, and shows a custom connection error screen
(with a Retry button) whenever the website cannot be reached.

---

## 1. Project structure

```text
chiroot_rust_windows/
│
├── src/
│   └── main.rs                        ← ALL Rust application logic (single file)
│
├── assets/
│   └── error-sign.png                 ← supplied error artwork (embedded in the .exe)
│
├── favicon.png                        ← supplied app icon (source of icons/icon.ico)
│
├── build.rs                           ← asset validation + tauri-build
├── Cargo.toml
├── tauri.conf.json
│
├── capabilities/
│   └── default.json                   ← mandatory Tauri v2 capability file
│
├── scripts/
│   └── generate-icons.py              ← favicon.png → Windows .ico (runs in CI)
│
└── .github/
    └── workflows/
        └── build-windows.yml          ← compiles Chiroot.exe on Windows x64
```

Generated (not committed, see `.gitignore`): `icons/`, `target/`, `dist/`, `gen/`.

---

## 2. Asset placement

| File | Location | Purpose |
| --- | --- | --- |
| favicon (1).png | **rename to `favicon.png`** at the repository root | application icon (window, taskbar, .exe properties) |
| error sign PNG | **`assets/error-sign.png`** | artwork of the built-in connection error screen |

Both files are used exactly as supplied — the artwork is never redrawn and the
error screen is never rebuilt with CSS. `assets/error-sign.png` is compiled into
the executable with `include_bytes!`, so the app stays a single portable file.

`icons/icon.ico` (16/20/24/32/40/48/64/128/256 px) is generated from
`favicon.png` by `scripts/generate-icons.py` during the GitHub Actions build —
you never have to create it by hand.

---

## 3. How the error screen works

WebView2 reports a failed navigation exactly like a successful one, so the app
decides from Rust instead of trusting a single event:

1. **Reachability probe** — `TcpStream::connect_timeout` to `chiroot.my.id:443`
   (5 s, with one 8 s confirmation attempt). This covers: no internet, DNS
   failure, server unavailable, connection refused, timeout and unreachable host.
2. **Fast path** — if the server cannot be reached while the page is not loaded,
   `assets/error-sign.png` + the Retry button are shown immediately.
3. **Slow path** — if the server *is* reachable the app stays quiet, because a
   slow website must never be reported as a connection error. The route is
   re-checked every 15 s while the page is still loading.
4. **Retry** — one click re-probes (a second click while a retry is running is
   ignored), restores the website on success, or keeps the error screen on
   failure. No restart is needed and there is no retry loop.

The screen and the image are served from a private `chiroot://` URI scheme
(`http://chiroot.localhost/` on Windows) that only ever returns the error page,
the PNG and a retry trigger — no filesystem, no shell, no app internals.

---

## 4. Supabase

`src/main.rs` hardcodes the **publishable** configuration:

```rust
const SUPABASE_URL: &str = "https://astuagdezahmuuwjoghf.supabase.co";
const SUPABASE_PUBLISHABLE_KEY: &str = "sb_publishable_NXhR-MquEMwYDrCBPAOgAQ_MZ4BXQlT";
```

Because the website is loaded remotely, Rust constants are not visible to its
JavaScript. The values are therefore published through a Tauri **initialisation
script** that:

* runs only when `location.origin` is `https://chiroot.my.id` or
  `https://www.chiroot.my.id`;
* exposes nothing but `window.__CHIROOT__.supabaseUrl` / `.supabaseKey`;
* **never overwrites** a configuration the website already set;
* creates no second Supabase client, so existing sessions, auth and RLS behave
  exactly like in the browser.

No service-role key, no secret key, no `.env` file and no RLS bypass anywhere.

---

## 5. Building

**Do not compile locally.** Every Windows build happens in GitHub Actions.

### Repository setup

```bash
git init
git add .
git commit -m "Chiroot desktop app (Rust + Tauri v2)"
git branch -M main
git remote add origin https://github.com/<username>/chiroot_rust_windows.git
git push -u origin main
```

Make sure `favicon.png` and `assets/error-sign.png` are committed.

### Trigger a build

* automatically on every push to `main`, or
* manually: **Actions → Build Chiroot (Windows x64) → Run workflow**.

### Download the executable

**Actions → the finished run → Artifacts → `Chiroot-Windows-x64`** (a zip that
contains `Chiroot.exe`).

The workflow produces a **portable executable only** — no MSI, no NSIS, no setup
file. `Chiroot.exe` can be copied anywhere and run directly. Nothing is uploaded
if the compilation fails.

---

## 6. WebView2 Runtime requirement

Tauri Windows apps render with **Microsoft Edge WebView2 Runtime**. It ships with
Windows 11 and with up-to-date Windows 10 installations, but it is **not**
bundled into `Chiroot.exe` (the portable build contains no browser installer).

If the app does not start on a machine that lacks it, install the
**WebView2 Runtime (Evergreen Bootstrapper)** once from
<https://developer.microsoft.com/microsoft-edge/webview2/>.

---

## 7. Security notes

* HTTPS only; navigation is restricted to the Chiroot hosts, the Supabase
  project and the OAuth providers the site signs in with — anything else opens
  in the user's default browser instead of inside the app.
* The capability file declares **no** `remote` entry, so the remote website has
  zero IPC access: only the app's own built-in error screen can call the single
  `chiroot_retry` command.
* No filesystem, shell, or developer-tool permission is granted anywhere, and
  devtools are disabled.
