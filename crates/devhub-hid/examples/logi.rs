//! Logicool (VID 046D) HID++ 2.0 プローブ — G903 解析用。
//!
//! 使い方:
//!   cargo run -p devhub-hid --example logi            # インターフェース列挙
//!   cargo run -p devhub-hid --example logi -- ping    # レシーバー/ペア機 ping
//!   cargo run -p devhub-hid --example logi -- feats <devidx> [pid]     # feature 一覧
//!   cargo run -p devhub-hid --example logi -- detail <devidx> [pid]    # 既知 feature の値
//!   cargo run -p devhub-hid --example logi -- pairinfo                 # 0x83 ペアリング情報
//!
//! HID++ 2.0 フレーム:
//!   short [0x10, dev, feat, func<<4|swid, p0..p2]
//!   long  [0x11, dev, feat, func<<4|swid, p0..p15]
//! 応答はレポート種別を保ってエコー。エラーは r[2]=0x8F で r[3]=要求 feature
//! エコー、r[5]=コード (1=UNKNOWN 8=BUSY 9=UNSUPPORTED — Solaar ErrorCode と同じ)。
//!
//! Windows HID の読み取り取りこぼし (短 timeout の連続 read で報告が捨てられる)
//! を避けるため、全 FF00 ハンドルに常駐リーダースレッドを置いてチャネルで集める。

use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
const SWID: u8 = 13;

fn feat_name(id: u16) -> &'static str {
    match id {
        0x0000 => "Root",
        0x0001 => "FeatureSet",
        0x0002 => "DeviceInfo",
        0x0003 => "DeviceName",
        0x0004 => "WirelessDevStatus",
        0x0005 => "FriendlyName",
        0x0006 => "HostsInfo",
        0x1000 => "BatteryStatus",
        0x1001 => "BatteryLevelStatus",
        0x1002 => "BatteryVoltage",
        0x1004 => "UnifiedBattery",
        0x2200 => "MousePointerBasic",
        0x2201 => "AdjustableDPI",
        0x2203 => "SmartShift",
        0x4300 => "MouseButtonSpy",
        0x4501 => "ReprogControlsV1",
        0x4502 => "ReprogControlsV2",
        0x4511 => "ReprogControlsV4",
        0x8060 => "MultiPlatform",
        0x8061 => "AdjustableReportRate",
        0x8062 => "NewReportRate",
        0x8063 => "UsbMaxReportRate",
        0x8070 => "ColorLedEffects",
        0x8071 => "RgbEffects",
        0x8072 => "RgbZones",
        0x8100 => "OnboardProfiles",
        0x8110 => "ProfileRecording",
        0x8400 => "GameMode",
        0x8500 => "Profiles",
        _ => "",
    }
}

