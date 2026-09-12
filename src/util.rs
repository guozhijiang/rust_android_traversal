//! 通用小工具：时间、路径、字符串、哈希。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::Instant;

/// 本地时间，用于展示：`2026-09-09 23:50:01`
pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 本地时间，用于文件名：`20260909_235001`
pub fn now_file_str() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

pub fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// 毫秒时长格式化：`1h02m03s` / `12m03s` / `3.4s`
pub fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!(
            "{:02}h{:02}m{:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{:02}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, (ms % 1000) / 100)
    }
}

/// 设备上 `toybox/sh` 的单引号包裹转义（空格用 %s 交给 input 命令处理）
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn ensure_dir(p: &Path) -> std::io::Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}

/// 去掉 adb 输出尾部的 `\r\n`（Windows 上 adb 会带 `\r\n`）
pub fn trim_output(mut s: String) -> String {
    while s.ends_with('\n') || s.ends_with('\r') || s.ends_with(' ') {
        s.pop();
    }
    s
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

pub fn hash64<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// 把任意字符串压成安全的文件名片段
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if c == '.' || c == ':' || c == '/' {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}
