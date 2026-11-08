//! Kota for Windows: AI kullanım limitleri görev çubuğunda.
//!
//! Linux'taki iki parçalı yapının (D-Bus servisi + GNOME eklentisi) tek
//! süreçli karşılığı: `kota-core` motoru burada çalışır, görev çubuğu simgesi
//! panel göstergesinin, açılır pencere ise GNOME menüsünün yerini alır.
//! Arayüz `web/` altındaki HTML/CSS/JS'tir (WebView2).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[path = "../../kota/src/accounts.rs"]
mod accounts;
mod agents;
#[path = "../../kota/src/focus.rs"]
mod focus;
mod keyring;
mod tray;
mod ui;

use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use kota_core::config::Config;
use kota_core::engine::{self, Engine};
use kota_core::forecast::History;
use kota_core::model::{Agent, Snapshot};
use kota_core::providers;
use serde_json::{Value, json};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Rect, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::Mutex;

use crate::agents::AgentScanner;
use crate::keyring::WinKeys;

const AGENT_TICK: Duration = Duration::from_secs(5);
const MANUAL_DEBOUNCE: Duration = Duration::from_secs(10);
const TRAY_ID: &str = "kota";

pub struct AppState {
    engine: Mutex<Engine>,
    keys: Arc<WinKeys>,
    latest: StdMutex<Option<Snapshot>>,
    agents: StdMutex<Vec<Agent>>,
    scanner: StdMutex<AgentScanner>,
    cache_path: PathBuf,
    history_path: PathBuf,
    last_done: StdMutex<Option<Instant>>,
    /// Engine'in yenileme döngüsünü uyandırmak için (ayar değişince).
    wake: tokio::sync::Notify,
    /// Son tıklanan tepsi simgesinin ekran konumu (açılır pencereyi yerleştirmek için).
    tray_rect: StdMutex<Option<Rect>>,
    /// Pencere odağı kaybedip gizlendiği an (aynı tıklamayla yeniden açılmasın).
    hidden_at: StdMutex<Option<Instant>>,
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AppState {
    fn snapshot_value(&self) -> Value {
        match lock(&self.latest).clone() {
            Some(mut s) => {
                s.agents = lock(&self.agents).clone();
                serde_json::to_value(&s).unwrap_or(Value::Null)
            }
            None => Value::Null,
        }
    }

    async fn refresh(&self, app: &AppHandle, only: Option<&str>) {
        let snap = {
            let mut eng = self.engine.lock().await;
            let snap = eng.refresh(only).await;
            let _ = engine::save_cache(&self.cache_path, &snap);
            if let Ok(h) = serde_json::to_vec(eng.history()) {
                let _ = std::fs::write(&self.history_path, h);
            }
            snap
        };
        *lock(&self.latest) = Some(snap);
        *lock(&self.last_done) = Some(Instant::now());
        self.broadcast(app);
    }

    fn scan_agents(&self) -> bool {
        let fresh = lock(&self.scanner).scan(Utc::now());
        let mut cur = lock(&self.agents);
        let changed = agents::structurally_differs(&cur, &fresh);
        *cur = fresh;
        changed
    }

    /// Anlık görüntüyü pencerelere gönderir ve tepsi simgesini günceller.
    fn broadcast(&self, app: &AppHandle) {
        let v = self.snapshot_value();
        let _ = app.emit("snapshot", &v);
        update_tray(app, self);
    }
}

// ------------------------------------------------------------ tepsi

fn update_tray(app: &AppHandle, st: &AppState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    let ui = ui::load();
    let tr = ui::is_turkish(&ui);
    let snap = lock(&st.latest).clone();
    let selected = ui::strv(&ui, "panel-providers");
    let mut lines = Vec::new();
    let mut top: Option<(String, kota_core::model::UsageWindow, bool)> = None;
    if let Some(s) = &snap {
        for p in &s.providers {
            if !selected.is_empty() && !selected.contains(&p.id) {
                continue;
            }
            let ws = ui::panel_windows(p, &ui, tr);
            if let Some(w) = ws.iter().max_by(|a, b| a.used_percent.total_cmp(&b.used_percent)) {
                if top.as_ref().is_none_or(|(_, t, _)| w.used_percent > t.used_percent) {
                    top = Some((p.id.clone(), w.clone(), p.stale || w.estimated));
                }
                let remaining = ui::flag(&ui, "show-remaining");
                let vals = ws
                    .iter()
                    .map(|w| {
                        let v = if remaining { (100.0 - w.used_percent).max(0.0) } else { w.used_percent };
                        let pct = if tr { format!("%{v:.0}") } else { format!("{v:.0}%") };
                        if ws.len() > 1 { format!("{} {pct}", tray::window_letter(w, tr)) } else { pct }
                    })
                    .collect();
                lines.push((p.name.clone(), vals));
            } else if let Some(b) = &p.balance {
                lines.push((p.name.clone(), vec![format!("{:.2} {}", b.amount, b.currency)]));
            }
        }
    }
    let style = ui::string(&ui, "tray-style", "ring");
    let light = ui::taskbar_is_light();
    let rgba = tray::render(top.as_ref().map(|(id, w, s)| (id.as_str(), w, *s)), &style, light);
    let _ = tray.set_icon(Some(Image::new_owned(rgba, tray::SIZE, tray::SIZE)));
    let paused = snap.as_ref().is_some_and(|s| s.paused);
    let _ = tray.set_tooltip(Some(tray::tooltip(snap.as_ref(), &lines, paused, tr)));
}

/// Açılır pencereyi tepsi simgesinin hemen üstüne (görev çubuğu üstteyse altına) yerleştirir.
fn place_popup(app: &AppHandle, win: &tauri::WebviewWindow) {
    let st = app.state::<Arc<AppState>>();
    let rect = *lock(&st.tray_rect);
    let Ok(size) = win.outer_size() else { return };
    let monitor = rect
        .and_then(|r| {
            let p = r.position.to_physical::<f64>(1.0);
            app.monitor_from_point(p.x, p.y).ok().flatten()
        })
        .or_else(|| win.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let work = m.work_area();
    let (wx, wy, ww, wh) = (work.position.x, work.position.y, work.size.width as i32, work.size.height as i32);
    let margin = (12.0 * m.scale_factor()) as i32;
    let (w, h) = (size.width as i32, size.height as i32);
    let (mut x, mut y) = (wx + ww - w - margin, wy + wh - h - margin);
    if let Some(r) = rect {
        let p = r.position.to_physical::<f64>(m.scale_factor());
        let s = r.size.to_physical::<f64>(m.scale_factor());
        let cx = (p.x + s.width / 2.0) as i32;
        x = cx - w / 2;
        // Görev çubuğu üstteyse pencere simgenin altına açılır.
        y = if (p.y as i32) < wy + wh / 2 { (p.y + s.height) as i32 + margin } else { p.y as i32 - h - margin };
    }
    x = x.clamp(wx + margin, wx + ww - w - margin);
    y = y.clamp(wy + margin, wy + wh - h - margin);
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

fn toggle_popup(app: &AppHandle) {
    let Some(win) = app.get_webview_window("popup") else { return };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    // Pencere odak kaybıyla az önce gizlendiyse (simgeye tıklama) yeniden açma.
    let st = app.state::<Arc<AppState>>();
    if lock(&st.hidden_at).is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return;
    }
    place_popup(app, &win);
    let _ = win.show();
    let _ = win.set_focus();
    let _ = win.emit("popup-shown", ());
}

fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // Windows'ta pencere ana iş parçacığında (senkron komut / menü olayı) kurulursa
    // WebView2 kilitlenir; kurulumu ayrı bir görevde yap.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
            .title("Kota")
            .inner_size(820.0, 880.0)
            .min_inner_size(560.0, 500.0)
            .center()
            .build();
    });
}