fn err_name(c: u8) -> &'static str {
    // Solaar hidpp10 ErrorCode (実機の 0x8F 応答は 1.0 形式)。
    match c {
        0x01 => "INVALID_SUB_ID_COMMAND",
        0x02 => "INVALID_ADDRESS",
        0x03 => "INVALID_VALUE",
        0x04 => "CONNECTION_REQUEST_FAILED",
        0x07 => "BUSY",
        0x08 => "UNKNOWN_DEVICE (未ペア)",
        0x09 => "RESOURCE_ERROR (スリープ/到達不可)",
        0x0A => "REQUEST_UNAVAILABLE",
        _ => "?",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let api = HidApi::new().unwrap();
    // (pid, ifno, short path, long path)。同一 IF の 2 コレクションを束ねる。
    let mut paths: Vec<(u16, i32, String, Option<String>)> = Vec::new();
    for d in api.device_list() {
        if d.vendor_id() != VID || (d.product_id() != 0xC53A && d.product_id() != 0xC53D) {
            continue;
        }
        if d.usage_page() != 0xFF00 {
            continue;
        }
        println!(
            "[if] {:04x}:{} up={:04x} u={:04x} {}",
            d.product_id(),
            d.interface_number(),
            d.usage_page(),
            d.usage(),
            d.product_string().unwrap_or("")
        );
        let path = d.path().to_string_lossy().into_owned();
        match paths
            .iter()
            .position(|e| e.0 == d.product_id() && e.1 == d.interface_number())
        {
            Some(i) => paths[i].3 = Some(path), // 2つ目のコレクション (u0002) = long
            None => paths.push((d.product_id(), d.interface_number(), path, None)),
        }
    }
    let mut ff: Vec<(u16, i32, HidDevice, HidDevice)> = Vec::new();
    for (pid, ifno, sp, lp) in &paths {
        let s = api.open_path(std::ffi::CString::new(sp.clone()).unwrap().as_c_str());
        let l = match lp {
            Some(ref p) => api.open_path(std::ffi::CString::new(p.clone()).unwrap().as_c_str()),
            None => api.open_path(std::ffi::CString::new(sp.clone()).unwrap().as_c_str()),
        };
        if let (Ok(s), Ok(l)) = (s, l) {
            ff.push((*pid, *ifno, s, l));
        }
    }

    // 常駐リーダースレッド (short/long 両方)。報告は全部チャネルへ。
    let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
    let mut readers = 0;
    let mut reader_devs: Vec<(String, HidDevice)> = Vec::new();
    // 書き込み用ハンドル (ff) は残しつつ、読み取り専用ハンドルを別途開く。
    for (pid, ifno, sp, lp) in &paths {
        reader_devs.push((
            format!("{pid:04x}:{ifno}/S"),
            api.open_path(std::ffi::CString::new(sp.clone()).unwrap().as_c_str()).unwrap(),
        ));
        if let Some(p) = lp {
            reader_devs.push((
                format!("{pid:04x}:{ifno}/L"),
                api.open_path(std::ffi::CString::new(p.clone()).unwrap().as_c_str()).unwrap(),
            ));
        }
    }
    for (tag, dev) in reader_devs {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                if let Ok(n) = dev.read_timeout(&mut buf, 100) {
                    if n > 0 {
                        let _ = tx.send((tag.clone(), buf[..n].to_vec()));
                    }
                }
            }
        });
        readers += 1;
    }
    println!("-- readers {readers} --");

    let Some(cmd) = args.first() else { return };
    let short_dev =
        |pid: u16| -> Option<&HidDevice> { ff.iter().find(|e| e.0 == pid).map(|e| &e.2) };
    let long_dev =
        |pid: u16| -> Option<&HidDevice> { ff.iter().find(|e| e.0 == pid).map(|e| &e.3) };

    match cmd.as_str() {
        "ping" => {
            for pid in [0xC53Au16, 0xC53D] {
                println!("== {pid:04x} ==");
                let Some(dev) = short_dev(pid) else { continue };
                for idx in [0xFFu8, 1, 2, 3, 4, 5, 6] {
                    let mut w = [0u8; 8];
                    w[0] = 0x10;
                    w[1] = idx;
                    w[2] = 0x00;
                    w[3] = 0x11;
                    w[6] = 0xAA;
                    dev.write(&w).ok();
                    let mut shown = false;
                    let dl = Instant::now() + Duration::from_millis(400);
                    while Instant::now() < dl {
                        if let Ok((_tag, r)) = rx.recv_timeout(Duration::from_millis(60)) {
                            // ping 応答は long (0x11) で返ることもある。
                            let rid_ok = r.first() == Some(&0x10) || r.first() == Some(&0x11);
                            if !rid_ok || r.get(1) != Some(&idx) {
                                continue;
                            }
                            if r.get(2) == Some(&0x8F) {
                                println!("  dev {idx:#04x} ERR {:#04x} ({})", r[5], err_name(r[5]));
                            } else {
                                println!(
                                    "  dev {idx:#04x} proto {}.{} echo={:02x}",
                                    r.get(4).unwrap_or(&0),
                                    r.get(5).unwrap_or(&0),
                                    r.get(6).unwrap_or(&0)
                                );
                            }
                            shown = true;
                            break;
                        }
                    }
                    if !shown {
                        println!("  dev {idx:#04x}: timeout");
                    }
                }
            }
        }
        "feats" => {
            let idx: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let pid_arg = parse_pid(args.get(2));
            for pid in [0xC53Au16, 0xC53D] {
                if let Some(p) = pid_arg {
                    if p != pid {
                        continue;
                    }
                }
                let Some(dev) = short_dev(pid) else { continue };
                println!("== {pid:04x} dev{idx} features ==");
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = idx;
                w[2] = 0x01;
                w[3] = 0x01;
                if dev.write(&w).is_err() {
                    println!("write err");
                    continue;
                }
                match wait_match(&rx, 0x10, idx, 0x01, 0x01, 500) {
                    None => println!("  FeatureSet count: timeout"),
                    Some(r) if r.get(2) == Some(&0x8F) => {
                        println!("  FeatureSet ERR {:#04x} ({})", r[5], err_name(r[5]))
                    }
                    Some(r) => {
                        let count = r.get(4).copied().unwrap_or(0);
                        println!("  feature count = {count}");
                        for i in 1..=count.min(255) {
                            let mut w = [0u8; 8];
                            w[0] = 0x10;
                            w[1] = idx;
                            w[2] = 0x01;
                            w[3] = 0x11;
                            w[4] = i as u8;
                            if dev.write(&w).is_err() {
                                break;
                            }
                            match wait_match(&rx, 0x10, idx, 0x01, 0x11, 400) {
                                Some(r) if r.get(2) == Some(&0x8F) => {
                                    println!("  [{i:3}] ERR {:#04x}", r[5])
                                }
                                Some(r) => {
                                    let id = u16::from_be_bytes([
                                        r.get(4).copied().unwrap_or(0),
                                        r.get(5).copied().unwrap_or(0),
                                    ]);
                                    println!("  [{i:3}] {id:#06x} {}", feat_name(id));
                                }
                                None => println!("  [{i:3}] timeout"),
                            }
                        }
                    }
                }
            }
        }
        "detail" => {
            let idx: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let pid_arg = parse_pid(args.get(2));
            for pid in [0xC53Au16, 0xC53D] {
                if let Some(p) = pid_arg {
                    if p != pid {
                        continue;
                    }
                }
                let Some(dev) = short_dev(pid) else { continue };
                println!("== {pid:04x} dev{idx} detail ==");
                // feature id -> index (Root GetFeature)
                let fid = |id: u16| -> Option<u8> {
                    let mut w = [0u8; 8];
                    w[0] = 0x10;
                    w[1] = idx;
                    w[2] = 0x00;
                    w[3] = 0x01;
                    // Root.GetFeature の feature ID はビッグエンディアン (Solaar と同じ)。
                    w[4] = (id >> 8) as u8;
                    w[5] = id as u8;
                    dev.write(&w).ok()?;
                    match wait_match(&rx, 0x10, idx, 0x00, 0x01, 400) {
                        Some(r) if r.get(2) != Some(&0x8F) => {
                            let i = r.get(4).copied().unwrap_or(0);
                            (i != 0).then_some(i)
                        }
                        _ => None,
                    }
                };
                let send = |feat: u8, func: u8, p0: u8, p1: u8, p2: u8| -> Option<Vec<u8>> {
                    let mut w = [0u8; 8];
                    w[0] = 0x10;
                    w[1] = idx;
                    w[2] = feat;
                    w[3] = 0x10 | SWID | (func << 4);
                    w[4] = p0;
                    w[5] = p1;
                    w[6] = p2;
                    dev.write(&w).ok()?;
                    wait_match(&rx, 0x10, idx, feat, 0x10 | SWID | (func << 4), 400)
                };
                for id in [
                    0x0003u16, 0x0005, 0x2200, 0x2201, 0x8061, 0x8062, 0x8063, 0x1000, 0x1001,
                    0x1004, 0x8100, 0x8071, 0x8070, 0x4511, 0x4502, 0x0002, 0x0004,
                ] {
                    let Some(fi) = fid(id) else { continue };
                    print!("{id:#06x}({fi:#04x}) {}:", feat_name(id));
                    for func in 0..4u8 {
                        match send(fi, func, 0, 0, 0) {
                            Some(r) if r.get(2) == Some(&0x8F) => {
                                print!("  f{func}=ERR{:#04x}", r[5])
                            }
                            Some(r) => print!("  f{func}={:02x?}", &r[4..]),
                            None => print!("  f{func}=-"),
                        }
                    }
                    println!();
                }
            }
        }
        "pairinfo" => {
            for pid in [0xC53Au16, 0xC53D] {
                println!("== {pid:04x} pairing info (long 0x83) ==");
                let Some(dev) = long_dev(pid) else { continue };
                // long get-register 0x83: [rid, dev, 0x83, レジスタ下位バイト, サブレジスタ…]
                // レジスタ 0x2B5 (RECEIVER_INFO) = w[3]=0xB5、w[4]=サブレジスタ。
                for &sub in [0x01u8, 0x02, 0x03, 0x20, 0x21, 0x22, 0x30, 0x31, 0x40].iter() {
                    let mut w = vec![0u8; 21];
                    w[0] = 0x11;
                    w[1] = 0xFF;
                    w[2] = 0x83;
                    w[3] = 0xB5;
                    w[4] = sub;
                    dev.write(&w).ok();
                    let dl = Instant::now() + Duration::from_millis(400);
                    let mut shown = false;
                    while Instant::now() < dl {
                        if let Ok((tag, r)) = rx.recv_timeout(Duration::from_millis(50)) {
                            println!("  sub {sub:#04x} via {tag}: {:02x?}", r);
                            shown = true;
                            break;
                        }
                    }
                    if !shown {
                        println!("  sub {sub:#04x}: timeout");
                    }
                }
            }
        }
        "f2loop" => {
            // GetDpi (0x0b func2) を連送し、feat=0x0b の全着信を生ダンプする。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let ldev = long_dev(pid);
            let Some(ld) = ldev else { return };
            let start = Instant::now();
            let mut n = 0;
            while start.elapsed() < Duration::from_secs(6) {
                let mut w = vec![0u8; 21];
                w[0] = 0x11;
                w[1] = 0x01;
                w[2] = 0x0B;
                w[3] = 0x20 | SWID;
                ld.write(&w).ok();
                n += 1;
                std::thread::sleep(Duration::from_millis(300));
                let dl = Instant::now() + Duration::from_millis(300);
                while Instant::now() < dl {
                    if let Ok((_tag, r)) = rx.recv_timeout(Duration::from_millis(50)) {
                        if r.get(2) == Some(&0x0B) {
                            println!("[{:>5.1}] rx {:02x?}", start.elapsed().as_secs_f32(), r);
                        }
                    }
                }
            }
            println!("sent {n} requests, done");
        }
        "f2" => {
            // Root.GetFeature を生送信し、全着信をそのまま表示する。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            for id in [0x2201u16, 0x0003, 0x1001] {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x01;
                w[4] = id as u8;
                w[5] = (id >> 8) as u8;
                dev.write(&w).ok();
                let dl = Instant::now() + Duration::from_millis(700);
                let mut got = false;
                while Instant::now() < dl {
                    if let Ok((tag, r)) = rx.recv_timeout(Duration::from_millis(80)) {
                        println!("GetFeature({id:#06x}) via {tag}: {:02x?}", r);
                        got = true;
                    }
                }
                if !got {
                    println!("GetFeature({id:#06x}): nothing in 700ms");
                }
            }
        }
        "sniff" => {
            // 全着信をそのままダンプ (G Hub の通信を観察する用)。
            let secs: u64 = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(secs) {
                if let Ok((_tag, r)) = rx.recv_timeout(Duration::from_millis(200)) {
                    println!("[{:>6.1}] {:02x?}", start.elapsed().as_secs_f32(), r);
                }
            }
        }
        "bat30" => {
            // 15 分監視 → 起きたら読み取り系の追加調査 (書き込みなし)。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            let ldev = long_dev(pid);
            let start = Instant::now();
            let mut awake = false;
            while start.elapsed() < Duration::from_secs(900) {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x11;
                w[6] = 0xAA;
                dev.write(&w).ok();
                if let Some(r) = wait_match(&rx, 0x10, 1, 0x00, 0x11, 1500) {
                    if r.get(2) != Some(&0x8F) {
                        awake = true;
                        break;
                    }
                }
            }
            if !awake {
                println!("15分間でスリープ解除されませんでした");
                return;
            }
            println!("-- AWAKE at {}s --", start.elapsed().as_secs_f32());
            let send = move |feat: u8, func: u8, p: [u8; 3], label: &str| {
                let mut w = vec![0u8; 21];
                w[0] = 0x11;
                w[1] = 0x01;
                w[2] = feat;
                w[3] = 0x10 | SWID | (func << 4);
                w[4] = p[0];
                w[5] = p[1];
                w[6] = p[2];
                let Some(ld) = ldev else {
                    println!("{label}: no long handle");
                    return;
                };
                if ld.write(&w).is_err() {
                    println!("{label}: write err");
                    return;
                }
                match wait_match(&rx, 0x10, 1, feat, 0x10 | SWID | (func << 4), 350) {
                    Some(r) => println!("{label}: {:02x?}", &r[4..]),
                    None => println!("{label}: -"),
                }
            };
            // 電池 0x1001 (feat idx 6) を3回 + func2
            for i in 0..3 {
                send(0x06, 0, [0, 0, 0], &format!("BAT try{i} f0"));
            }
            send(0x06, 2, [0, 0, 0], "BAT f2");
            // OnboardProfiles (idx 8): プロファイルデータ読みのパラメータ探し
            send(0x08, 0, [0, 0, 0], "PROF f0 [0,0,0]");
            send(0x08, 0, [1, 0, 0], "PROF f0 [1,0,0]");
            send(0x08, 1, [1, 0, 0], "PROF f1 [1,0,0]");
            send(0x08, 1, [1, 1, 0], "PROF f1 [1,1,0]");
            send(0x08, 2, [1, 0, 0], "PROF f2 [1,0,0]");
            send(0x08, 3, [1, 0, 0], "PROF f3 [1,0,0]");
            send(0x08, 3, [0, 0, 0], "PROF f3 [0,0,0]");
            // GetDpi (idx 0x0b) func2 のパラメータ違い
            send(0x0B, 2, [0, 0, 0], "DPI f2 [0,0,0]");
            send(0x0B, 2, [0, 0, 1], "DPI f2 [0,0,1]");
            send(0x0B, 2, [1, 0, 0], "DPI f2 [1,0,0]");
            send(0x0B, 3, [0, 0, 0], "DPI f3 [0,0,0]");
            // RGB (idx 7) のパラメータ違い
            send(0x07, 0, [1, 0, 0], "RGB f0 [1,0,0]");
            send(0x07, 1, [1, 0, 0], "RGB f1 [1,0,0]");
            send(0x07, 2, [1, 0, 0], "RGB f2 [1,0,0]");
            println!("-- done --");
        }
        "ghub" => {
            // G Hub の観測シーケンスをなぞって GetDpi が通るか検証。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            let ldev = long_dev(pid);
            // G Hub と同じく全呼び出しの間に ping を挟む (keep-alive)。
            let ping = |chk: u8| {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x1d;
                w[4] = 0x04;
                w[5] = 0x02;
                w[6] = chk;
                dev.write(&w).ok();
                wait_match(&rx, 0x10, 1, 0x00, 0x1d, 200);
            };
            let send = |feat: u8, func: u8, p: [u8; 3], label: &str| {
                ping((label.len() * 7 & 0xff) as u8);
                let Some(ld) = ldev else { return };
                let mut w = vec![0u8; 21];
                w[0] = 0x11;
                w[1] = 0x01;
                w[2] = feat;
                w[3] = 0x10 | SWID | (func << 4);
                w[4] = p[0];
                w[5] = p[1];
                w[6] = p[2];
                if ld.write(&w).is_err() {
                    println!("{label}: write err");
                    return;
                }
                match wait_match(&rx, 0x11, 1, feat, 0x10 | SWID | (func << 4), 400) {
                    Some(r) => println!("{label}: {:02x?}", &r[4..]),
                    None => println!("{label}: -"),
                }
            };
            // ping (G Hub は protocol 4.2 エコー f7)
            {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x1d;
                w[6] = 0xf7;
                dev.write(&w).ok();
                if let Some(r) = wait_match(&rx, 0x10, 1, 0x00, 0x1d, 400) {
                    println!("ping: {:02x?}", &r[4..]);
                }
            }
            // FeatureSet 全列挙
            send(0x01, 0, [0, 0, 0], "FS count");
            for i in 1u8..=30 {
                send(0x01, 1, [i, 0, 0], &format!("FS [{i}]"));
            }
            // OnboardProfiles
            send(0x08, 0, [0, 0, 0], "PROF f0");
            send(0x08, 1, [1, 0, 0], "PROF f1 [1]");
            send(0x08, 2, [0, 0, 0], "PROF f2");
            send(0x08, 4, [0, 0, 0], "PROF f4");
            for pg in [0u8, 1, 2, 3] {
                send(0x08, 5, [pg, 0, 0], &format!("PROF f5 pg{pg}"));
            }
            // ProfileRecording
            send(0x09, 0, [0, 0, 0], "PREC f0");
            send(0x09, 3, [0, 0, 0], "PREC f3");
            // RGB
            for (func, p) in [
                (0u8, [0xffu8, 0, 2]),
                (0, [0, 0, 0]),
                (0, [0, 1, 0]),
                (0, [0, 2, 0]),
                (0, [0, 3, 0]),
                (7, [0, 0, 0]),
                (3, [0, 0, 0]),
                (8, [0, 0, 0]),
            ] {
                send(0x07, func, p, &format!("RGB f{func} {p:?}"));
            }
            // MultiPlatform
            send(0x0A, 0, [0, 0, 0], "MP f0");
            send(0x0A, 1, [0, 0, 0], "MP f1");
            // DPI
            send(0x0B, 1, [0, 0, 0], "DPI f1");
            send(0x0B, 2, [0, 0, 0], "DPI f2");
            send(0x0B, 2, [0, 0, 0], "DPI f2 retry");
            println!("-- done --");
        }
        "all30" => {
            // 15 分監視 → 目覚めたら主要 feature の値を高速取得して終了。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            println!("== {pid:04x} dev1 all30 (15分監視) ==");
            let start = Instant::now();
            let mut awake = false;
            while start.elapsed() < Duration::from_secs(900) {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x11;
                w[6] = 0xAA;
                dev.write(&w).ok();
                if let Some(r) = wait_match(&rx, 0x10, 1, 0x00, 0x11, 1500) {
                    if r.get(2) != Some(&0x8F) {
                        awake = true;
                        break;
                    }
                }
            }
            if !awake {
                println!("15分間でスリープ解除されませんでした");
                return;
            }
            println!("-- AWAKE at {}s --", start.elapsed().as_secs_f32());
            // HID++ 2.0 feature 呼びは LONG フレームで LONG ハンドルに書く。
            // 起床直後の DPI 問い合わせは応答しないことがあるので少し待つ。
            std::thread::sleep(Duration::from_millis(2500));
            let ldev = long_dev(pid);
            let send = move |feat: u8, func: u8, p: [u8; 3]| -> Option<Vec<u8>> {
                let mut w = vec![0u8; 21];
                w[0] = 0x11;
                w[1] = 0x01;
                w[2] = feat;
                w[3] = 0x10 | SWID | (func << 4);
                w[4] = p[0];
                w[5] = p[1];
                w[6] = p[2];
                ldev?.write(&w).ok()?;
                wait_match(&rx, 0x10, 1, feat, 0x10 | SWID | (func << 4), 300)
            };
            // 0x2201 DPI: list(func1), now(func2) — f2 は応答しないことが多いので再試行。
            for (func, p, tag) in [
                (1u8, [0u8, 0, 0], "f1"),
                (2, [0, 0, 0], "f2a"),
                (2, [0, 0, 0], "f2b"),
                (2, [0, 0, 0], "f2c"),
            ] {
                if let Some(r) = send(0x0B, func, p) {
                    println!("DPI {tag}: {:02x?}", &r[4..]);
                } else {
                    println!("DPI {tag}: -");
                }
            }
            // 0x1001 battery func0
            for func in [0u8, 1] {
                if let Some(r) = send(0x06, func, [0, 0, 0]) {
                    println!("BAT f{func}: {:02x?}", &r[4..]);
                } else {
                    println!("BAT f{func}: -");
                }
            }
            // 0x0005 friendly name
            if let Some(r) = send(0x03, 0, [0, 0, 0]) {
                println!("FNAME f0: {:02x?}", &r[4..]);
            }
            if let Some(r) = send(0x03, 1, [0, 0, 0]) {
                println!("FNAME f1: {:02x?}", &r[4..]);
            }
            // 0x8071 RGB funcs 0-2
            for func in [0u8, 1, 2] {
                if let Some(r) = send(0x07, func, [0, 0, 0]) {
                    println!("RGB f{func}: {:02x?}", &r[4..]);
                } else {
                    println!("RGB f{func}: -");
                }
            }
            // 0x8100 onboard profiles funcs 0-4
            for func in [0u8, 1, 2, 3, 4] {
                if let Some(r) = send(0x08, func, [0, 0, 0]) {
                    println!("PROF f{func}: {:02x?}", &r[4..]);
                } else {
                    println!("PROF f{func}: -");
                }
            }
            // 0x8110 profile recording func 0
            if let Some(r) = send(0x09, 0, [0, 0, 0]) {
                println!("PREC f0: {:02x?}", &r[4..]);
            }
            println!("-- done ({}s 経過) --", start.elapsed().as_secs_f32());
        }
        "wake30" => {
            // 30 分監視: 目覚めたら feature 一覧を取得して終了する。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            println!("== {pid:04x} dev1 wake30 (30分監視) ==");
            let start = Instant::now();
            let mut last: Option<Vec<u8>> = None;
            let mut awake = false;
            while start.elapsed() < Duration::from_secs(1800) {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x11;
                w[6] = 0xAA;
                dev.write(&w).ok();
                let dl = Instant::now() + Duration::from_millis(1800);
                while Instant::now() < dl {
                    if let Ok((_tag, r)) = rx.recv_timeout(Duration::from_millis(300)) {
                        if r.first() == Some(&0x10) && r.get(1) == Some(&1u8) {
                            if last.as_deref() != Some(r.as_slice()) {
                                println!("[{:>6.0}s] {:02x?}", start.elapsed().as_secs_f32(), r);
                                last = Some(r.clone());
                            }
                            if r.get(2) != Some(&0x8F) {
                                awake = true;
                            }
                            break;
                        }
                    }
                }
                if awake {
                    println!("-- AWAKE! feature 一覧 --");
                    let mut w = [0u8; 8];
                    w[0] = 0x10;
                    w[1] = 1;
                    w[2] = 0x01;
                    w[3] = 0x01;
                    dev.write(&w).ok();
                    if let Some(r) = wait_match(&rx, 0x10, 1, 0x01, 0x01, 800) {
                        let count = r.get(4).copied().unwrap_or(0);
                        println!("feature count = {count}");
                        for i in 1..=count.min(255) {
                            let mut w = [0u8; 8];
                            w[0] = 0x10;
                            w[1] = 1;
                            w[2] = 0x01;
                            w[3] = 0x11;
                            w[4] = i as u8;
                            dev.write(&w).ok();
                            if let Some(r) = wait_match(&rx, 0x10, 1, 0x01, 0x11, 500) {
                                let id = u16::from_be_bytes([
                                    r.get(4).copied().unwrap_or(0),
                                    r.get(5).copied().unwrap_or(0),
                                ]);
                                println!("  [{i:3}] {id:#06x} {}", feat_name(id));
                            }
                        }
                    }
                    break;
                }
            }
            if !awake {
                println!("30分間でスリープ解除されませんでした");
            }
        }
        "wake" => {
            // dev1 へ 60 秒連続 ping。スリープ解除 (2.0 応答) を待つ。
            let pid = parse_pid(args.get(2)).unwrap_or(0xC53A);
            let Some(dev) = short_dev(pid) else { return };
            println!("== {pid:04x} dev1 wake loop (60s) ==");
            let start = Instant::now();
            let mut last: Option<Vec<u8>> = None;
            let mut awake = false;
            while start.elapsed() < Duration::from_secs(60) {
                let mut w = [0u8; 8];
                w[0] = 0x10;
                w[1] = 0x01;
                w[2] = 0x00;
                w[3] = 0x11;
                w[6] = 0xAA;
                dev.write(&w).ok();
                let dl = Instant::now() + Duration::from_millis(300);
                while Instant::now() < dl {
                    if let Ok((_tag, r)) = rx.recv_timeout(Duration::from_millis(60)) {
                        if r.first() == Some(&0x10) && r.get(1) == Some(&1u8) {
                            if last.as_deref() != Some(r.as_slice()) {
                                println!("[{:>5.1}s] {:02x?}", start.elapsed().as_secs_f32(), r);
                                last = Some(r.clone());
                            }
                            if r.get(2) != Some(&0x8F) {
                                awake = true;
                            }
                            break;
                        }
                    }
                }
                if awake {
                    println!("-- AWAKE! feature 一覧を取得します --");
                    // ping が通ったので FeatureSet を読む
                    let mut w = [0u8; 8];
                    w[0] = 0x10;
                    w[1] = 1;
                    w[2] = 0x01;
                    w[3] = 0x01;
                    dev.write(&w).ok();
                    if let Some(r) = wait_match(&rx, 0x10, 1, 0x01, 0x01, 500) {
                        let count = r.get(4).copied().unwrap_or(0);
                        println!("feature count = {count}");
                        for i in 1..=count.min(255) {
                            let mut w = [0u8; 8];
                            w[0] = 0x10;
                            w[1] = 1;
                            w[2] = 0x01;
                            w[3] = 0x11;
                            w[4] = i as u8;
                            dev.write(&w).ok();
                            if let Some(r) = wait_match(&rx, 0x10, 1, 0x01, 0x11, 400) {
                                let id = u16::from_be_bytes([
                                    r.get(4).copied().unwrap_or(0),
                                    r.get(5).copied().unwrap_or(0),
                                ]);
                                println!("  [{i:3}] {id:#06x} {}", feat_name(id));
                            }
                        }
                    }
                    break;
                }
            }
            if !awake {
                println!("60秒間でスリープ解除されませんでした");
            }
        }
        other => println!("unknown: {other}"),
    }
}

fn parse_pid(s: Option<&String>) -> Option<u16> {
    s.and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

/// リーダーチャネルから (rid, dev, feat, funcswid) が一致する応答を待つ。
/// 0x8F エラー (r[3]=要求 feature エコー) も一致扱い。
fn wait_match(
    rx: &std::sync::mpsc::Receiver<(String, Vec<u8>)>,
    rid: u8,
    dev: u8,
    feat: u8,
    funcswid: u8,
    ms: u64,
) -> Option<Vec<u8>> {
    let dl = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < dl {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((_tag, r)) => {
                // 応答レポート種別は要求と異なることがある (short→long など)。
                let rid_ok = r.first() == Some(&rid) || r.first() == Some(&0x11) || r.first() == Some(&0x10);
                if rid_ok && r.get(1) == Some(&dev) {
                    if r.get(2) == Some(&feat) && r.get(3) == Some(&funcswid) {
                        return Some(r);
                    }
                    if r.get(2) == Some(&0x8F) && r.get(3) == Some(&feat) {
                        return Some(r);
                    }
                }
            }
            Err(_) => {}
        }
    }
    None
}
