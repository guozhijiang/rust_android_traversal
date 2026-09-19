//! 设备能力抽象。
//!
//! 同一套遍历引擎有两种运行形态：
//!   · `LocalDevice`：二进制本身跑在手机上（Fastbot 形态），直接 exec `/system/bin/*`；
//!   · `AdbDevice`：跑在 PC 上，通过 adb 转发命令（便于调试、无需交叉编译）。
//!
//! 上层（策略 / 截图 / 监控 / 报告）只依赖 `Device` trait，两者行为一致。

pub mod adb;
pub mod local;
/// 模拟设备：供 `atraverse demo` 与单元测试使用，无需真机即可验证整条链路
pub mod mock;

pub use adb::AdbDevice;
pub use local::LocalDevice;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// 默认命令超时
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
/// dump 超时（uiautomator 在复杂界面上可能较慢）
pub const DUMP_TIMEOUT: Duration = Duration::from_secs(30);

pub trait Device {
    /// "device" 或 "host"
    fn kind(&self) -> &'static str;

    /// 执行一条 shell 命令（字符串形式，引号由调用方负责），返回 stdout
    fn sh(&self, cmd: &str, timeout: Duration) -> Result<String>;

    /// 执行但不关心失败
    fn sh_quiet(&self, cmd: &str) -> String {
        self.sh(cmd, DEFAULT_TIMEOUT).unwrap_or_default()
    }

    /// 截图 PNG 原始字节
    fn screencap(&self) -> Result<Vec<u8>>;

    /// 控件树 dump（XML 文本）
    fn dump_xml(&mut self) -> Result<String>;

    /// logcat 长连接命令（用于流式监控）
    fn logcat_cmd(&self) -> Command;

    /// 读取设备上的文件（host 模式通过 adb，device 模式直接读本地文件系统）
    fn read_file(&self, path: &str) -> Result<Vec<u8>>;
}

/// 带超时地执行命令并收集输出（避免 uiautomator / screencap 卡死）。
///
/// 注意：**必须**用独立线程并发抽干 stdout/stderr。若只在子进程退出后才读管道，
/// 输出量超过管道缓冲区（dumpsys 数千行、screencap 上 MB）时子进程会阻塞在写管道上，
/// 父进程又等它退出 —— 经典死锁，表现为必然超时。
pub fn capture_output(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("无法启动命令: {:?}", cmd.get_program()))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let drain = |pipe: &mut Option<std::process::ChildStdout>| -> Vec<u8> {
        let mut v = Vec::new();
        if let Some(p) = pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    };
    // stderr 类型不同，单独包一层
    let h_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let h_out = std::thread::spawn(move || drain(&mut out_pipe));

    let program = cmd.get_program().to_os_string();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = h_out.join().unwrap_or_default();
            let stderr = h_err.join().unwrap_or_default();
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            // kill 后管道关闭，读线程会自行结束
            let _ = h_out.join();
            let _ = h_err.join();
            bail!("命令超时（{:?}）: {:?}", timeout, program);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ------------------------------------------------------------------ 高层操作

pub fn getprop<D: Device + ?Sized>(d: &D, key: &str) -> String {
    d.sh_quiet(&format!("getprop {}", key)).trim().to_string()
}

pub fn sdk_int<D: Device + ?Sized>(d: &D) -> i32 {
    getprop(d, "ro.build.version.sdk").parse().unwrap_or(0)
}

pub fn screen_size<D: Device + ?Sized>(d: &D) -> Result<(i32, i32)> {
    let out = d.sh("wm size", DEFAULT_TIMEOUT)?;
    let line = out.lines().next_back().unwrap_or("");
    let dim = line.split(':').next_back().unwrap_or("").trim();
    let mut it = dim.split('x');
    let w: i32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    let h: i32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    if w > 0 && h > 0 {
        Ok((w, h))
    } else {
        Err(anyhow::anyhow!("无法解析屏幕尺寸: {}", out))
    }
}

/// 当前前台 Activity：`com.x/.Main`
pub fn current_activity<D: Device + ?Sized>(d: &D) -> Result<String> {
    let out = d.sh("dumpsys activity activities", DEFAULT_TIMEOUT)?;
    if let Some(a) = parse_resumed_activity(&out) {
        return Ok(a);
    }
    let out2 = d.sh("dumpsys activity top", DEFAULT_TIMEOUT)?;
    for line in out2.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ACTIVITY ") {
            let comp = rest.split_whitespace().next().unwrap_or("");
            if !comp.is_empty() {
                return Ok(comp.to_string());
            }
        }
    }
    Ok(String::new())
}

