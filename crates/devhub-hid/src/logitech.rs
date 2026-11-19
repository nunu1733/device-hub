//! Logicool / Logitech HID++ 2.0 — Lightspeed レシーバー経由のマウス (G903 HERO 実機)。
//!
//! プロトコル (2026-11 実機解析、docs/PROTOCOL_NOTES.md §6):
//! - 設定チャネル: if2 `FF00:0001` (short, report 0x10) + `FF00:0002` (long, report 0x11)。
//!   書き込みは short = 8B / long = 21B (report ID 込み)。入力はどちらのハンドルでも
//!   観測できるので常駐リーダーで集める (Windows HID は短 timeout の連続 read で
//!   報告を取りこぼすことがあるためスレッド + チャネル方式)。
//! - ペア機はレシーバー上の index (1..6) で指定。`Root.Ping` で 2.0 対応機が
//!   `[major, minor, echo]` を返す。スリープ中は 0x8F エラー (コード 0x09 =
//!   RESOURCE_ERROR、未ペアは 0x08 = UNKNOWN_DEVICE)。
//! - エラー応答は `[rid, dev, 0x8F, SubID/feature エコー, Address/func エコー, code, 0]`
//!   (実測。code は Solaar の hidpp10 ErrorCode と同じ)。
//! - 対応機能: DPI (0x2201)、ポーリングレート (0x8061)、電池 (0x1004/0x1000)、
//!   デバイス名 (0x0003)、ファーム (0x0002)。ボタン割当 (0x8100 オンボード
//!   プロファイル) は未対応 — caps で宣言しない。
//!
//! G Hub 置換にあたっての注意: G Hub は onboardMode を使う (settings.db で確認)。
//! ホスト側 DPI (0x2201) はオンボードモード OFF のとき実設定として効く。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 要求の swid (4bit)。デバイスの自発通知は swid=0 なので 1 を固定で使う。
const SWID: u8 = 13;
const SHORT_RID: u8 = 0x10;
const LONG_RID: u8 = 0x11;

/// HID++ 1.0/2.0 エラーコード (実測の r[5] 位置。Solaar hidpp10_constants.ErrorCode)。
#[allow(dead_code)]
mod err {
    pub const INVALID_SUB_ID_COMMAND: u8 = 0x01;
    pub const INVALID_ADDRESS: u8 = 0x02;
    pub const INVALID_VALUE: u8 = 0x03;
    pub const CONNECTION_REQUEST_FAILED: u8 = 0x04;
    pub const BUSY: u8 = 0x07;
    pub const UNKNOWN_DEVICE: u8 = 0x08;
    pub const RESOURCE_ERROR: u8 = 0x09;
    pub const REQUEST_UNAVAILABLE: u8 = 0x0A;
}

/// リーダースレッドが積んだ 1 報告。
struct Rx {
    data: Vec<u8>,
}

/// ping 1 回の結果。
#[derive(Clone, Copy, PartialEq)]
enum Ping {
    /// 2.0 対応機が起きている。
    Awake,
    /// ペアリング済みだがリンク到達不可 (スリープ)。
    Asleep,
    /// その index にペア機はない。
    Absent,
}

pub struct LogiDriver {
    /// short コレクション書き込みハンドル (現在は long に統一して送るため未使用だが
    /// 経路実験用に保持する)。
    #[allow(dead_code)]
    short: Mutex<hidapi::HidDevice>,
    long: Mutex<hidapi::HidDevice>,
    rx: Mutex<Receiver<Rx>>,
    stop: Arc<AtomicBool>,
    info: Mutex<DeviceInfo>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// ペア機のレシーバー上 index (スリープ中も変わらず Some)。
    dev: Mutex<Option<u8>>,
    /// リンクが生きている (ping が通る) か。
    awake: Mutex<bool>,
    /// (feature index, feature id)。起きている間のスナップショット。
    feats: Mutex<Vec<(u8, u16)>>,
    /// 0x2201 の DPI 一覧 (昇順)。
    dpi_list: Mutex<Vec<u32>>,
    battery_cache: Mutex<Option<Battery>>,
}