// ------------------------------------------------------------ komutlar

type S<'a> = State<'a, Arc<AppState>>;

#[tauri::command]
async fn get_snapshot(st: S<'_>) -> Result<Value, String> {
    Ok(st.snapshot_value())
}

#[tauri::command]
async fn refresh(app: AppHandle, st: S<'_>) -> Result<Value, String> {
    let recent = lock(&st.last_done).is_some_and(|t| t.elapsed() < MANUAL_DEBOUNCE);
    if !recent {
        st.refresh(&app, None).await;
    }
    Ok(st.snapshot_value())
}

#[tauri::command]
async fn set_paused(app: AppHandle, st: S<'_>, paused: bool) -> Result<(), String> {
    update_config(&app, &st, None, |c| c.paused = paused).await
}

async fn update_config(app: &AppHandle, st: &AppState, refresh: Option<&str>, f: impl FnOnce(&mut Config)) -> Result<(), String> {
    {
        let mut eng = st.engine.lock().await;
        let mut cfg = eng.config().clone();
        f(&mut cfg);
        cfg.save().map_err(|e| format!("config write failed: {e}"))?;
        eng.reconfigure(cfg);
        if let Some(last) = eng.last().cloned() {
            *lock(&st.latest) = Some(last);
        }
    }
    match refresh {
        Some("*") => st.refresh(app, None).await,
        Some(id) => st.refresh(app, Some(id)).await,
        None => st.broadcast(app),
    }
    st.wake.notify_one();
    Ok(())
}

