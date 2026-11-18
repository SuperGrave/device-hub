//! Logicool / Logitech HID++ 2.0 ドライバ (G PRO Wireless — G HUB 置換)。
//!
//! 公開仕様 (Solaar / libratbag の HID++ 2.0 解析) に基づく。**実機未検証**。
//!
//! - トランスポート: vendor usage page FF00 / usage 0002 の長レポート (report 0x11, 20B)。
//!   有線 (C088) は device index 0xFF、Lightspeed 受信機 (C539/C53A) は index 1。
//! - 機能は IRoot (index 0) の GetFeature で feature id → index を引く。
//! - 対応: DPI (0x2201) / ポーリング (0x8060) / バッテリー (0x1001, 0x1004) / FW (0x0003)。
//! - 未対応 (caps に出さない): ボタン割り当て・ライティング (0x8100 オンボードプロファイルの
//!   セクター書き込みは実機無しでは危険なため)。
//!
//! G HUB は常駐してデバイス設定をホストモードで再適用する。これを置換するため、適用した設定を
//! `%APPDATA%\device-hub\hidpp-settings.json` に保存し、ドライバを開いたとき・`read()` で
//! 実機の値が保存値とずれていた (スリープ復帰・電源再投入) ときに再適用する。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REPORT_LONG: u8 = 0x11;
const LEN_LONG: usize = 20;
/// ソフトウェア ID (1..=15。0 は通知用なので避ける)。
const SWID: u8 = 0x0A;

const F_ROOT: u16 = 0x0000;
const F_FWVER: u16 = 0x0003;
const F_BATT_UNIFIED: u16 = 0x1004;
const F_BATT_VOLT: u16 = 0x1001;
const F_DPI: u16 = 0x2201;
const F_RATE: u16 = 0x8060;

// ---------------------------------------------------------------- 純粋関数 (テスト対象)

/// 長レポート 1 本を組む: `[0x11, dev, feat_index, (func<<4)|swid, params…]`。
pub fn build_request(dev: u8, feat_index: u8, func: u8, params: &[u8]) -> [u8; LEN_LONG] {
    let mut b = [0u8; LEN_LONG];
    b[0] = REPORT_LONG;
    b[1] = dev;
    b[2] = feat_index;
    b[3] = (func << 4) | SWID;
    let n = params.len().min(LEN_LONG - 4);
    b[4..4 + n].copy_from_slice(&params[..n]);
    b
}

/// 応答の分類。
#[derive(Debug, PartialEq)]
pub enum Reply<'a> {
    /// 期待した応答 (params = b[4..])。
    Ok(&'a [u8]),
    /// HID++ 2.0 エラー (feature index 0xFF)。
    Err(u8),
    /// 別の応答 / 通知 — 読み飛ばす。
    Other,
}

pub fn classify<'a>(b: &'a [u8], dev: u8, feat_index: u8, func: u8) -> Reply<'a> {
    if b.len() < 4 || b[0] != REPORT_LONG || b[1] != dev {
        return Reply::Other;
    }
    let fs = (func << 4) | SWID;
    if b[2] == 0xFF && b.len() >= 6 && b[3] == feat_index && b[4] == fs {
        return Reply::Err(b[5]);
    }
    if b[2] == feat_index && b[3] == fs {
        return Reply::Ok(&b[4..]);
    }
    Reply::Other
}

/// 0x2201 getSensorDpiList の応答 → (min, max, step)。
/// `params` は `[sensor, w0_hi, w0_lo, w1_hi, w1_lo, …]`。0xE000 以上のワードは刻み指定。
pub fn parse_dpi_list(params: &[u8]) -> Option<(u32, u32, u32)> {
    let words: Vec<u16> = params
        .get(1..)?
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .take_while(|w| *w != 0)
        .collect();
    if words.len() >= 3 && words[1] >= 0xE000 {
        let step = (words[1] & 0x1FFF) as u32;
        return Some((words[0] as u32, words[2] as u32, step.max(1)));
    }
    let min = *words.first()? as u32;
    let max = *words.last()? as u32;
    Some((min, max, 50))
}