/// レシーバー 1 台ぶんのハンドル束。書き込み用 (short/long) とリーダー用
/// (short/long) を別ハンドルで開く (HidDevice は clone できないため)。
fn open_handles(dev: hidapi::HidDevice, pid: u16) -> HubResult<Handles> {
    let api = hidapi::HidApi::new().map_err(|e| HubError::Io(format!("hidapi init: {e}")))?;
    let mut short_path = None;
    let mut long_path = None;
    for d in api.device_list() {
        if d.vendor_id() != 0x046D || d.product_id() != pid || d.usage_page() != 0xFF00 {
            continue;
        }
        let p = d.path().to_owned();
        if d.usage() == 0x0001 {
            short_path = Some(p);
        } else if d.usage() == 0x0002 {
            long_path = Some(p);
        }
    }
    let Some(long_path) = long_path else {
        return Err(HubError::Io("HID++ long コレクションが見つかりません".into()));
    };
    let open = |p: std::ffi::CString| {
        api.open_path(&p).map_err(|e| HubError::Io(format!("hid open: {e}")))
    };
    // dev (lib.rs から渡された short ハンドル) を書き込み用に使い、
    // 読み取り用に short をもう 1 本開く。
    let short_r = open(short_path.ok_or_else(|| HubError::Io("HID++ short コレクションが見つかりません".into()))?)?;
    let long_w = open(long_path.clone())?;
    let long_r = open(long_path)?;
    Ok(Handles {
        short_w: dev,
        long_w,
        short_r,
        long_r,
    })
}

/// ハンドル束。`_w` = 書き込み用、`_r` = リーダースレッド用。
struct Handles {
    short_w: hidapi::HidDevice,
    long_w: hidapi::HidDevice,
    short_r: hidapi::HidDevice,
    long_r: hidapi::HidDevice,
}

/// 所有権ごとハンドルをスレッドへ move するリーダー起動 (HidDevice は
/// Send だが Sync でない — 1 スレッドが 1 ハンドルを専有する)。
fn spawn_reader_owned(
    short: hidapi::HidDevice,
    long: hidapi::HidDevice,
    stop: Arc<AtomicBool>,
) -> Receiver<Rx> {
    let (tx, rx): (Sender<Rx>, Receiver<Rx>) = mpsc::channel();
    for dev in [short, long] {
        let tx = tx.clone();
        let stop = stop.clone();
        std::thread::spawn(move || read_owned(dev, tx, stop));
    }
    rx
}

fn read_owned(dev: hidapi::HidDevice, tx: Sender<Rx>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 64];
    while !stop.load(Ordering::Relaxed) {
        if let Ok(n) = dev.read_timeout(&mut buf, 150) {
            if n > 0 {
                let _ = tx.send(Rx {
                    data: buf[..n].to_vec(),
                });
            }
        }
    }
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<LogiDriver> {
    let pid = info.pid;
    let h = open_handles(dev, pid)?;
    let stop = Arc::new(AtomicBool::new(false));
    let rx = spawn_reader_owned(h.short_r, h.long_r, stop.clone());
    let d = LogiDriver {
        short: Mutex::new(h.short_w),
        long: Mutex::new(h.long_w),
        rx: Mutex::new(rx),
        stop,
        info: Mutex::new(info.clone()),
        events,
        dev: Mutex::new(None),
        awake: Mutex::new(false),
        feats: Mutex::new(Vec::new()),
        dpi_list: Mutex::new(Vec::new()),
        battery_cache: Mutex::new(None),
    };
    // 「戻る」ボタンのゲーム内 Esc 置換フック (冪等・アプリ生存期間で 1 本)。
    super::logi_remap::ensure_hook();
    d.detect();
    Ok(d)
}

impl Drop for LogiDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl LogiDriver {
    // ------------------------------------------------------------ 送受信

    /// long フレーム 1 通を送り、(rid, dev, feat, funcswid) が一致する応答を待つ。
    /// エラー応答 (0x8F, r[3]=feat エコー) も Some で返す。None = タイムアウト。
    fn tx(&self, dev_idx: u8, feat: u8, func: u8, payload: &[u8]) -> Option<Vec<u8>> {
        let funcswid = (func << 4) | SWID;
        {
            let long = self.long.lock().unwrap_or_else(|p| p.into_inner());
            let mut w = vec![0u8; 21];
            w[0] = LONG_RID;
            w[1] = dev_idx;
            w[2] = feat;
            w[3] = funcswid;
            let n = payload.len().min(16);
            w[4..4 + n].copy_from_slice(&payload[..n]);
            long.write(&w).ok()?;
        }
        self.wait(dev_idx, feat, funcswid, 500)
    }

