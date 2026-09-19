//! 设备端后端：二进制直接跑在手机上（adb shell /data/local/tmp/atraverse ...）。
//!
//! 直接 exec `/system/bin` 下的工具：uiautomator / input / screencap / logcat / dumpsys，
//! 不需要 adb 转发，因此不受 USB 传输与 adb 协议开销影响（Fastbot 的同款形态）。

use super::{capture_output, Device, DEFAULT_TIMEOUT, DUMP_TIMEOUT};
use anyhow::{bail, Result};
use std::process::{Command, Stdio};

/// Android 上常用的可执行文件路径
const PATH_ENV: &str = "/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system/bin:/system/xbin:/vendor/bin:/product/bin";

pub struct LocalDevice {
    dump_path: String,
}

impl Default for LocalDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDevice {
    pub fn new() -> Self {
        let pid = std::process::id();
        Self {
            dump_path: format!("/data/local/tmp/.atraverse_dump_{}.xml", pid),
        }
    }

    fn base_cmd(program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("PATH", PATH_ENV);
        c.stdin(Stdio::null());
        c
    }
}

impl Device for LocalDevice {
    fn kind(&self) -> &'static str {
        "device"
    }

    fn sh(&self, cmd: &str, timeout: std::time::Duration) -> Result<String> {
        let mut c = Self::base_cmd("/system/bin/sh");
        c.arg("-c").arg(cmd);
        let out = capture_output(&mut c, timeout)?;
        let stdout = crate::util::trim_output(String::from_utf8_lossy(&out.stdout).to_string());
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if stdout.is_empty() && (stderr.contains("not found") || stderr.contains("No such file")) {
            bail!("命令执行失败: {} -> {}", cmd, stderr.trim());
        }
        Ok(stdout)
    }

    fn screencap(&self) -> Result<Vec<u8>> {
        // 直接执行，避免经过 shell 带来的二进制安全/引号问题
        let mut c = Self::base_cmd("screencap");
        c.arg("-p");
        let out = capture_output(&mut c, DEFAULT_TIMEOUT)?;
        if out.stdout.len() < 1024 || !out.stdout.starts_with(&[0x89, b'P', b'N', b'G']) {
            bail!("screencap 返回数据异常 ({} 字节)", out.stdout.len());
        }
        Ok(out.stdout)
    }

    fn dump_xml(&mut self) -> Result<String> {
        // 复用同一路径并在读取后删除：dump 前先清掉旧文件保证不会读到残留，
        // 也不会像递增文件名那样在 /data/local/tmp 里堆积上千个 XML 小文件
        let path = &self.dump_path;
        let _ = std::fs::remove_file(path);
        // 这里不走 self.sh()：它只返回 stdout，而 uiautomator 的失败原因写在 **stderr**
        // （典型 `ERROR: could not get idle state.`，此时它既不生成文件、stdout 也是空的）。
        // 只取 stdout 会把唯一线索丢成空字符串，失败就被误报成「读取 dump 文件失败」。
        let mut c = Self::base_cmd("uiautomator");
        c.arg("dump").arg(path);
        let out = capture_output(&mut c, DUMP_TIMEOUT)?;
        let so = crate::util::trim_output(String::from_utf8_lossy(&out.stdout).to_string());
        let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let said = match (so.trim().is_empty(), se.is_empty()) {
            (true, true) => "(stdout/stderr 均为空)".to_string(),
            (false, true) => so.clone(),
            (true, false) => se.clone(),
            (false, false) => format!("{} / {}", so, se),
        };
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => bail!(
                "uiautomator dump 未生成文件({})；命令输出: {}",
                e,
                crate::util::truncate(said.trim(), 200)
            ),
        };
        let _ = std::fs::remove_file(path);
        if s.contains("<hierarchy") {
            Ok(s)
        } else {
            bail!(
                "uiautomator dump 内容异常: {}",
                crate::util::truncate(&s, 80)
            )
        }
    }

    fn logcat_cmd(&self) -> Command {
        let mut c = Self::base_cmd("logcat");
        c.arg("-v")
            .arg("time")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        c
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(path)?)
    }
}
