//! PC 端后端：通过 adb 转发命令（调试用，或没有交叉编译产物时使用）。

use super::{capture_output, Device, DEFAULT_TIMEOUT, DUMP_TIMEOUT};
use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct AdbDevice {
    serial: Option<String>,
    /// dump 方式：首次探测后固定，避免每步都试错
    dump_mode: Option<DumpMode>,
}

/// `uiautomator dump` 的输出方式
#[derive(Clone, Copy)]
enum DumpMode {
    /// 直接输出到 stdout（/dev/tty），无文件往返 —— 老版本 Android 可用
    Tty,
    /// 落到设备文件再读回 —— Android 12+ /dev/tty 常常拿不到内容
    File,
}

impl AdbDevice {
    pub fn new(serial: Option<String>) -> Self {
        Self {
            serial,
            dump_mode: None,
        }
    }

    fn adb(&self) -> Command {
        let mut c = Command::new("adb");
        if let Some(s) = &self.serial {
            c.arg("-s").arg(s);
        }
        c
    }

    /// 已连接且状态为 device 的序列号列表
    pub fn devices() -> Result<Vec<String>> {
        let out = Command::new("adb")
            .arg("devices")
            .stdin(Stdio::null())
            .output()
            .context("执行 adb devices 失败（请确认 adb 在 PATH 中）")?;
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        let mut v = Vec::new();
        for line in s.lines().skip(1) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            if let (Some(id), Some(state)) = (it.next(), it.next()) {
                if state == "device" {
                    v.push(id.to_string());
                }
            }
        }
        Ok(v)
    }

    pub fn wait_for_device(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let devs = Self::devices()?;
            if let Some(s) = &self.serial {
                if devs.iter().any(|d| d == s) {
                    return Ok(());
                }
            } else if !devs.is_empty() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("等待设备超时：请用 adb devices 确认设备已连接并授权 USB 调试");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// adb push（本地 -> 设备）
    pub fn push(&self, local: &std::path::Path, remote: &str) -> Result<()> {
        let out = self
            .adb()
            .arg("push")
            .arg(local)
            .arg(remote)
            .stdin(Stdio::null())
            .output()
            .context("执行 adb push 失败")?;
        if !out.status.success() {
            bail!("adb push 失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }

    /// adb pull（设备 -> 本地目录）
    pub fn pull(&self, remote: &str, local: &std::path::Path) -> Result<()> {
        let out = self
            .adb()
            .arg("pull")
            .arg(remote)
            .arg(local)
            .stdin(Stdio::null())
            .output()
            .context("执行 adb pull 失败")?;
        if !out.status.success() {
            bail!("adb pull 失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }

    /// 执行一条命令并继承 stdio（用于把设备端 agent 的输出实时打到控制台）
    pub fn shell_inherit(&self, cmd: &str) -> Result<std::process::ExitStatus> {
        let status = self
            .adb()
            .arg("shell")
            .arg(cmd)
            .stdin(Stdio::null())
            .status()
            .context("执行 adb shell 失败")?;
        Ok(status)
    }
}

impl Device for AdbDevice {
    fn kind(&self) -> &'static str {
        "host"
    }

    fn sh(&self, cmd: &str, timeout: Duration) -> Result<String> {
        let mut c = self.adb();
        c.arg("shell").arg(cmd);
        let out = capture_output(&mut c, timeout)?;
        let stdout = crate::util::trim_output(String::from_utf8_lossy(&out.stdout).to_string());
        Ok(stdout)
    }

    fn screencap(&self) -> Result<Vec<u8>> {
        let mut c = self.adb();
        c.arg("exec-out").arg("screencap").arg("-p");
        let out = capture_output(&mut c, DEFAULT_TIMEOUT)?;
        if out.stdout.len() < 1024 || !out.stdout.starts_with(&[0x89, b'P', b'N', b'G']) {
            bail!("screencap 返回数据异常 ({} 字节)", out.stdout.len());
        }
        Ok(out.stdout)
    }

    fn dump_xml(&mut self) -> Result<String> {
        match self.dump_mode {
            Some(DumpMode::File) => self.dump_to_file(),
            Some(DumpMode::Tty) => self.dump_to_tty(),
            None => {
                // 首次：先试更快的 /dev/tty，拿不到再退化到文件
                match self.dump_to_tty() {
                    Ok(xml) => {
                        self.dump_mode = Some(DumpMode::Tty);
                        Ok(xml)
                    }
                    Err(e) => match self.dump_to_file() {
                        Ok(xml) => {
                            self.dump_mode = Some(DumpMode::File);
                            Ok(xml)
                        }
                        Err(e2) => bail!(
                            "uiautomator dump 失败：/dev/tty -> {}；文件方式 -> {}",
                            e,
                            e2
                        ),
                    },
                }
            }
        }
    }

    fn logcat_cmd(&self) -> Command {
        let mut c = self.adb();
        c.arg("logcat").arg("-v").arg("time");
        c.stdout(Stdio::piped()).stderr(Stdio::null());
        c
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let mut c = self.adb();
        c.arg("exec-out").arg("cat").arg(path);
        let out = capture_output(&mut c, DEFAULT_TIMEOUT)?;
        Ok(out.stdout)
    }
}

impl AdbDevice {
    fn dump_to_tty(&self) -> Result<String> {
        let out = self.sh("uiautomator dump /dev/tty", DUMP_TIMEOUT)?;
        if !out.contains("<hierarchy") {
            bail!("/dev/tty 未返回控件树: {}", crate::util::truncate(&out, 80));
        }
        Ok(out)
    }

    fn dump_to_file(&self) -> Result<String> {
        // 用 pid 区分，避免多个实例互相覆盖
        let path = format!("/data/local/tmp/.atraverse_dump_{}.xml", std::process::id());
        let _ = self.sh(&format!("rm -f {}", path), DEFAULT_TIMEOUT);
        self.sh(&format!("uiautomator dump {}", path), DUMP_TIMEOUT)?;
        let xml = String::from_utf8_lossy(&self.read_file(&path)?).to_string();
        let _ = self.sh(&format!("rm -f {}", path), DEFAULT_TIMEOUT);
        if !xml.contains("<hierarchy") {
            bail!("文件方式未返回控件树: {}", crate::util::truncate(&xml, 80));
        }
        Ok(xml)
    }
}