    /// 応答待ち。0x8F エラー (r[3]=要求 feature エコー) も一致扱い。
    fn wait(&self, dev_idx: u8, feat: u8, funcswid: u8, ms: u64) -> Option<Vec<u8>> {
        let dl = Instant::now() + Duration::from_millis(ms);
        let rx = self.rx.lock().unwrap_or_else(|p| p.into_inner());
        while Instant::now() < dl {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Rx { data }) => {
                    if data.first() != Some(&LONG_RID) && data.first() != Some(&SHORT_RID) {
                        continue;
                    }
                    if data.get(1) != Some(&dev_idx) {
                        continue;
                    }
                    if data.get(2) == Some(&feat) && data.get(3) == Some(&funcswid) {
                        return Some(data);
                    }
                    if data.get(2) == Some(&0x8F) && data.get(3) == Some(&feat) {
                        return Some(data);
                    }
                }
                Err(_) => {}
            }
        }
        None
    }

    /// tx の成否を HubResult に変換する。エラー応答はコード別に整理。
    fn cmd(&self, dev_idx: u8, feat: u8, func: u8, payload: &[u8]) -> HubResult<Vec<u8>> {
        match self.tx(dev_idx, feat, func, payload) {
            None => Err(HubError::Protocol("HID++ 応答タイムアウト".into())),
            Some(r) if r.get(2) == Some(&0x8F) => {
                let code = r.get(5).copied().unwrap_or(0);
                Err(match code {
                    err::RESOURCE_ERROR | err::CONNECTION_REQUEST_FAILED => HubError::Disconnected,
                    _ => HubError::Protocol(format!("HID++ エラー {code:#04x}")),
                })
            }
            Some(r) => Ok(r),
        }
    }

    // ------------------------------------------------------------ 検出

    /// ペア機を ping で探し、起きていれば feature 一覧を取得する。
    /// スリープ中 (RESOURCE_ERROR) は index だけ覚えて asleep にする。
    fn detect(&self) {
        // 短い待ちで過去の迷子レポートを捨てる。
        let _ = self.drain(120);
        let mut awake_idx = None;
        let mut asleep_idx = None;
        for idx in 1..=6u8 {
            match self.ping(idx) {
                Ping::Awake => {
                    awake_idx = Some(idx);
                    break;
                }
                Ping::Asleep => {
                    if asleep_idx.is_none() {
                        asleep_idx = Some(idx);
                    }
                }
                Ping::Absent => {}
            }
        }
        let mut info = self.info.lock().unwrap_or_else(|p| p.into_inner());
        match awake_idx {
            Some(idx) => {
                *self.dev.lock().unwrap_or_else(|p| p.into_inner()) = Some(idx);
                *self.awake.lock().unwrap_or_else(|p| p.into_inner()) = true;
                info.asleep = false;
                self.enum_features(idx);
                // マウス機能 (DPI) が無ければキーボード等 → 未対応として名札だけ残す。
                let is_mouse =
                    self.feat_idx(0x2201).is_some() || self.feat_idx(0x2202).is_some();
                info.supported = is_mouse;
                if is_mouse {
                    if let Some(name) = self.device_name(idx) {
                        info.name = name;
                    }
                    let mut list = self.dpi_list.lock().unwrap_or_else(|p| p.into_inner());
                    *list = self.read_dpi_list(idx);
                }
            }
            None => {
                // 名前は書き換えない (registry の名前を残す。UI は asleep で状態を出す)。
                *self.dev.lock().unwrap_or_else(|p| p.into_inner()) = asleep_idx;
                *self.awake.lock().unwrap_or_else(|p| p.into_inner()) = false;
                info.asleep = asleep_idx.is_some();
                info.supported = asleep_idx.is_some();
            }
        }
    }

    /// ping 1 回の結果。
    fn ping(&self, idx: u8) -> Ping {
        match self.tx(idx, 0x00, 1, &[0, 0, 0xAA]) {
            Some(r) if r.get(2) != Some(&0x8F) && r.get(4).is_some_and(|v| *v >= 2) => {
                Ping::Awake
            }
            Some(r) if r.get(2) == Some(&0x8F) && r.get(5) == Some(&err::RESOURCE_ERROR) => {
                Ping::Asleep
            }
            _ => Ping::Absent,
        }
    }

    /// FeatureSet (0x0001) で (index, id) 一覧を作る。
    fn enum_features(&self, idx: u8) {
        let Ok(r) = self.cmd(idx, 0x01, 0, &[0, 0]) else {
            return;
        };
        let count = r.get(4).copied().unwrap_or(0) as u16;
        let mut feats = Vec::new();
        for i in 1..=count.min(255) {
            if let Ok(r) = self.cmd(idx, 0x01, 1, &[i as u8]) {
                let id = u16::from_be_bytes([
                    r.get(4).copied().unwrap_or(0),
                    r.get(5).copied().unwrap_or(0),
                ]);
                if id != 0 {
                    feats.push((i as u8, id));
                }
            }
        }
        *self.feats.lock().unwrap_or_else(|p| p.into_inner()) = feats;
    }

    /// feature id → feature index。
    fn feat_idx(&self, id: u16) -> Option<u8> {
        self.feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(_, fid)| *fid == id)
            .map(|(i, _)| *i)
    }

    /// DeviceName (0x0003)。
    /// FriendlyName (0x0005) — 16文字チャンク読み (実測: チャンクは payload[0..] に
    /// そのまま入る、param = 文字オフセット。0x0003 DeviceName は FW 風の別データを
    /// 返すため使わない)。
    fn device_name(&self, idx: u8) -> Option<String> {
        let fi = self.feat_idx(0x0005)?;
        let mut s = String::new();
        for chunk in 0..8u8 {
            let off = chunk * 16;
            let r = self.cmd(idx, fi, 1, &[off, 0, 0]).ok()?;
            let mut any = false;
            for &c in r.get(4..20)? {
                if c == 0 {
                    break;
                }
                s.push(c as char);
                any = true;
            }
            if !any {
                break;
            }
        }
        let s = s.trim_end().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// AdjustableDPI (0x2201) の対応 DPI 一覧。
    fn read_dpi_list(&self, idx: u8) -> Vec<u32> {
        let Some(fi) = self.feat_idx(0x2201) else {
            return Vec::new();
        };
        let mut bytes: Vec<u8> = Vec::new();
        for page in 0..16u8 {
            let Ok(r) = self.cmd(idx, fi, 1, &[0x00, 0x00, page]) else {
                break;
            };
            // 応答ペイロードは r[4..] (r[4]=エコー、r[5..]=DPI 値 u16BE 列)。
            let start = 5.min(r.len());
            bytes.extend_from_slice(&r[start..]);
            if bytes.len() >= 2 && bytes[bytes.len() - 2..] == [0, 0] {
                break;
            }
        }
        let mut list = Vec::new();
        let mut i = 0;
        while i + 2 <= bytes.len() {
            let val = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as u32;
            if val == 0 {
                break;
            }
            if val >> 13 == 0b111 {
                // 圧縮エントリ: [step は 13bit, 次の u16 = 終端] で範囲を埋める。
                let step = val & 0x1FFF;
                if i + 4 > bytes.len() || list.is_empty() {
                    break;
                }
                let last = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as u32;
                let mut v = list.last().copied().unwrap_or(0) + step;
                while v <= last {
                    list.push(v);
                    v += step;
                }
                i += 4;
            } else {
                list.push(val);
                i += 2;
            }
        }
        list
    }

    /// 迷子レポートを読み捨てる (ms)。
    fn drain(&self, ms: u64) -> usize {
        let dl = Instant::now() + Duration::from_millis(ms);
        let rx = self.rx.lock().unwrap_or_else(|p| p.into_inner());
        let mut n = 0;
        while Instant::now() < dl {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(_) => n += 1,
                Err(_) => break,
            }
        }
        n
    }

    fn dev_idx(&self) -> HubResult<u8> {
        self.dev
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ok_or(HubError::Disconnected)
    }

    /// リンクが生きていることを確認する。見つかっていない / 寝ているときは
    /// detect() で再検出し、復帰していれば Info + StateChanged を出す。
    fn ensure_awake(&self) -> HubResult<u8> {
        let was_up = self.dev_idx().is_ok()
            && *self.awake.lock().unwrap_or_else(|p| p.into_inner());
        if was_up {
            return self.dev_idx();
        }
        // 見つかっていない (dev None) / スリープ中 — ping が通るようなら全体を再検出。
        let reachable = match self.dev_idx() {
            Ok(idx) => self.ping(idx) == Ping::Awake,
            Err(_) => false,
        };
        if reachable || self.dev_idx().is_err() {
            self.detect();
        } else if let Ok(idx) = self.dev_idx() {
            // ペア機はいるが ping が通らない — ping が通った瞬間だけ再検出。
            if self.ping(idx) != Ping::Awake {
                return Err(HubError::Disconnected);
            }
            self.detect();
        }
        if self.dev_idx().is_err()
            || !*self.awake.lock().unwrap_or_else(|p| p.into_inner())
        {
            return Err(HubError::Disconnected);
        }
        let id = self.info.lock().unwrap_or_else(|p| p.into_inner()).id.clone();
        if !was_up {
            if let Some(ev) = &self.events {
                ev(HubEvent::Info(id.clone(), Box::new(self.info())));
                ev(HubEvent::StateChanged(id));
            }
        }
        self.dev_idx()
    }

    /// バッテリー取得 (0x1004 → 0x1000 フォールバック)。
    /// バッテリー取得。G903 HERO は BatteryLevelStatus (0x1001) のみ搭載
    /// (実測: func 0 → [level, next, state]、level を % として使う)。
    /// 0x1004 / 0x1000 搭載機はそちらを優先。
    fn read_battery(&self, idx: u8) -> HubResult<Battery> {
        if let Some(fi) = self.feat_idx(0x1004) {
            let r = self.cmd(idx, fi, 1, &[0, 0])?;
            let soc = r.get(4).copied().unwrap_or(0).min(100);
            let charging = r.get(6).copied().unwrap_or(0) == 1;
            let external = r.get(5).copied().unwrap_or(0) == 1;
            return Ok(Battery {
                percent: Some(soc),
                charging: charging || external,
            });
        }
        if let Some(fi) = self.feat_idx(0x1000) {
            let r = self.cmd(idx, fi, 0, &[0, 0])?;
            let level = r.get(4).copied().unwrap_or(0).min(100);
            let status = r.get(6).copied().unwrap_or(0);
            return Ok(Battery {
                percent: Some(level),
                charging: status & 0x20 != 0,
            });
        }
        if let Some(fi) = self.feat_idx(0x1001) {
            let r = self.cmd(idx, fi, 0, &[0, 0])?;
            let level = r.get(4).copied().unwrap_or(0).min(100);
            let state = r.get(6).copied().unwrap_or(0);
            if level == 0 {
                return Err(HubError::Protocol("battery level 0 (取得できず)".into()));
            }
            return Ok(Battery {
                percent: Some(level),
                charging: state & 0x21 != 0,
            });
        }
        Err(HubError::Unsupported("battery"))
    }

    /// ポーリングレート対応一覧 (Hz)。0x8061 ビットマップから。
    fn rate_options(&self, idx: u8) -> Vec<(u8, i64)> {
        let Some(fi) = self.feat_idx(0x8061) else {
            return Vec::new();
        };
        let Ok(r) = self.cmd(idx, fi, 0, &[0, 0]) else {
            return Vec::new();
        };
        let flags = r.get(4).copied().unwrap_or(0);
        // bit i = period (i+1) ms。
        (0..8)
            .filter(|i| flags >> i & 1 == 1)
            .map(|i| (i + 1, 1000 / (i as i64 + 1)))
            .collect()
    }
}

