// La coquille Tauri : fenêtre, icône de la barre des tâches, démarrage avec la
// session et la boucle du moteur (qui tourne fenêtre fermée).

use crate::engine::{self, Config, Engine, Link, Status};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

struct Shared {
    engine: Arc<Mutex<Engine>>,
    status: Arc<Mutex<Status>>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}
fn config_path() -> PathBuf {
    home().join(".config/my-safe-place/config.json")
}
fn links_path() -> PathBuf {
    home().join(".local/share/my-safe-place/links.json")
}

fn load<T: serde::de::DeserializeOwned + Default>(p: &PathBuf) -> T {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Écrit à côté puis renomme : un fichier coupé en deux perdrait les règles.
fn save<T: serde::Serialize>(p: &PathBuf, v: &T) -> Result<(), String> {
    let dir = p.parent().ok_or("chemin invalide")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
struct AppState {
    config: Config,
    status: Status,
}

// shortcut: le moteur garde le verrou pendant une copie, donc ces commandes
// attendent la fin d'un gros fichier ; async pour ne pas figer la fenêtre.
#[tauri::command]
async fn get_state(s: State<'_, Shared>) -> Result<AppState, String> {
    let config = s.engine.lock().unwrap().config.clone();
    Ok(AppState { config, status: s.status.lock().unwrap().clone() })
}

#[tauri::command]
async fn save_config(s: State<'_, Shared>, mut config: Config) -> Result<Config, String> {
    config.rules = config.rules.into_iter().map(engine::prepare_rule).collect::<Result<_, _>>()?;
    save(&config_path(), &config)?;
    s.engine.lock().unwrap().config = config.clone();
    Ok(config)
}

#[tauri::command]
async fn preview_existing(s: State<'_, Shared>, rule_id: String) -> Result<Vec<PathBuf>, String> {
    let e = s.engine.lock().unwrap();
    let rule = e.config.rules.iter().find(|r| r.id == rule_id).ok_or("règle inconnue")?;
    Ok(engine::preview_existing(rule, &e.config.categories))
}

#[tauri::command]
async fn apply_existing(s: State<'_, Shared>, rule_id: String, files: Vec<PathBuf>) -> Result<usize, String> {
    let mut e = s.engine.lock().unwrap();
    let n = e.apply_existing(&rule_id, &files);
    save(&links_path(), &e.links)?;
    n
}

fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Démarre avec la session (XDG), fenêtre cachée.
fn install_autostart() {
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).or_else(|| std::env::current_exe().ok());
    let Some(exe) = exe else { return };
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=MY SAFE PLACE\nExec=\"{}\" --hidden\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    let p = home().join(".config/autostart/my-safe-place.desktop");
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(p, desktop);
}

fn notify(body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-u", "critical", "-a", "MY SAFE PLACE", "-i", "dialog-warning", "MY SAFE PLACE", body])
        .spawn();
}

pub fn run() {
    // WebKitGTK plante au démarrage sous KDE Wayland + NVIDIA sans ça.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    let mut config: Config = load(&config_path());
    if config.categories.is_empty() {
        config.categories = engine::default_categories();
    }
    let links: Vec<Link> = load(&links_path());
    let shared = Shared {
        engine: Arc::new(Mutex::new(Engine::new(config, links))),
        status: Arc::new(Mutex::new(Status::default())),
    };
    let (eng, stat) = (shared.engine.clone(), shared.status.clone());
    let hidden = std::env::args().any(|a| a == "--hidden");

    tauri::Builder::default()
        // Deux moteurs en même temps se battraient pour les mêmes fichiers.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![get_state, save_config, preview_existing, apply_existing])
        .on_window_event(|w, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .setup(move |app| {
            install_autostart();
            let normal = app.default_window_icon().cloned().ok_or("pas d'icône")?.to_owned();
            let alert = Image::from_bytes(include_bytes!("../icons/alert.png"))?;
            let open = MenuItem::with_id(app, "open", "Ouvrir", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let tray = TrayIconBuilder::with_id("main")
                .icon(normal.clone())
                .tooltip("MY SAFE PLACE")
                .menu(&Menu::with_items(app, &[&open, &quit])?)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "open" => show(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|t, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                        show(t.app_handle());
                    }
                })
                .build(app)?;
            if !hidden {
                show(app.handle());
            }

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut blink = false;
                let mut tick = 0u32;
                loop {
                    // Le moteur tourne toutes les 2 s ; le clignotant toutes les 0,5 s.
                    if tick % 4 == 0 {
                        let status = {
                            let mut e = eng.lock().unwrap();
                            let (s, changed) = e.tick();
                            if changed {
                                let _ = save(&links_path(), &e.links);
                            }
                            s
                        };
                        let previous = std::mem::replace(&mut *stat.lock().unwrap(), status.clone());
                        let newly: Vec<String> = status.unplugged.iter().filter(|id| !previous.unplugged.contains(id)).cloned().collect();
                        if !newly.is_empty() {
                            let e = eng.lock().unwrap();
                            let targets: Vec<String> = e.config.rules.iter().filter(|r| newly.contains(&r.id)).map(|r| r.target.display().to_string()).collect();
                            notify(&format!("Attention, tu as débranché un truc !\n{}\nLes fichiers attendent dans le dossier de départ.", targets.join("\n")));
                            show(&handle);
                        }
                        if status != previous {
                            let _ = handle.emit("status", &status);
                        }
                    }
                    let alarm = !stat.lock().unwrap().unplugged.is_empty();
                    if alarm || blink {
                        blink = alarm && !blink;
                        let _ = tray.set_icon(Some(if blink { alert.clone() } else { normal.clone() }));
                    }
                    tick = tick.wrapping_add(1);
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("MY SAFE PLACE n'a pas pu démarrer");
}
