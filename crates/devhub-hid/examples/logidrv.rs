//! Logitech HID++ ドライバの直接検証 — detect / info / caps / read / apply。
//!
//!   cargo run -p devhub-hid --example logidrv
//!   (マウスがスリープ中なら "スリープ中" と出る。動かすと再検出する。)

use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use devhub_hid::logitech;
use std::time::Duration;

fn main() {
    let mut info = DeviceInfo {
        id: DeviceId("hid:046d:c53a:test".into()),
        kind: DeviceKind::Mouse,
        name: "G903 LIGHTSPEED".into(),
        vendor: "Logicool".into(),
        model: "logi-lightspeed-c53a".into(),
        vid: 0x046D,
        pid: 0xC53A,
        transport: Transport::Wireless24G,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Logitech,
        battery: None,
        supported: true,
        asleep: false,
        board_id: None,
    };
    info.serial = None;
    let Ok(api) = hidapi::HidApi::new() else {
        panic!("hidapi");
    };
    let Some(d) = api
        .device_list()
        .find(|d| {
            d.vendor_id() == 0x046D
                && d.product_id() == 0xC53A
                && d.usage_page() == 0xFF00
                && d.usage() == 0x0001
        })
        .and_then(|i| i.open_device(&api).ok())
    else {
        println!("C53A FF00:0001 が見つかりません");
        return;
    };
    let drv = match logitech::open(d, info, None) {
        Ok(d) => d,
        Err(e) => {
            println!("open err: {e}");
            return;
        }
    };
    for round in 0..5 {
        let info = drv.info();
        println!(
            "[{round}] name={:?} fw={:?} asleep={} supported={} battery={:?}",
            info.name, info.firmware, info.asleep, info.supported, info.battery
        );
        if info.supported && !info.asleep {
            let caps = drv.caps();
            println!("caps: settings={:?} dpi={:?}", 
                caps.settings.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
                caps.mouse.as_ref().map(|m| (m.dpi_min, m.dpi_max, m.dpi_step)));
            match drv.read() {
                Ok(st) => {
                    println!("read: settings={:?}", st.settings);
                    if let Some(m) = &st.mouse {
                        println!("read: stages={:?} active={}", 
                            m.stages.iter().map(|s| s.x).collect::<Vec<_>>(), m.active_stage);
                    }
                    // DPI 書き込み往復テスト: 800 にして読み戻し、1600 に戻す。
                    if let Some(m) = &st.mouse {
                        let orig = m.stages.first().map(|s| s.x).unwrap_or(1600);
                        for target in [800u32, orig] {
                            let ch = Change::DpiStages {
                                stages: vec![DpiStage {
                                    x: target,
                                    y: None,
                                    color: Rgb::new(255, 255, 255),
                                    enabled: true,
                                }],
                                active: 0,
                            };
                            match drv.apply(&ch) {
                                Ok(()) => println!("apply {target}: ok"),
                                Err(e) => println!("apply {target}: {e}"),
                            }
                            std::thread::sleep(Duration::from_millis(300));
                            match drv.read() {
                                Ok(st2) => println!(
                                    "  -> read back: {:?}",
                                    st2.mouse.as_ref().map(|m| m.stages.iter().map(|s| s.x).collect::<Vec<_>>())
                                ),
                                Err(e) => println!("  -> read err: {e}"),
                            }
                        }
                    }
                }
                Err(e) => println!("read err: {e}"),
            }
            return;
        }
        println!("  (マウスを動かすと再検出します — 3 秒待機)");
        std::thread::sleep(Duration::from_millis(3000));
    }
    println!("5 回試して起きませんでした");
}