// ---------------------------------------------------------------- caps

impl LogiDriver {
    /// 検出結果から caps を組む。スリープ中は caps を取れないので
    /// G903 HERO 既定 (100–25600, 50 刻み) を仮置きする — 起きたら
    /// HubEvent::Info で UI に再読させる。
    fn caps_now(&self) -> Capabilities {
        let paired = self.dev.lock().unwrap_or_else(|p| p.into_inner()).is_some();
        let awake = *self.awake.lock().unwrap_or_else(|p| p.into_inner());
        let is_mouse = (!awake && paired)
            || (awake && (self.feat_idx(0x2201).is_some() || self.feat_idx(0x2202).is_some()));
        if !paired || !is_mouse {
            return Capabilities::empty();
        }
        let dpi_list = self.dpi_list.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (dpi_min, dpi_max, dpi_step) = match dpi_list.as_slice() {
            [] => (100, 25600, 50),
            l => (*l.first().unwrap(), *l.last().unwrap(), 50),
        };
        let mut caps = Capabilities::empty();
        caps.battery = true;
        caps.mouse = Some(MouseCaps {
            dpi_min,
            dpi_max,
            dpi_step,
            min_stages: 1,
            max_stages: 1,
            xy_split: false,
            stage_colors: false,
            stage_toggle: false,
            buttons: vec![],
            button_kinds: vec![],
            require_left_click: false,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        });
        caps
    }
}