/// 0x8060 のポーリング間隔ビットマスク (bit n = n+1 ms) → Hz の降順一覧。
pub fn rate_list_hz(mask: u8) -> Vec<u32> {
    let mut v: Vec<u32> = (0..8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| 1000 / (i as u32 + 1))
        .collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

/// LiPo の電圧 (mV) → 残量 % (Solaar と同じ折れ線テーブル)。
pub fn voltage_to_percent(mv: u16) -> u8 {
    const T: [(u16, u8); 13] = [
        (4186, 100),
        (4067, 90),
        (3989, 80),
        (3922, 70),
        (3859, 60),
        (3811, 50),
        (3778, 40),
        (3751, 30),
        (3717, 20),
        (3671, 10),
        (3646, 5),
        (3579, 2),
        (3500, 0),
    ];
    if mv >= T[0].0 {
        return 100;
    }
    for w in T.windows(2) {
        let (hi_mv, hi_p) = w[0];
        let (lo_mv, lo_p) = w[1];
        if mv >= lo_mv {
            let span = (hi_mv - lo_mv) as f32;
            let f = (mv - lo_mv) as f32 / span;
            return (lo_p as f32 + f * (hi_p - lo_p) as f32).round() as u8;
        }
    }
    0
}

// ---------------------------------------------------------------- 保存設定

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Saved {
    dpi: Option<u32>,
    rate_hz: Option<u32>,
}

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("device-hub").join("hidpp-settings.json"))
}

fn load_saved(key: &str) -> Saved {
    let Some(p) = settings_path() else {
        return Saved::default();
    };
    let Ok(txt) = std::fs::read_to_string(p) else {
        return Saved::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return Saved::default();
    };
    let e = &v[key];
    Saved {
        dpi: e["dpi"].as_u64().map(|x| x as u32),
        rate_hz: e["rate_hz"].as_u64().map(|x| x as u32),
    }
}

fn store_saved(key: &str, s: Saved) {
    let Some(p) = settings_path() else { return };
    let mut v = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    v[key] = serde_json::json!({ "dpi": s.dpi, "rate_hz": s.rate_hz });
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, serde_json::to_string_pretty(&v).unwrap_or_default());
}

// ---------------------------------------------------------------- ドライバ

struct Features {
    dpi: Option<u8>,
    rate: Option<u8>,
    batt_unified: Option<u8>,
    batt_volt: Option<u8>,
    fwver: Option<u8>,
}

struct Ranges {
    dpi: (u32, u32, u32),
    rates: Vec<u32>,
}

pub struct HidppDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    /// device index (有線 0xFF / 受信機 1)。
    idx: u8,
    feats: Features,
    ranges: Ranges,
    key: String,
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo) -> HubResult<HidppDriver> {
    dev.set_blocking_mode(false).ok();
    let idx = if info.transport == Transport::Wired {
        0xFF
    } else {
        1
    };
    let mut d = HidppDriver {
        dev: Mutex::new(dev),
        key: format!("{:04x}:{:04x}", info.vid, info.pid),
        info: Mutex::new(info),
        idx,
        feats: Features {
            dpi: None,
            rate: None,
            batt_unified: None,
            batt_volt: None,
            fwver: None,
        },
        ranges: Ranges {
            dpi: (100, 25600, 50),
            rates: vec![1000, 500, 250, 125],
        },
    };
    d.discover()?;
    d.reapply();
    Ok(d)
}

