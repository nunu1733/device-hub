use hidapi::HidApi;
fn main() {
    let api = HidApi::new().unwrap();
    let mut targets: Vec<(String, hidapi::HidDevice)> = Vec::new();
    for d in api.device_list() {
        // レシーバーのマウス (COL01) とキーボード IF をサンプル
        if d.vendor_id() == 0x046D
            && (d.product_id() == 0xC53A || d.product_id() == 0xC53D)
            && d.usage_page() == 0x0001
            && (d.usage() == 0x0002 || d.usage() == 0x0006)
            && d.interface_number() == 1
        {
            if let Ok(dev) = d.open_device(&api) {
                targets.push((format!("{:04x} if{} u{:04x}", d.product_id(), d.interface_number(), d.usage()), dev));
            }
        }
    }
    println!("sampling {} interfaces for 6s (マウスを動かしてみてください)", targets.len());
    let start = std::time::Instant::now();
    let mut counts = vec![0usize; targets.len()];
    while start.elapsed().as_secs() < 6 {
        for (i, (_t, dev)) in targets.iter().enumerate() {
            let mut buf = [0u8; 64];
            if let Ok(n) = dev.read_timeout(&mut buf, 20) {
                if n > 0 {
                    counts[i] += 1;
                }
            }
        }
    }
    for (i, (t, _)) in targets.iter().enumerate() {
        println!("{t}: {} reports", counts[i]);
    }
}