// ---------------------------------------------------------------- DeviceDriver

impl DeviceDriver for LogiDriver {
    fn info(&self) -> DeviceInfo {
        // スリープから復帰していたら再検出。通らなければ asleep のまま返す。
        if !*self.awake.lock().unwrap_or_else(|p| p.into_inner()) {
            if self.ensure_awake().is_err() {
                let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
                i.asleep = true;
                i.supported = self.dev.lock().unwrap_or_else(|p| p.into_inner()).is_some();
                return i;
            }
        }
        let idx = match self.dev_idx() {
            Ok(v) => v,
            Err(_) => return self.info.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        };
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // 応答しなくなった = スリープに落ちた。
        if self.ping(idx) != Ping::Awake {
            *self.awake.lock().unwrap_or_else(|p| p.into_inner()) = false;
            i.asleep = true;
            i.supported = true;
            if let Some(ev) = &self.events {
                ev(HubEvent::Info(i.id.clone(), Box::new(i.clone())));
            }
            return i;
        }
        i.asleep = false;
        if let Some(fi) = self.feat_idx(0x0002) {
            if let Ok(r) = self.cmd(idx, fi, 0, &[0, 0]) {
                let count = r.get(4).copied().unwrap_or(0) as u8;
                for k in 0..count {
                    if let Ok(r) = self.cmd(idx, fi, 1, &[k]) {
                        let kind = r.get(4).copied().unwrap_or(0xFF) & 0x0F;
                        // 0/1 = Main/Boot firmware。
                        if (kind == 0 || kind == 1) && r.len() >= 12 {
                            let major = r[8];
                            let minor = r[9];
                            let build = u16::from_be_bytes([r[10], r[11]]);
                            let mut v = format!("{major:02X}.{minor:02X}");
                            if build != 0 {
                                v.push_str(&format!(".B{build:04X}"));
                            }
                            i.firmware = Some(v);
                            break;
                        }
                    }
                }
            }
        }
        if let Ok(b) = self.read_battery(idx) {
            i.battery = Some(b);
            *self.battery_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(b);
        }
        i
    }