#[tauri::command]
async fn list_providers(st: S<'_>) -> Result<Value, String> {
    let cfg = st.engine.lock().await.config().clone();
    let list: Vec<Value> = providers::catalog()
        .into_iter()
        .map(|e| {
            let needs_key = matches!(
                e.auth,
                providers::AuthKind::ApiKey | providers::AuthKind::AdminKey | providers::AuthKind::GithubToken
            );
            json!({
                "id": e.id, "name": e.name, "auth": e.auth, "shows": e.shows, "key_url": e.key_url,
                "budget_supported": e.budget, "budget": cfg.budget(e.id),
                "login_command": e.login_command, "enabled": cfg.is_enabled(e.id),
                "has_key": needs_key && keyring::has_key(e.id),
            })
        })
        .collect();
    Ok(Value::Array(list))
}

fn known(id: &str) -> Result<(), String> {
    if providers::catalog().iter().any(|e| e.id == id) { Ok(()) } else { Err(format!("unknown provider {id}")) }
}

#[tauri::command]
async fn set_provider_enabled(app: AppHandle, st: S<'_>, id: String, enabled: bool) -> Result<(), String> {
    known(&id)?;
    update_config(&app, &st, enabled.then_some(id.as_str()), |c| c.set_enabled(&id, enabled)).await
}

#[tauri::command]
async fn set_api_key(app: AppHandle, st: S<'_>, id: String, key: String) -> Result<(), String> {
    known(&id)?;
    keyring::store(&id, &key)?;
    st.keys.invalidate();
    update_config(&app, &st, Some(&id), |_| {}).await
}

#[tauri::command]
async fn set_budget(app: AppHandle, st: S<'_>, id: String, amount: f64) -> Result<(), String> {
    update_config(&app, &st, Some(&id), |c| c.set_budget(&id, Some(amount))).await
}

#[tauri::command]
async fn get_settings(st: S<'_>) -> Result<Value, String> {
    let c = st.engine.lock().await.config().clone();
    Ok(json!({
        "interval_seconds": c.interval_seconds, "warning": c.thresholds.warning,
        "critical": c.thresholds.critical, "paused": c.paused,
        "claude_refresh_tokens": c.claude.refresh_tokens,
        "local_logs": c.claude.local_logs && c.codex.local_logs,
        "config_path": kota_core::paths::config_file(),
    }))
}

#[tauri::command]
async fn set_settings(app: AppHandle, st: S<'_>, patch: Value) -> Result<(), String> {
    let num = |k: &str| patch.get(k).and_then(Value::as_f64);
    let flag = |k: &str| patch.get(k).and_then(Value::as_bool);
    let needs = num("warning").is_some() || num("critical").is_some() || flag("local_logs").is_some();
    update_config(&app, &st, needs.then_some("*"), |c| {
        if let Some(v) = num("interval_seconds") {
            c.interval_seconds = (v as u64).clamp(kota_core::config::MIN_INTERVAL_SECS, 3600);
        }
        if let Some(v) = num("warning") {
            c.thresholds.warning = v.clamp(1.0, 100.0);
        }
        if let Some(v) = num("critical") {
            c.thresholds.critical = v.clamp(1.0, 100.0);
        }
        if let Some(v) = flag("paused") {
            c.paused = v;
        }
        if let Some(v) = flag("claude_refresh_tokens") {
            c.claude.refresh_tokens = v;
        }
        if let Some(v) = flag("local_logs") {
            c.claude.local_logs = v;
            c.codex.local_logs = v;
        }
    })
    .await
}