fn parse_resumed_activity(out: &str) -> Option<String> {
    let keys = [
        "topResumedActivity=",
        "mResumedActivity:",
        "mFocusedActivity:",
        "mLastResumedActivity=",
    ];
    for key in keys {
        for line in out.lines() {
            if let Some(idx) = line.find(key) {
                let seg = &line[idx + key.len()..];
                if let Some(comp) = extract_component(seg) {
                    return Some(comp);
                }
            }
        }
    }
    None
}

fn extract_component(seg: &str) -> Option<String> {
    let seg = seg.trim_start();
    let end = seg.find('}').unwrap_or(seg.len());
    for tok in seg[..end].split_whitespace() {
        if tok.contains('/') && tok.contains('.') {
            return Some(tok.to_string());
        }
    }
    None
}

/// 解析 LAUNCHER Activity
pub fn resolve_launcher<D: Device + ?Sized>(d: &D, package: &str) -> Option<String> {
    let out = d.sh_quiet(&format!("cmd package resolve-activity --brief {}", package));
    out.lines()
        .rfind(|l| l.contains('/'))
        .map(|l| l.trim().to_string())
}

pub fn force_stop<D: Device + ?Sized>(d: &D, package: &str) -> Result<()> {
    d.sh(&format!("am force-stop {}", package), DEFAULT_TIMEOUT)?;
    Ok(())
}

pub fn launch<D: Device + ?Sized>(d: &D, package: &str, component: Option<&str>) -> Result<String> {
    let comp = match component {
        Some(c) => c.to_string(),
        None => {
            resolve_launcher(d, package).unwrap_or_else(|| format!("{}/.MainActivity", package))
        }
    };
    let comp = if comp.contains('/') {
        comp
    } else {
        format!("{}/{}", package, comp)
    };
    d.sh(&format!("am start -n {} -W", comp), DEFAULT_TIMEOUT)?;
    Ok(comp)
}

pub fn is_running<D: Device + ?Sized>(d: &D, package: &str) -> bool {
    let pid = d.sh_quiet(&format!("pidof {}", package));
    let pid = pid.trim();
    if !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit() || c.is_whitespace()) {
        return true;
    }
    let ps = d.sh_quiet("ps -A -o NAME");
    if ps.is_empty() {
        return d.sh_quiet("ps").contains(package);
    }
    ps.lines().any(|l| l.trim() == package)
}

pub fn logcat_clear<D: Device + ?Sized>(d: &D) -> Result<()> {
    d.sh("logcat -c", DEFAULT_TIMEOUT)?;
    Ok(())
}

// ---- 输入

pub fn input_tap<D: Device + ?Sized>(d: &D, x: i32, y: i32) -> Result<()> {
    d.sh(&format!("input tap {} {}", x, y), DEFAULT_TIMEOUT)?;
    Ok(())
}