impl HidppDriver {
    /// 1 往復。`feat_index` に対して `func` を呼び、params を返す。
    fn call(&self, feat_index: u8, func: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        // 古い通知を捨てる。
        let mut junk = [0u8; 64];
        while matches!(dev.read_timeout(&mut junk, 0), Ok(n) if n > 0) {}
        let req = build_request(self.idx, feat_index, func, params);
        dev.write(&req)
            .map_err(|e| HubError::Io(format!("hid write: {e}")))?;
        let deadline = Instant::now() + Duration::from_millis(900);
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            let n = dev
                .read_timeout(&mut buf, 50)
                .map_err(|e| HubError::Io(format!("hid read: {e}")))?;
            if n == 0 {
                continue;
            }
            match classify(&buf[..n], self.idx, feat_index, func) {
                Reply::Ok(p) => return Ok(p.to_vec()),
                Reply::Err(c) => {
                    return Err(HubError::Protocol(format!("HID++ error 0x{c:02X}")))
                }
                Reply::Other => {}
            }
        }
        // 受信機経由で応答無し = マウス休眠/未接続。
        if self.idx != 0xFF {
            Err(HubError::Disconnected)
        } else {
            Err(HubError::Protocol("応答がありません".into()))
        }
    }

    /// IRoot.GetFeature。0 = 非対応。
    fn feature_index(&self, id: u16) -> HubResult<Option<u8>> {
        let r = self.call(0, 0, &id.to_be_bytes())?;
        Ok(match r.first() {
            Some(&0) | None => None,
            Some(&i) => Some(i),
        })
    }

    fn discover(&mut self) -> HubResult<()> {
        // IRoot への疎通確認 (protocol version)。ここで失敗すれば開けない。
        let pv = self.call(0, 1, &[])?;
        let _ = pv;
        self.feats.dpi = self.feature_index(F_DPI)?;
        self.feats.rate = self.feature_index(F_RATE)?;
        self.feats.batt_unified = self.feature_index(F_BATT_UNIFIED)?;
        self.feats.batt_volt = self.feature_index(F_BATT_VOLT)?;
        self.feats.fwver = self.feature_index(F_FWVER)?;
        let _ = F_ROOT;
        if let Some(i) = self.feats.dpi {
            if let Ok(r) = self.call(i, 1, &[0]) {
                if let Some(range) = parse_dpi_list(&r) {
                    self.ranges.dpi = range;
                }
            }
        }
        if let Some(i) = self.feats.rate {
            if let Ok(r) = self.call(i, 0, &[]) {
                if let Some(&m) = r.first() {
                    let l = rate_list_hz(m);
                    if !l.is_empty() {
                        self.ranges.rates = l;
                    }
                }
            }
        }
        Ok(())
    }

    fn live_dpi(&self) -> HubResult<u32> {
        let i = self.feats.dpi.ok_or(HubError::Unsupported("dpi"))?;
        let r = self.call(i, 2, &[0])?;
        if r.len() < 3 {
            return Err(HubError::Protocol("dpi: short".into()));
        }
        Ok(u16::from_be_bytes([r[1], r[2]]) as u32)
    }

    fn set_dpi(&self, dpi: u32) -> HubResult<()> {
        let i = self.feats.dpi.ok_or(HubError::Unsupported("dpi"))?;
        let (min, max, step) = self.ranges.dpi;
        let d = dpi.clamp(min, max);
        let d = min + (d - min + step / 2) / step * step;
        let b = (d.min(max) as u16).to_be_bytes();
        self.call(i, 3, &[0, b[0], b[1]])?;
        Ok(())
    }

    fn live_rate_hz(&self) -> HubResult<u32> {
        let i = self.feats.rate.ok_or(HubError::Unsupported("polling"))?;
        let r = self.call(i, 1, &[])?;
        let ms = *r.first().ok_or_else(|| HubError::Protocol("rate: short".into()))?;
        Ok(1000 / ms.max(1) as u32)
    }

    fn set_rate_hz(&self, hz: u32) -> HubResult<()> {
        let i = self.feats.rate.ok_or(HubError::Unsupported("polling"))?;
        let ms = (1000 / hz.max(1)).clamp(1, 8) as u8;
        self.call(i, 2, &[ms])?;
        Ok(())
    }

    /// 保存値を実機へ書く (ずれているものだけ)。
    fn reapply(&self) {
        let s = load_saved(&self.key);
        if let (Some(d), Ok(cur)) = (s.dpi, self.live_dpi()) {
            if d != cur {
                let _ = self.set_dpi(d);
            }
        }
        if let (Some(h), Ok(cur)) = (s.rate_hz, self.live_rate_hz()) {
            if h != cur {
                let _ = self.set_rate_hz(h);
            }
        }
    }

    fn save(&self, f: impl FnOnce(&mut Saved)) {
        let mut s = load_saved(&self.key);
        f(&mut s);
        store_saved(&self.key, s);
    }
}

impl DeviceDriver for HidppDriver {
    fn info(&self) -> DeviceInfo {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(fi) = self.feats.fwver {
            // entity 0 = メインファーム: [type, prefix×3, major, minor, build(2), …]
            match self.call(fi, 1, &[0]) {
                Ok(r) if r.len() >= 8 => {
                    let prefix = String::from_utf8_lossy(&r[1..4]).trim().to_string();
                    i.firmware = Some(format!(
                        "{prefix} {:02x}.{:02x}.B{:04x}",
                        r[4],
                        r[5],
                        u16::from_be_bytes([r[6], r[7]])
                    ));
                    i.asleep = false;
                }
                Err(HubError::Disconnected) => i.asleep = true,
                _ => {}
            }
        }
        if let Ok(b) = self.battery() {
            i.battery = Some(b);
        }
        i
    }