    fn caps(&self) -> Capabilities {
        let mut caps = self.caps_now();
        // ポーリングレート: 0x8061 があれば選択肢を作る (スリープ中でも既定を出す)。
        if let Ok(idx) = self.dev_idx() {
            let opts = self.rate_options(idx);
            if !opts.is_empty() {
                caps.settings.push(SettingSpec::choice(
                    "polling_wireless",
                    section::PERFORMANCE,
                    "ポーリングレート",
                    opts
                        .iter()
                        .map(|&(_, hz)| ChoiceOpt::new(hz, format!("{hz}Hz")))
                        .collect(),
                ));
            }
        }
        // ゲーム中 戻るボタン → Esc (ホスト側リマップ)。マウス機能のある機種だけ出す。
        if caps.mouse.is_some() {
            caps.settings.push(
                SettingSpec::toggle(
                    "game_back_esc",
                    section::CLICK,
                    "「戻る」ボタンを Esc にする",
                )
                .help(concat!(
                    "ON の間は「戻る」ボタンが Esc として送られます。",
                    "通常は OFF にしておき、自動化 > プロセス連動でゲーム用プロファイル",
                    "(この設定 ON) をゲームの exe に割り当てると、ゲーム中だけ Esc になります",
                    " (旧 G Hub + AHK の F14 置換の代替)"
                )),
            );
        }
        caps
    }

