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

/// 在 `parent` 下取一个**尚不存在**的目录名，避免两次运行挤进同一个目录。
///
/// 优先用 `stem` 本身；被占用则依次尝试 `{stem}_2`、`{stem}_3`……最多到 999，
/// 全都占用时退回带纳秒后缀的名字（几乎不可能走到）。
///
/// 存在的理由：报告目录名的时间戳只到**秒**。同一秒内连跑两次
/// （实测场景：先跑正样本、紧接着跑负样本校验闸门）会落进同一个目录，
/// 后一次的报告把前一次整个覆盖掉 —— 两次结果只剩一份，排查时非常容易被误导。
pub fn unique_dir(parent: &Path, stem: &str) -> std::path::PathBuf {
    let first = parent.join(stem);
    if !first.exists() {
        return first;
    }
    for n in 2..1000u32 {
        let cand = parent.join(format!("{stem}_{n}"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!(
        "{stem}_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S_%f")
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立临时目录，避免并发跑测试时互相踩
    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("atraverse_util_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn unique_dir_returns_stem_when_free() {
        let root = tmp_root("free");
        let d = unique_dir(&root, "script_20260919_104535");
        assert_eq!(d.file_name().unwrap(), "script_20260919_104535");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unique_dir_avoids_collision_without_overwriting() {
        let root = tmp_root("collide");
        let stem = "script_20260919_104535";

        // 第一次拿到原名，建出来；第二次必须换名，不能指向同一个目录
        let d1 = unique_dir(&root, stem);
        std::fs::create_dir_all(&d1).unwrap();
        let d2 = unique_dir(&root, stem);
        assert_ne!(d1, d2, "同名目录已存在时必须换名");
        assert_eq!(d2.file_name().unwrap(), "script_20260919_104535_2");

        std::fs::create_dir_all(&d2).unwrap();
        let d3 = unique_dir(&root, stem);
        assert_eq!(d3.file_name().unwrap(), "script_20260919_104535_3");
        std::fs::create_dir_all(&d3).unwrap();

        // 三个都应真实存在且互不相同
        assert!(d1.exists() && d2.exists() && d3.exists());
        assert_ne!(d1, d3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn slug_strips_unsafe_chars() {
        assert_eq!(slug("com.ss.android.ugc.aweme"), "com_ss_android_ugc_aweme");
        assert_eq!(slug("///"), "unknown");
        assert_eq!(slug("a b\tc"), "abc");
    }
}