    fn caps(&self) -> Capabilities {
        use section::*;
        let mut c = Capabilities::empty();
        c.battery = self.feats.batt_unified.is_some() || self.feats.batt_volt.is_some();
        if self.feats.rate.is_some() {
            let opts: Vec<ChoiceOpt> = self
                .ranges
                .rates
                .iter()
                .map(|&h| ChoiceOpt::new(h as i64, format!("{h}")))
                .collect();
            c.settings.push(
                SettingSpec::choice("polling", PERFORMANCE, "ポーリングレート", opts)
                    .help("マウスがPCへ位置を報告する頻度 (Hz)"),
            );
        }
        if self.feats.dpi.is_some() {
            let (min, max, step) = self.ranges.dpi;
            c.mouse = Some(MouseCaps {
                dpi_min: min,
                dpi_max: max,
                dpi_step: step,
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
        }
        c
    }

    fn read(&self) -> HubResult<DeviceState> {
        // スリープ復帰・電源再投入でデバイス側が既定値に戻っていたら保存値を戻す。
        self.reapply();
        let mut settings = BTreeMap::new();
        if let Ok(h) = self.live_rate_hz() {
            settings.insert("polling".to_string(), SettingValue::Choice(h as i64));
        }
        let mouse = match self.live_dpi() {
            Ok(dpi) => Some(MouseState {
                stages: vec![DpiStage {
                    x: dpi,
                    y: None,
                    color: Rgb::default(),
                    enabled: true,
                }],
                active_stage: 0,
                buttons: vec![],
                macros: vec![],
                adaptive: None,
            }),
            Err(HubError::Disconnected) => return Err(HubError::Disconnected),
            Err(_) => None,
        };
        Ok(DeviceState {
            profile: ProfileState::default(),
            settings,
            mouse,
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting {
                id,
                value: SettingValue::Choice(h),
            } if id == "polling" => {
                self.set_rate_hz(*h as u32)?;
                self.save(|s| s.rate_hz = Some(*h as u32));
                Ok(())
            }
            Change::DpiStages { stages, active } => {
                let st = stages
                    .get(*active)
                    .or_else(|| stages.first())
                    .ok_or_else(|| HubError::Invalid("DPI ステージがありません".into()))?;
                self.set_dpi(st.x)?;
                self.save(|s| s.dpi = Some(st.x));
                Ok(())
            }
            Change::ActiveStage(_) => Ok(()),
            _ => Err(HubError::Unsupported("この変更")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        if let Some(i) = self.feats.batt_unified {
            // getStatus: [percent, level bitmask, status(0 放電/1 充電中/2 充電完了…)]
            let r = self.call(i, 1, &[])?;
            if r.len() >= 3 {
                return Ok(Battery {
                    percent: Some(r[0].min(100)),
                    charging: matches!(r[2], 1..=3),
                });
            }
        }
        if let Some(i) = self.feats.batt_volt {
            // getBatteryInfo: [mV(2), flags]。bit7 = 充電中。
            let r = self.call(i, 0, &[])?;
            if r.len() >= 3 {
                let mv = u16::from_be_bytes([r[0], r[1]]);
                return Ok(Battery {
                    percent: Some(voltage_to_percent(mv)),
                    charging: r[2] & 0x80 != 0,
                });
            }
        }
        Err(HubError::Unsupported("battery"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let b = build_request(0xFF, 0x05, 3, &[0, 0x06, 0x40]);
        assert_eq!(&b[..7], &[0x11, 0xFF, 0x05, 0x3A, 0, 0x06, 0x40]);
        assert_eq!(b.len(), 20);
    }

    #[test]
    fn classify_replies() {
        let mut ok = [0u8; 20];
        ok[..6].copy_from_slice(&[0x11, 1, 4, 0x1A, 7, 8]);
        assert!(matches!(classify(&ok, 1, 4, 1), Reply::Ok(p) if p[0] == 7));
        let mut err = [0u8; 20];
        err[..6].copy_from_slice(&[0x11, 1, 0xFF, 4, 0x1A, 0x02]);
        assert_eq!(classify(&err, 1, 4, 1), Reply::Err(2));
        assert_eq!(classify(&ok, 2, 4, 1), Reply::Other);
        assert_eq!(classify(&ok, 1, 5, 1), Reply::Other);
    }

    #[test]
    fn dpi_range_with_step() {
        // sensor 0, 100, step 50 (0xE032), 25600
        let p = [0, 0x00, 0x64, 0xE0, 0x32, 0x64, 0x00];
        assert_eq!(parse_dpi_list(&p), Some((100, 25600, 50)));
    }

    #[test]
    fn rates_and_battery() {
        assert_eq!(rate_list_hz(0b1000_1011), vec![1000, 500, 250, 125]);
        assert_eq!(voltage_to_percent(4200), 100);
        assert_eq!(voltage_to_percent(3500), 0);
        assert_eq!(voltage_to_percent(3859), 60);
    }
}
