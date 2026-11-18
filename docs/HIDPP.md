# Logitech HID++ 2.0 (G PRO Wireless) — G HUB 置換

実装: `crates/devhub-hid/src/hidpp.rs` / 常駐: `crates/devhub-daemon`。**実機未検証** (公開仕様 Solaar / libratbag ベース)。

- 対象: 有線 046D:C088、Lightspeed 受信機 046D:C539 / C53A。HID++ 長レポート (0x11, 20B)、usage FF00:0002。
  有線は device index 0xFF、受信機は 1。
- 機能: DPI (0x2201)、ポーリング (0x8060)、バッテリー (0x1001 電圧 / 0x1004)、FW (0x0003)。
- 未対応: ボタン割り当て・ライティング (0x8100 オンボードプロファイルのセクター書込み)。
- 設定は `%APPDATA%\device-hub\hidpp-settings.json` に保存し、デバイスを開いたとき・`read()` でずれを検知したとき再適用。

## 常駐
```powershell
cargo build --release -p devhub-daemon
target\release\devhub-daemon.exe --install     # ログオン時に自動起動 (HKCU Run)
target\release\devhub-daemon.exe --uninstall   # 解除
target\release\devhub-daemon.exe --console     # デバッグ出力 (debug ビルド時)
```
