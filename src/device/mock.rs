//! 模拟设备：脚本化的假 App + 假 dump / 截图 / logcat。
//! 仅用于测试——没有真机时也能端到端验证遍历、标注、异常捕获与报告。

use super::Device;
use anyhow::Result;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

pub struct MockDevice {
    /// 0 = 列表页 com.demo/.Main，1 = 详情页 com.demo/.Detail
    pub screen: Mutex<u32>,
    /// 滑动次数（用于改变列表内容，模拟滚动出新的条目）
    pub scroll: Mutex<u32>,
    pub cmds: Mutex<Vec<String>>,
    pub log_lines: Vec<String>,
    pub png: Vec<u8>,
    pub tmp: std::path::PathBuf,
}

impl MockDevice {
    pub fn new(tmp: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(tmp);
        let img = image::RgbaImage::from_pixel(540, 1200, image::Rgba([40, 42, 48, 255]));
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        Self {
            screen: Mutex::new(0),
            scroll: Mutex::new(0),
            cmds: Mutex::new(Vec::new()),
            log_lines: Vec::new(),
            png: buf,
            tmp: tmp.to_path_buf(),
        }
    }

    pub fn with_log(lines: Vec<String>, tmp: &std::path::Path) -> Self {
        let mut m = Self::new(tmp);
        m.log_lines = lines;
        m
    }

    fn activity(&self) -> String {
        if *self.screen.lock().unwrap() == 0 {
            "com.demo/.Main".to_string()
        } else {
            "com.demo/.Detail".to_string()
        }
    }

    fn xml(&self) -> String {
        let n = *self.scroll.lock().unwrap();
        if *self.screen.lock().unwrap() == 0 {
            format!(
                r#"<hierarchy rotation="0" width="540" height="1200">
<node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" bounds="[0,0][540,1200]">
<node index="0" text="" resource-id="com.demo:id/list" class="android.widget.ListView" package="com.demo" scrollable="true" bounds="[0,100][540,1100]">
<node index="0" text="条目A{}" resource-id="com.demo:id/item_title" class="android.widget.TextView" package="com.demo" clickable="true" long-clickable="true" bounds="[0,100][540,200]"/>
<node index="1" text="条目B{}" resource-id="com.demo:id/item_title" class="android.widget.TextView" package="com.demo" clickable="true" bounds="[0,210][540,310]"/>
</node>
<node index="1" text="" resource-id="com.demo:id/et" class="android.widget.EditText" package="com.demo" content-desc="搜索框" clickable="true" long-clickable="true" bounds="[20,1120][520,1180]"/>
</node>
</hierarchy>"#,
                n, n
            )
        } else {
            r#"<hierarchy rotation="0" width="540" height="1200">
<node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" bounds="[0,0][540,1200]">
<node index="0" text="详情标题" resource-id="com.demo:id/detail_title" class="android.widget.TextView" package="com.demo" clickable="true" bounds="[0,100][540,200]"/>
<node index="1" text="返回" resource-id="com.demo:id/back" class="android.widget.Button" package="com.demo" clickable="true" bounds="[0,1000][540,1100]"/>
</node>
</hierarchy>"#
                .to_string()
        }
    }
}

impl Device for MockDevice {
    fn kind(&self) -> &'static str {
        "mock"
    }

    fn sh(&self, cmd: &str, _timeout: Duration) -> Result<String> {
        self.cmds.lock().unwrap().push(cmd.to_string());
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        match parts.as_slice() {
            ["getprop", k] => Ok(match *k {
                "ro.product.model" => "MockPhone".to_string(),
                "ro.product.manufacturer" => "Mock".to_string(),
                "ro.build.version.release" => "13".to_string(),
                "ro.build.version.sdk" => "33".to_string(),
                "ro.product.cpu.abi" => "arm64-v8a".to_string(),
                _ => String::new(),
            }),
            ["wm", "size"] => Ok("Physical size: 540x1200".to_string()),
            ["dumpsys", "activity", ..] => {
                let a = self.activity();
                Ok(format!(
                    "topResumedActivity=ActivityRecord{{1 u0 {} t1}}",
                    a
                ))
            }
            ["input", "tap", x, y] => {
                let y: i32 = y.parse().unwrap_or(0);
                let _x: i32 = x.parse().unwrap_or(0);
                let mut s = self.screen.lock().unwrap();
                if *s == 0 && (100..320).contains(&y) {
                    *s = 1; // 点列表项进详情
                } else if *s == 1 && y > 950 {
                    *s = 0; // 点返回
                }
                Ok(String::new())
            }
            ["input", "keyevent", code] => {
                if *code == "4" {
                    *self.screen.lock().unwrap() = 0;
                }
                Ok(String::new())
            }
            ["input", "swipe", ..] => {
                *self.scroll.lock().unwrap() += 1;
                Ok(String::new())
            }
            ["input", "text", ..] => Ok(String::new()),
            ["am", "start", ..] => {
                *self.screen.lock().unwrap() = 0;
                Ok("Status: ok".to_string())
            }
            ["am", "force-stop", ..] => Ok(String::new()),
            ["logcat", "-c"] => Ok(String::new()),
            ["pidof", ..] => Ok("1234".to_string()),
            ["ps", ..] => Ok("com.demo".to_string()),
            ["dumpsys", "dropbox", ..] => Ok("dropbox: mock entry".to_string()),
            ["cat", ..] => Ok(String::new()),
            ["ls", ..] => Ok(String::new()),
            ["cmd", "package", "resolve-activity", ..] => Ok("com.demo/.Main".to_string()),
            _ => Ok(String::new()),
        }
    }

    fn screencap(&self) -> Result<Vec<u8>> {
        Ok(self.png.clone())
    }

    fn dump_xml(&mut self) -> Result<String> {
        Ok(self.xml())
    }

    fn logcat_cmd(&self) -> Command {
        let file = self.tmp.join("mock_logcat.txt");
        let _ = std::fs::write(&file, self.log_lines.join("\n"));
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "type", &file.to_string_lossy()]);
            c
        } else {
            let mut c = Command::new("cat");
            c.arg(&file);
            c
        };
        c.stdout(Stdio::piped()).stderr(Stdio::null());
        c
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(path)?)
    }
}
