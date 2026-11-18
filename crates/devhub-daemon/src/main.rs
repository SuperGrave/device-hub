//! devhub-daemon: 常駐して Logitech (HID++) デバイスの設定を保ち続ける。G HUB の置換。
//!
//! - 起動すると HID を監視し、対応デバイスを開く (`hidpp::open` が保存設定を再適用する)。
//! - 数秒ごとに `read()` する。スリープ復帰・電源再投入でデバイスが既定値に戻っていれば
//!   `read()` 内で保存値を書き戻す。抜き差しは `scan` の変化で検知して開き直す。
//! - 二重起動は 127.0.0.1:47831 の bind で防ぐ。
//! - `--install` でログオン時の自動起動 (HKCU Run) を登録、`--uninstall` で解除。
//!
//! コンソールを出さないため release では windows subsystem で動く。`--console` で出力を見られる。

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use devhub_core::model::DriverKind;
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "DeviceHubDaemon";

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--install" => return install(),
        "--uninstall" => return uninstall(),
        _ => {}
    }
    // 二重起動ガード (プロセス終了で OS が解放する)。
    let Ok(_guard) = TcpListener::bind("127.0.0.1:47831") else {
        return;
    };
    run();
}

fn install() {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("実行ファイルのパスを取得できません");
        return;
    };
    let value = format!("\"{}\"", exe.display());
    let ok = Command::new("reg")
        .args(["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &value, "/f"])
        .status()
        .is_ok_and(|s| s.success());
    println!(
        "{}",
        if ok {
            "自動起動を登録しました"
        } else {
            "登録に失敗しました"
        }
    );
}

fn uninstall() {
    let _ = Command::new("reg")
        .args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"])
        .status();
    println!("自動起動を解除しました");
}

fn run() {
    let verbose = std::env::args().any(|a| a == "--console");
    let backend = loop {
        match devhub_hid::connect() {
            Ok(b) => break b,
            Err(e) => {
                if verbose {
                    eprintln!("hid init: {e}");
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };
    loop {
        if let Ok(devs) = backend.scan() {
            for d in devs
                .iter()
                .filter(|d| d.driver == DriverKind::Hidpp && d.supported)
            {
                // driver() は初回に開いて保存設定を再適用、以後はキャッシュを返す。
                match backend.driver(&d.id).and_then(|drv| drv.read().map(|_| ())) {
                    Ok(()) => {}
                    Err(e) => {
                        if verbose {
                            eprintln!("{}: {e}", d.name);
                        }
                        backend.invalidate(&d.id);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