#[tauri::command]
async fn stop_agent(app: AppHandle, st: S<'_>, pid: u32) -> Result<(), String> {
    let known = lock(&st.agents).clone();
    lock(&st.scanner).stop(pid, &known)?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    st.scan_agents();
    st.broadcast(&app);
    Ok(())
}

/// Sağlayıcının kendi giriş komutunu yeni bir konsol penceresinde açar.
#[tauri::command]
fn connect(id: String) -> Result<(), String> {
    let argv = providers::login_command(&id).ok_or_else(|| format!("unknown provider: {id}"))?;
    let line = argv.join(" ");
    let msg = "Giris tamamlaninca bu pencereyi kapatabilirsiniz / You can close this window after logging in.";
    std::process::Command::new("cmd")
        .args(["/C", "start", "Kota", "cmd", "/K", &format!("{line} & echo. & echo {msg}")])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open a console: {e}"))
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    let p = if let Some(rest) = path.strip_prefix('~') {
        format!("{}{}", kota_core::paths::home_dir().display(), rest)
    } else {
        path
    };
    std::process::Command::new("explorer").arg(p).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
    open_settings(&app);
}

#[tauri::command]
fn hide_popup(app: AppHandle) {
    if let Some(p) = app.get_webview_window("popup") {
        let _ = p.hide();
    }
}

/// İçerik yüksekliğine göre açılır pencereyi boyutlandırır (CSS pikseli).
#[tauri::command]
fn fit_popup(app: AppHandle, width: f64, height: f64) {
    if let Some(p) = app.get_webview_window("popup") {
        let max = p
            .current_monitor()
            .ok()
            .flatten()
            .map(|m| m.work_area().size.height as f64 / m.scale_factor() - 24.0)
            .unwrap_or(900.0);
        let _ = p.set_size(tauri::LogicalSize::new(width, height.min(max)));
        if p.is_visible().unwrap_or(false) {
            place_popup(&app, &p);
        }
    }
}

#[tauri::command]
fn get_ui() -> Value {
    ui::load()
}

#[tauri::command]
fn set_ui(app: AppHandle, st: S<'_>, key: String, value: Value) -> Result<(), String> {
    ui::set(&key, value.clone())?;
    let _ = app.emit("ui-changed", json!({"key": key, "value": value}));
    if key == "autostart" {
        use tauri_plugin_autostart::ManagerExt;
        let al = app.autolaunch();
        let _ = if value.as_bool().unwrap_or(false) { al.enable() } else { al.disable() };
    }
    update_tray(&app, &st);
    Ok(())
}