pub fn input_long_press<D: Device + ?Sized>(d: &D, x: i32, y: i32, ms: u32) -> Result<()> {
    d.sh(
        &format!("input swipe {} {} {} {} {}", x, y, x, y, ms),
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

pub fn input_swipe<D: Device + ?Sized>(
    d: &D,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    ms: u32,
) -> Result<()> {
    d.sh(
        &format!("input swipe {} {} {} {} {}", x1, y1, x2, y2, ms),
        DEFAULT_TIMEOUT,
    )?;
    Ok(())
}

/// 输入文本：空格转成 `%s`，整体单引号包裹
pub fn input_text<D: Device + ?Sized>(d: &D, text: &str) -> Result<()> {
    let escaped = text.replace(' ', "%s");
    let arg = crate::util::shell_quote(&escaped);
    d.sh(&format!("input text {}", arg), DEFAULT_TIMEOUT)?;
    Ok(())
}

pub fn input_keyevent<D: Device + ?Sized>(d: &D, code: i32) -> Result<()> {
    d.sh(&format!("input keyevent {}", code), DEFAULT_TIMEOUT)?;
    Ok(())
}

pub fn press_back<D: Device + ?Sized>(d: &D) -> Result<()> {
    input_keyevent(d, 4)
}

// ---- 系统动画开关

/// 三个动画缩放开关。置 0 后系统不再播放窗口 / 转场 / 属性动画。
pub const ANIM_KEYS: [&str; 3] = [
    "window_animation_scale",
    "transition_animation_scale",
    "animator_duration_scale",
];

/// 关掉系统动画，返回原值（供 [`restore_animations`] 复原）。
///
/// 为什么要动设备设置：`uiautomator dump` 必须等到 UI **空闲**（idle）才拿控件树。
/// 页面存在持续动画时（视频在播、转场没结束、无限循环动画）永远等不到 idle，
/// dump 直接以 `ERROR: could not get idle state.` 失败，而且失败前要白等十几秒。
/// 实测抖音推荐流（视频在播）：动画开启时 dump 3/3 失败（每次约 11.4s）；
/// 三个 scale 置 0 后，同一页面 3/3 成功（每次约 3s）。
/// 这是通用手段，不含任何应用专有知识，Appium/UiAutomator2 也都这么做。
///
/// 遍历与用例执行都该在开始前调用它 —— 用例执行的每条断言都要 dump 一次页面，
/// 不关动画的话每次断言都要先空转十几秒再失败，重试机制会把超时耗光。
pub fn disable_animations<D: Device + ?Sized>(d: &D) -> Vec<String> {
    let mut prev = Vec::with_capacity(ANIM_KEYS.len());
    for key in ANIM_KEYS {
        let cur = d
            .sh(
                &format!("settings get global {}", key),
                Duration::from_secs(10),
            )
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        prev.push(cur);
        let _ = d.sh(
            &format!("settings put global {} 0", key),
            Duration::from_secs(10),
        );
    }
    prev
}

/// 恢复 [`disable_animations`] 读到的原值；读不到（空 / null）时退回系统默认 1.0。
///
/// 注意：进程被 `kill -9` 时来不及执行这里，设备会残留动画关闭状态。
pub fn restore_animations<D: Device + ?Sized>(d: &D, prev: &[String]) {
    for (key, old) in ANIM_KEYS.iter().zip(prev) {
        let value = if old.is_empty() || old == "null" {
            "1.0"
        } else {
            old.as_str()
        };
        let _ = d.sh(
            &format!("settings put global {} {}", key, value),
            Duration::from_secs(10),
        );
    }
}

// ------------------------------------------------------------------ 设备信息

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceInfo {
    /// 运行形态：device（跑在手机上）/ host（PC 通过 adb）
    pub mode: String,
    pub serial: String,
    pub model: String,
    pub manufacturer: String,
    pub release: String,
    pub sdk: i32,
    pub abi: String,
    pub width: i32,
    pub height: i32,
}

impl DeviceInfo {
    pub fn fetch<D: Device + ?Sized>(d: &D, serial: &str) -> Result<Self> {
        let (w, h) = screen_size(d).unwrap_or((0, 0));
        Ok(Self {
            mode: d.kind().to_string(),
            serial: serial.to_string(),
            model: getprop(d, "ro.product.model"),
            manufacturer: getprop(d, "ro.product.manufacturer"),
            release: getprop(d, "ro.build.version.release"),
            sdk: sdk_int(d),
            abi: getprop(d, "ro.product.cpu.abi"),
            width: w,
            height: h,
        })
    }

    pub fn display(&self) -> String {
        format!(
            "{} {} (Android {} / API {} / {}x{} / {} / {})",
            self.manufacturer,
            self.model,
            self.release,
            self.sdk,
            self.width,
            self.height,
            self.abi,
            if self.serial.is_empty() {
                "本机"
            } else {
                &self.serial
            }
        )
    }
}

/// ABI -> Rust target 三元组
pub fn target_for_abi(abi: &str) -> &'static str {
    if abi.contains("arm64") || abi.contains("aarch64") {
        "aarch64-linux-android"
    } else if abi.contains("x86_64") {
        "x86_64-linux-android"
    } else {
        "armv7-linux-androideabi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resumed_activity() {
        let s = "topResumedActivity=ActivityRecord{8f2c1 u0 com.demo.app/.ui.MainActivity t42}";
        assert_eq!(
            parse_resumed_activity(s).unwrap(),
            "com.demo.app/.ui.MainActivity"
        );
    }

    #[test]
    fn maps_abi() {
        assert_eq!(target_for_abi("arm64-v8a"), "aarch64-linux-android");
        assert_eq!(target_for_abi("armeabi-v7a"), "armv7-linux-androideabi");
        assert_eq!(target_for_abi("x86_64"), "x86_64-linux-android");
    }

    /// 回归：输出量远超管道缓冲区时不得死锁。
    /// 曾经的写法是「子进程退出后才读管道」，dumpsys（数千行）/ screencap（MB 级）
    /// 一上来就把子进程堵在写管道上，父进程又等它退出 —— 必然超时。
    #[test]
    fn big_output_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("atraverse_pipe_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("big.txt");
        // 约 6MB，远超任何平台的 pipe buffer
        let line = "0123456789abcdefghijklmnopqrstuvwxyz-0123456789abcdefghijklmnopqrstuvwxyz\n";
        let mut content = String::new();
        for _ in 0..70_000 {
            content.push_str(line);
        }
        std::fs::write(&f, &content).unwrap();

        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "type", &f.to_string_lossy()]);
            c
        } else {
            let mut c = Command::new("cat");
            c.arg(&f);
            c
        };
        let out = capture_output(&mut c, Duration::from_secs(30)).expect("大输出被卡死");
        assert!(
            out.stdout.len() > 4_000_000,
            "输出被截断: {} 字节",
            out.stdout.len()
        );

        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