    fn read(&self) -> HubResult<DeviceState> {
        let idx = self.ensure_awake()?;
        let mut settings = BTreeMap::new();

        // ゲーム中 戻る → Esc (ホスト側リマップ。デバイス通信は不要だが read の流れで埋める)。
        settings.insert(
            "game_back_esc".into(),
            SettingValue::Bool(super::logi_remap::get().game_back_esc),
        );

        // ポーリングレート (0x8061 func 1 → period ms)。
        if let Some(fi) = self.feat_idx(0x8061) {
            if let Ok(r) = self.cmd(idx, fi, 1, &[0]) {
                if let Some(ms) = r.get(4).copied() {
                    if (1..=8).contains(&ms) {
                        settings.insert(
                            "polling_wireless".into(),
                            SettingValue::Choice(1000 / ms as i64),
                        );
                    }
                }
            }
        }

        // DPI (0x2201 read fnid 0x20 → [pad, curHi, curLo, defHi, defLo])。
        let dpi_now = if let Some(fi) = self.feat_idx(0x2201) {
            self.cmd(idx, fi, 2, &[])
                .ok()
                .and_then(|r| {
                    let hi = r.get(5).copied()?;
                    let lo = r.get(6).copied()?;
                    Some(u16::from_be_bytes([hi, lo]) as u32)
                })
                .filter(|v| *v > 0)
        } else {
            None
        };

        let stages = match dpi_now {
            Some(dpi) => vec![DpiStage {
                x: dpi,
                y: None,
                color: Rgb::new(255, 255, 255),
                enabled: true,
            }],
            None => vec![],
        };

        let battery = self.read_battery(idx).ok();
        if battery.is_some() {
            *self.battery_cache.lock().unwrap_or_else(|p| p.into_inner()) = battery;
        }

        Ok(DeviceState {
            profile: ProfileState {
                active: 0,
                names: vec![],
            },
            settings,
            mouse: Some(MouseState {
                stages,
                active_stage: 0,
                buttons: vec![],
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        let idx = self.ensure_awake()?;
        match change {
            Change::Setting { id, value } if id == "game_back_esc" => {
                // ホスト側リマップ。デバイスへの書き込みは不要、フック設定だけ更新する。
                let mut cfg = super::logi_remap::get();
                cfg.game_back_esc = value.as_bool();
                super::logi_remap::set(cfg);
                Ok(())
            }
            Change::Setting { id, value } if id == "polling_wireless" => {
                let fi = self
                    .feat_idx(0x8061)
                    .ok_or(HubError::Unsupported("polling rate"))?;
                let hz = value.as_choice();
                let ms = match hz {
                    1000 => 1,
                    500 => 2,
                    333 => 3,
                    250 => 4,
                    200 => 5,
                    166 => 6,
                    142 => 7,
                    125 => 8,
                    _ => return Err(HubError::Invalid("polling rate".into())),
                };
                let allowed = self.rate_options(idx);
                if !allowed.iter().any(|&(_, h)| h == hz) {
                    return Err(HubError::Invalid("このデバイスでは選べないポーリングレートです".into()));
                }
                self.cmd(idx, fi, 2, &[ms])?;
                Ok(())
            }
            Change::DpiStages { stages, .. } => {
                let fi = self.feat_idx(0x2201).ok_or(HubError::Unsupported("dpi"))?;
                let dpi = stages
                    .first()
                    .map(|s| s.x)
                    .ok_or(HubError::Invalid("dpi stage".into()))?;
                let allowed = self.dpi_list.lock().unwrap_or_else(|p| p.into_inner());
                if !allowed.is_empty() && !allowed.contains(&dpi) {
                    return Err(HubError::Invalid("このデバイスでは選べない DPI です".into()));
                }
                // write fnid 0x30、prefix 0x00 + DPI (BE)。
                self.cmd(idx, fi, 3, &[0x00, (dpi >> 8) as u8, dpi as u8])?;
                Ok(())
            }
            Change::ActiveStage(_) => Ok(()), // 単一ステージ
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        let idx = self.ensure_awake()?;
        let b = self.read_battery(idx)?;
        *self.battery_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(b);
        Ok(b)
    }
}