#[tauri::command]
async fn list_accounts() -> Result<Value, String> {
    tokio::task::spawn_blocking(|| {
        let current: serde_json::Map<String, Value> = accounts::PROVIDERS
            .iter()
            .map(|p| (p.to_string(), json!(accounts::current_email(p))))
            .collect();
        json!({"profiles": accounts::list(), "current": current})
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_account(provider: String, name: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || accounts::save(&provider, (!name.trim().is_empty()).then_some(name.trim())))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn remove_account(provider: String, name: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || accounts::remove(&provider, &name)).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn switch_account(app: AppHandle, st: S<'_>, provider: String, name: String) -> Result<(), String> {
    let p = provider.clone();
    tokio::task::spawn_blocking(move || accounts::switch(&p, &name)).await.map_err(|e| e.to_string())??;
    update_config(&app, &st, Some(&provider), |_| {}).await
}

#[tauri::command]
fn app_version() -> String {
    kota_core::VERSION.to_string()
}

// ------------------------------------------------------------ başlangıç

fn build_state() -> Arc<AppState> {
    let (config, warning) = Config::load();
    if let Some(w) = warning {
        log::warn!("config file ignored, using defaults: {w}");
    }
    let keys = Arc::new(WinKeys::new());
    let mut eng = Engine::with_keys(config, keys.clone());
    let cache_path = engine::default_cache_path();
    let history_path = cache_path.with_file_name("history.json");
    let mut initial = None;
    if let Some(snap) = engine::load_cache(&cache_path) {
        initial = Some(snap.clone());
        eng.seed(snap);
    }
    if let Some(h) = std::fs::read(&history_path).ok().and_then(|b| serde_json::from_slice::<History>(&b).ok()) {
        eng.set_history(h);
    }
    Arc::new(AppState {
        engine: Mutex::new(eng),
        keys,
        latest: StdMutex::new(initial),
        agents: StdMutex::new(Vec::new()),
        scanner: StdMutex::new(AgentScanner::new()),
        cache_path,
        history_path,
        last_done: StdMutex::new(None),
        wake: tokio::sync::Notify::new(),
        tray_rect: StdMutex::new(None),
        hidden_at: StdMutex::new(None),
    })
}

fn tray_menu(app: &AppHandle, tr: bool, paused: bool) -> tauri::Result<Menu<tauri::Wry>> {
    let t = |a: &'static str, b: &'static str| if tr { a } else { b };
    Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "open", t("Kota'yı aç", "Open Kota"), true, None::<&str>)?,
            &MenuItem::with_id(app, "refresh", t("Şimdi yenile", "Refresh now"), true, None::<&str>)?,
            &CheckMenuItem::with_id(app, "pause", t("Duraklat", "Pause"), true, paused, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "settings", t("Ayarlar…", "Settings…"), true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", t("Çıkış", "Quit"), true, None::<&str>)?,
        ],
    )
}

fn create_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let ui = ui::load();
    let menu = tray_menu(app, ui::is_turkish(&ui), false)?;
    let rgba = tray::render(None, "logo", ui::taskbar_is_light());
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::new_owned(rgba, tray::SIZE, tray::SIZE))
        .tooltip("Kota")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| {
            let st = app.state::<Arc<AppState>>().inner().clone();
            match ev.id().as_ref() {
                "open" => toggle_popup(app),
                "settings" => open_settings(app),
                "quit" => app.exit(0),
                "refresh" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move { st.refresh(&app, None).await });
                }
                "pause" => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let paused = !st.engine.lock().await.config().paused;
                        let _ = update_config(&app, &st, None, |c| c.paused = paused).await;
                    });
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = ev
            {
                let app = tray.app_handle();
                *lock(&app.state::<Arc<AppState>>().tray_rect) = Some(rect);
                toggle_popup(app);
            }
        })
        .build(app)
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let state = build_state();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| toggle_popup(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            refresh,
            set_paused,
            list_providers,
            set_provider_enabled,
            set_api_key,
            set_budget,
            get_settings,
            set_settings,
            stop_agent,
            connect,
            open_path,
            open_settings_window,
            hide_popup,
            fit_popup,
            get_ui,
            set_ui,
            list_accounts,
            save_account,
            remove_account,
            switch_account,
            app_version,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            create_tray(&handle)?;
            // İlk açılışta Windows ile başlat (kullanıcı ayarlardan kapatabilir).
            let ui = ui::load();
            if ui.get("autostart").is_none() {
                use tauri_plugin_autostart::ManagerExt;
                let _ = handle.autolaunch().enable();
                let _ = ui::set("autostart", json!(true));
            }
            if let Some(popup) = app.get_webview_window("popup") {
                let h = handle.clone();
                popup.on_window_event(move |ev| {
                    if let tauri::WindowEvent::Focused(false) = ev
                        && let Some(w) = h.get_webview_window("popup")
                    {
                        let _ = w.hide();
                        *lock(&h.state::<Arc<AppState>>().hidden_at) = Some(Instant::now());
                    }
                });
            }
            // Sağlayıcı döngüsü
            let (st, h) = (state.clone(), handle.clone());
            tauri::async_runtime::spawn(async move {
                st.scan_agents();
                st.broadcast(&h);
                loop {
                    let (paused, interval) = {
                        let eng = st.engine.lock().await;
                        (eng.config().paused, eng.config().interval())
                    };
                    if !paused {
                        st.refresh(&h, None).await;
                    }
                    // Tepsi menüsündeki "Duraklat" işaretini senkron tut.
                    if let (Some(tray), Ok(menu)) = (h.tray_by_id(TRAY_ID), tray_menu(&h, ui::is_turkish(&ui::load()), paused)) {
                        let _ = tray.set_menu(Some(menu));
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => {}
                        _ = st.wake.notified() => {}
                    }
                }
            });
            // Ajan döngüsü
            let (st, h) = (state.clone(), handle.clone());
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(AGENT_TICK).await;
                    let st2 = st.clone();
                    let changed = tokio::task::spawn_blocking(move || st2.scan_agents()).await.unwrap_or(false);
                    if changed {
                        st.broadcast(&h);
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Kota");
}
