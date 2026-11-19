//! Logicool マウスの「戻る」ボタンを Esc に置き換えるホスト側リマップ。
//!
//! 旧運用 (AHK key_swap_setting.ahk + G Hub オンボードメモリの F14 経路) の置き換えで、
//! 手法は AHK と同じ **WH_MOUSE_LL フック + SendInput**。
//!
//! このモジュール自体は単なるブール設定 (`game_back_esc`) しか持たない。
//! **ON の間は常に「戻る」(XBUTTON1) を握りつぶして Esc を注入**し、OFF なら何もしない。
//! ゲーム中だけ有効化するのは標準機能の「自動化 > プロセス連動」の仕事:
//! 既定プロファイル (OFF) とゲーム用プロファイル (ON) を作り、ゲームの exe に連動させる。
//!
//! - 設定は `%APPDATA%\DeviceHub\mouse_remap.json` に永続。
//! - フックスレッドはアプリ生存期間で 1 本 (LL フックはメッセージループが必要)。
//! - 注入イベントには `MARKER` を付けて自分自身のフックで再捕捉しない。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::UI::WindowsAndMessaging::WH_MOUSE_LL;

/// 注入マーカー (dwExtraInfo)。自分の SendInput をフックで無視するための印。
const MARKER: usize = 0x0044_4855; // "DHUB"

/// リマップ設定 (`%APPDATA%\DeviceHub\mouse_remap.json`)。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RemapConfig {
    /// ON の間、「戻る」ボタンを Esc に差し替える。
    /// 通常は OFF にして、プロセス連動のゲーム用プロファイルから ON にする。
    pub game_back_esc: bool,
}

impl Default for RemapConfig {
    fn default() -> Self {
        Self { game_back_esc: false }
    }
}

static CONFIG: OnceLock<Mutex<RemapConfig>> = OnceLock::new();
static HOOK_STARTED: AtomicBool = AtomicBool::new(false);

fn config() -> &'static Mutex<RemapConfig> {
    CONFIG.get_or_init(|| Mutex::new(load()))
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    let dir = PathBuf::from(base).join("DeviceHub");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("mouse_remap.json"))
}

fn load() -> RemapConfig {
    config_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(cfg: &RemapConfig) {
    if let Some(p) = config_path() {
        if let Ok(v) = serde_json::to_vec_pretty(cfg) {
            let _ = std::fs::write(p, v);
        }
    }
}

/// 現在の設定のスナップショット (UI の read 用)。
pub fn get() -> RemapConfig {
    config()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// 設定を更新して永続する。フックがまだ無ければ起動する。
pub fn set(cfg: RemapConfig) {
    *config().lock().unwrap_or_else(|p| p.into_inner()) = cfg.clone();
    save(&cfg);
    ensure_hook();
}

/// フックスレッドの起動 (冪等)。ドライバ open 時に呼ぶ。
pub fn ensure_hook() {
    if HOOK_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::Builder::new()
        .name("logi-mouse-hook".into())
        .spawn(|| unsafe { hook_main() })
        .ok();
}

unsafe fn hook_main() {
    let hook = windows_sys::Win32::UI::WindowsAndMessaging::SetWindowsHookExW(
        WH_MOUSE_LL,
        Some(mouse_proc),
        0,
        0,
    );
    if hook == 0 {
        return; // フック不能 (権限等) — 設定は残るが差し替えは無効
    }
    // LL フックはフックスレッドのメッセージループが必要。
    let mut msg = std::mem::zeroed::<windows_sys::Win32::UI::WindowsAndMessaging::MSG>();
    while windows_sys::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, 0, 0, 0) > 0 {}
    windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
}

/// Esc キーの down+up を注入 (フックで無視されるよう MARKER 付き)。
fn send_esc() {
    unsafe {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    wScan: 0,
                    dwFlags: 0,
                    time: 0,
                    dwExtraInfo: MARKER,
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_ESCAPE,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: MARKER,
                },
            },
        };
        SendInput(2, [down, up].as_mut_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    if ncode < 0 {
        return CallNextHookEx(0, ncode, wparam, lparam);
    }
    let st = &*(lparam as *const MSLLHOOKSTRUCT);
    if st.dwExtraInfo == MARKER {
        return CallNextHookEx(0, ncode, wparam, lparam);
    }
    let msg = wparam as u32;
    if msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP {
        // mouseData の上位ワード: 1 = XBUTTON1 (戻る)。
        if (st.mouseData >> 16) as u16 == 1 {
            let enabled = config()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .game_back_esc;
            if enabled {
                // 「戻る」の down/up を握りつぶして Esc を注入。
                if msg == WM_XBUTTONDOWN {
                    send_esc();
                }
                return 1;
            }
        }
    }
    CallNextHookEx(0, ncode, wparam, lparam)
}
