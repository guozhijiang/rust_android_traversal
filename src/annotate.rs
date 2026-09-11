//! 截图标注：在截图上用图形标出本次操作（click / long_click / swipe / input / key）。
//!
//! 不依赖任何字体文件与 GUI 库：内置 5x7 点阵字模，直接按像素绘制，
//! 因此可以在任意环境（含 CI）离线生成带标注的 PNG。

use crate::model::{Action, Rect};
use anyhow::{bail, Result};
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};
use std::path::Path;

pub const COLOR_CLICK: (u8, u8, u8) = (255, 59, 48);
pub const COLOR_LONG: (u8, u8, u8) = (255, 149, 0);
pub const COLOR_SWIPE: (u8, u8, u8) = (10, 132, 255);
pub const COLOR_INPUT: (u8, u8, u8) = (175, 82, 222);
pub const COLOR_KEY: (u8, u8, u8) = (120, 200, 120);
const SHADOW: (u8, u8, u8) = (0, 0, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Click,
    LongClick,
    Swipe,
    Input,
    Key,
}

impl MarkerKind {
    pub fn color(&self) -> (u8, u8, u8) {
        match self {
            MarkerKind::Click => COLOR_CLICK,
            MarkerKind::LongClick => COLOR_LONG,
            MarkerKind::Swipe => COLOR_SWIPE,
            MarkerKind::Input => COLOR_INPUT,
            MarkerKind::Key => COLOR_KEY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Marker {
    pub kind: MarkerKind,
    /// 关键点：点击=1 个，滑动=起止 2 个
    pub points: Vec<(i32, i32)>,
    /// 目标控件范围（用于画框）
    pub bounds: Option<Rect>,
    pub number: usize,
    pub label: String,
}

/// 由动作生成标注
pub fn markers_for_action(action: &Action, step: usize) -> Vec<Marker> {
    let label = action.kind().to_uppercase();
    match action {
        Action::Click { point, target, .. } => vec![Marker {
            kind: MarkerKind::Click,
            points: vec![*point],
            bounds: Some(target.bounds),
            number: step,
            label,
        }],
        Action::LongClick { point, target, .. } => vec![Marker {
            kind: MarkerKind::LongClick,
            points: vec![*point],
            bounds: Some(target.bounds),
            number: step,
            label,
        }],
        Action::Swipe { from, to, target, .. } => vec![Marker {
            kind: MarkerKind::Swipe,
            points: vec![*from, *to],
            bounds: target.as_ref().map(|t| t.bounds),
            number: step,
            label,
        }],
        Action::Input { point, target, text, .. } => vec![Marker {
            kind: MarkerKind::Input,
            points: vec![*point],
            bounds: Some(target.bounds),
            number: step,
            label: format!("INPUT {}", text),
        }],
        Action::Back => vec![Marker {
            kind: MarkerKind::Key,
            points: vec![],
            bounds: None,
            number: step,
            label: "BACK".to_string(),
        }],
        Action::Key { name, .. } => vec![Marker {
            kind: MarkerKind::Key,
            points: vec![],
            bounds: None,
            number: step,
            label: name.to_uppercase(),
        }],
        Action::Launch { .. } => vec![],
    }
}

// ------------------------------------------------------------------ 画布

struct Canvas {
    img: RgbaImage,
    w: i32,
    h: i32,
    s: f32, // 相对 1080 宽的缩放系数
}

impl Canvas {
    fn new(img: RgbaImage) -> Self {
        let (w, h) = (img.width() as i32, img.height() as i32);
        let s = (w as f32 / 1080.0).clamp(0.6, 3.0);
        Self { img, w, h, s }
    }

    fn blend(&mut self, x: i32, y: i32, c: (u8, u8, u8), a: f32) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = a.clamp(0.0, 1.0);
        if a <= 0.001 {
            return;
        }
        let px = self.img.get_pixel_mut(x as u32, y as u32);
        let alpha = px.0[3] as f32 / 255.0;
        let eff = a * alpha;
        let cc = [c.0, c.1, c.2];
        for i in 0..3 {
            let cur = px.0[i] as f32;
            px.0[i] = (cur * (1.0 - eff) + cc[i] as f32 * eff) as u8;
        }
        if px.0[3] < 255 {
            px.0[3] = 255;
        }
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, w: i32, h: i32, c: (u8, u8, u8), a: f32) {
        for y in y0..(y0 + h) {
            for x in x0..(x0 + w) {
                self.blend(x, y, c, a);
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: (u8, u8, u8), a: f32) {
        let r0 = (cx - r - 1.0).floor() as i32;
        let r1 = (cx + r + 1.0).ceil() as i32;
        let c0 = (cy - r - 1.0).floor() as i32;
        let c1 = (cy + r + 1.0).ceil() as i32;
        for y in c0..=c1 {
            for x in r0..=r1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, cov * a);
                }
            }
        }
    }

    fn ring(&mut self, cx: f32, cy: f32, r: f32, t: f32, c: (u8, u8, u8), a: f32) {
        let outer = r + t / 2.0 + 1.0;
        let r0 = (cx - outer).floor() as i32;
        let r1 = (cx + outer).ceil() as i32;
        let c0 = (cy - outer).floor() as i32;
        let c1 = (cy + outer).ceil() as i32;
        for y in c0..=c1 {
            for x in r0..=r1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let cov = (t / 2.0 + 0.5 - (d - r).abs()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, cov * a);
                }
            }
        }
    }

    fn line(&mut self, p0: (i32, i32), p1: (i32, i32), t: f32, c: (u8, u8, u8), a: f32) {
        let dx = (p1.0 - p0.0) as f32;
        let dy = (p1.1 - p0.1) as f32;
        let dist = (dx * dx + dy * dy).sqrt().max(1.0);
        let steps = (dist.ceil() as i32).max(1);
        let r = (t / 2.0).max(0.6);
        for i in 0..=steps {
            let f = i as f32 / steps as f32;
            self.disc(p0.0 as f32 + dx * f, p0.1 as f32 + dy * f, r, c, a);
        }
    }

    fn stroke_rect(&mut self, r: &Rect, t: f32, c: (u8, u8, u8), a: f32) {
        let pts = [
            ((r.x1, r.y1), (r.x2, r.y1)),
            ((r.x2, r.y1), (r.x2, r.y2)),
            ((r.x2, r.y2), (r.x1, r.y2)),
            ((r.x1, r.y2), (r.x1, r.y1)),
        ];
        for (p0, p1) in pts {
            self.line(p0, p1, t, c, a);
        }
    }

    /// 点阵文字（仅支持大写字母/数字/少量符号）
    fn text(&mut self, x: i32, y: i32, s: &str, scale: i32, c: (u8, u8, u8), a: f32) {
        let mut cx = x;
        for ch in s.chars() {
            let ch = ch.to_ascii_uppercase();
            if let Some(g) = glyph(ch) {
                for (row, bits) in g.iter().enumerate() {
                    for col in 0..5 {
                        if (bits >> (4 - col)) & 1 == 1 {
                            self.fill_rect(
                                cx + col * scale,
                                y + row as i32 * scale,
                                scale,
                                scale,
                                c,
                                a,
                            );
                        }
                    }
                }
            }
            cx += 6 * scale;
        }
    }

    fn text_width(s: &str, scale: i32) -> i32 {
        (s.chars().count() as i32) * 6 * scale - scale
    }

    /// 带底色的标签条
    fn chip(&mut self, x: i32, y: i32, text: &str, c: (u8, u8, u8)) {
        let scale = (2.0 * self.s).round().max(1.0) as i32;
        let pad = (5.0 * self.s).round().max(2.0) as i32;
        let tw = Self::text_width(text, scale);
        let th = 7 * scale;
        let w = tw + pad * 2;
        let h = th + pad * 2;
        let (x, y) = (x.clamp(0, self.w - w), y.clamp(0, self.h - h));
        self.fill_rect(x, y, w, h, SHADOW, 0.72);
        self.stroke_rect(&Rect::new(x, y, x + w, y + h), 2.0 * self.s, c, 0.95);
        self.text(x + pad, y + pad, text, scale, c, 1.0);
    }

    /// 编号徽标
    fn badge(&mut self, cx: f32, cy: f32, number: usize, c: (u8, u8, u8)) {
        let r = 15.0 * self.s;
        let scale = (2.0 * self.s).round().max(1.0) as i32;
        let txt = number.to_string();
        self.disc(cx, cy, r + 3.0 * self.s, SHADOW, 0.55);
        self.disc(cx, cy, r, c, 1.0);
        let tw = Self::text_width(&txt, scale);
        let th = 7 * scale;
        self.text(
            (cx - tw as f32 / 2.0).round() as i32,
            (cy - th as f32 / 2.0).round() as i32,
            &txt,
            scale,
            (255, 255, 255),
            1.0,
        );
    }

    fn arrow(&mut self, from: (i32, i32), to: (i32, i32), t: f32, c: (u8, u8, u8)) {
        // 主体（先画阴影再画本体，保证在任意背景上可见）
        self.line(from, to, t + 4.0 * self.s, SHADOW, 0.55);
        self.line(from, to, t, c, 1.0);
        // 箭头
        let ang = ((to.1 - from.1) as f32).atan2((to.0 - from.0) as f32);
        let len = 26.0 * self.s;
        for sign in [-0.42f32, 0.42f32] {
            let a = ang + std::f32::consts::PI + sign;
            let tip = (
                (to.0 as f32 + a.cos() * len).round() as i32,
                (to.1 as f32 + a.sin() * len).round() as i32,
            );
            self.line(to, tip, t + 4.0 * self.s, SHADOW, 0.55);
            self.line(to, tip, t, c, 1.0);
        }
    }

    fn draw_marker(&mut self, m: &Marker) {
        let c = m.kind.color();
        let s = self.s;
        match m.kind {
            MarkerKind::Click => {
                if let Some(b) = &m.bounds {
                    self.stroke_rect(b, 3.0 * s, SHADOW, 0.45);
                    self.stroke_rect(b, 2.0 * s, c, 0.95);
                }
                if let Some(p) = m.points.first() {
                    let (x, y) = (p.0 as f32, p.1 as f32);
                    self.ring(x, y, 20.0 * s, 6.0 * s, SHADOW, 0.5);
                    self.ring(x, y, 20.0 * s, 4.0 * s, c, 1.0);
                    // 十字准星
                    let l = 13.0 * s;
                    self.line(((p.0 - l as i32), p.1), (p.0 + l as i32, p.1), 3.0 * s, c, 1.0);
                    self.line((p.0, p.1 - l as i32), (p.0, p.1 + l as i32), 3.0 * s, c, 1.0);
                    self.disc(x, y, 3.5 * s, c, 1.0);
                    self.badge(x + 26.0 * s, y - 26.0 * s, m.number, c);
                }
            }
            MarkerKind::LongClick => {
                if let Some(b) = &m.bounds {
                    self.stroke_rect(b, 3.0 * s, SHADOW, 0.45);
                    self.stroke_rect(b, 2.0 * s, c, 0.95);
                }
                if let Some(p) = m.points.first() {
                    let (x, y) = (p.0 as f32, p.1 as f32);
                    self.ring(x, y, 22.0 * s, 6.0 * s, SHADOW, 0.5);
                    self.ring(x, y, 22.0 * s, 4.0 * s, c, 1.0);
                    self.ring(x, y, 32.0 * s, 5.0 * s, SHADOW, 0.5);
                    self.ring(x, y, 32.0 * s, 3.0 * s, c, 0.9);
                    self.disc(x, y, 3.5 * s, c, 1.0);
                    self.badge(x + 34.0 * s, y - 34.0 * s, m.number, c);
                }
            }
            MarkerKind::Swipe => {
                if let Some(b) = &m.bounds {
                    self.stroke_rect(b, 3.0 * s, SHADOW, 0.35);
                    self.stroke_rect(b, 2.0 * s, c, 0.6);
                }
                if m.points.len() >= 2 {
                    let (from, to) = (m.points[0], m.points[1]);
                    self.arrow(from, to, 6.0 * s, c);
                    self.disc(from.0 as f32, from.1 as f32, 9.0 * s, c, 1.0);
                    let l = 9.0 * s;
                    self.line(
                        (to.0 - l as i32, to.1),
                        (to.0 + l as i32, to.1),
                        3.0 * s,
                        c,
                        1.0,
                    );
                    self.line(
                        (to.0, to.1 - l as i32),
                        (to.0, to.1 + l as i32),
                        3.0 * s,
                        c,
                        1.0,
                    );
                    self.badge(from.0 as f32 + 24.0 * s, from.1 as f32 - 24.0 * s, m.number, c);
                }
            }
            MarkerKind::Input => {
                if let Some(b) = &m.bounds {
                    self.stroke_rect(b, 4.0 * s, SHADOW, 0.45);
                    self.stroke_rect(b, 3.0 * s, c, 1.0);
                }
                if let Some(p) = m.points.first() {
                    let (x, y) = (p.0 as f32, p.1 as f32);
                    self.disc(x, y, 10.0 * s, c, 1.0);
                    // 光标竖线
                    self.fill_rect(
                        p.0 - (1.5 * s) as i32,
                        (p.1 as f32 - 16.0 * s) as i32,
                        (3.0 * s) as i32,
                        (32.0 * s) as i32,
                        c,
                        1.0,
                    );
                    self.badge(x + 24.0 * s, y - 24.0 * s, m.number, c);
                }
            }
            MarkerKind::Key => {
                // 无坐标的全局按键：左上角画一个角标
                let y = (24.0 * s) as i32;
                let x = (24.0 * s) as i32;
                self.badge(x as f32, y as f32 + 16.0 * s, m.number, c);
            }
        }

        // 文字标签（放在关键点下方，Key 类型放徽标右侧）
        let (lx, ly) = match m.kind {
            MarkerKind::Key => ((52.0 * s) as i32, (24.0 * s) as i32),
            _ => match m.points.first() {
                Some(p) => {
                    let below = p.1 + (44.0 * s) as i32;
                    let ly = if below + (40.0 * s) as i32 > self.h {
                        p.1 - (70.0 * s) as i32
                    } else {
                        below
                    };
                    ((p.0 as f32 - 20.0 * s) as i32, ly)
                }
                None => ((52.0 * s) as i32, (24.0 * s) as i32),
            },
        };
        if !m.label.is_empty() {
            self.chip(lx, ly, &m.label, c);
        }
    }
}

// ------------------------------------------------------------------ 对外接口

/// 在 PNG 截图上绘制标注，返回新的 PNG 字节
pub fn annotate_png(png: &[u8], markers: &[Marker]) -> Result<Vec<u8>> {
    let dyn_img = image::load_from_memory(png)?;
    let (w, h) = dyn_img.dimensions();
    if w == 0 || h == 0 {
        bail!("截图尺寸异常");
    }
    let mut canvas = Canvas::new(dyn_img.to_rgba8());
    for m in markers {
        canvas.draw_marker(m);
    }
    let mut out: Vec<u8> = Vec::new();
    let img = DynamicImage::ImageRgba8(canvas.img);
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

/// 生成缩略图（用于报告时间线）
pub fn write_thumbnail(png: &[u8], out: &Path, max_w: u32, max_h: u32) -> Result<()> {
    let dyn_img = image::load_from_memory(png)?;
    let (w, h) = dyn_img.dimensions();
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32).min(1.0);
    let tw = ((w as f32 * scale) as u32).max(1);
    let th = ((h as f32 * scale) as u32).max(1);
    let thumb = dyn_img.resize(tw, th, image::imageops::FilterType::Triangle);
    thumb.save(out)?;
    Ok(())
}

/// 从 PNG 读取尺寸
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    image::load_from_memory(png).ok().map(|i| i.dimensions())
}

// ------------------------------------------------------------------ 5x7 点阵字模

fn glyph(ch: char) -> Option<[u8; 7]> {
    let rows: [&str; 7] = match ch {
        '0' => ["01110", "10001", "10011", "10101", "11001", "10001", "01110"],
        '1' => ["00100", "01100", "00100", "00100", "00100", "00100", "01110"],
        '2' => ["01110", "10001", "00001", "00010", "00100", "01000", "11111"],
        '3' => ["11111", "00010", "00100", "00010", "00001", "10001", "01110"],
        '4' => ["00010", "00110", "01010", "10010", "11111", "00010", "00010"],
        '5' => ["11111", "10000", "11110", "00001", "00001", "10001", "01110"],
        '6' => ["00110", "01000", "10000", "11110", "10001", "10001", "01110"],
        '7' => ["11111", "00001", "00010", "00100", "01000", "01000", "01000"],
        '8' => ["01110", "10001", "10001", "01110", "10001", "10001", "01110"],
        '9' => ["01110", "10001", "10001", "01111", "00001", "00010", "01100"],
        'A' => ["01110", "10001", "10001", "11111", "10001", "10001", "10001"],
        'B' => ["11110", "10001", "10001", "11110", "10001", "10001", "11110"],
        'C' => ["01110", "10001", "10000", "10000", "10000", "10001", "01110"],
        'D' => ["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
        'E' => ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
        'F' => ["11111", "10000", "10000", "11110", "10000", "10000", "10000"],
        'G' => ["01110", "10001", "10000", "10111", "10001", "10001", "01111"],
        'H' => ["10001", "10001", "10001", "11111", "10001", "10001", "10001"],
        'I' => ["01110", "00100", "00100", "00100", "00100", "00100", "01110"],
        'J' => ["00111", "00010", "00010", "00010", "00010", "10010", "01100"],
        'K' => ["10001", "10010", "10100", "11000", "10100", "10010", "10001"],
        'L' => ["10000", "10000", "10000", "10000", "10000", "10000", "11111"],
        'M' => ["10001", "11011", "10101", "10101", "10001", "10001", "10001"],
        'N' => ["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
        'O' => ["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
        'P' => ["11110", "10001", "10001", "11110", "10000", "10000", "10000"],
        'Q' => ["01110", "10001", "10001", "10001", "10101", "10011", "01101"],
        'R' => ["11110", "10001", "10001", "11110", "10100", "10010", "10001"],
        'S' => ["01111", "10000", "10000", "01110", "00001", "00001", "11110"],
        'T' => ["11111", "00100", "00100", "00100", "00100", "00100", "00100"],
        'U' => ["10001", "10001", "10001", "10001", "10001", "10001", "01110"],
        'V' => ["10001", "10001", "10001", "10001", "10001", "01010", "00100"],
        'W' => ["10001", "10001", "10001", "10101", "10101", "11011", "10001"],
        'X' => ["10001", "10001", "01010", "00100", "01010", "10001", "10001"],
        'Y' => ["10001", "10001", "01010", "00100", "00100", "00100", "00100"],
        'Z' => ["11111", "00001", "00010", "00100", "01000", "10000", "11111"],
        '.' => ["00000", "00000", "00000", "00000", "00000", "01100", "01100"],
        '-' => ["00000", "00000", "00000", "11111", "00000", "00000", "00000"],
        ':' => ["00000", "01100", "01100", "00000", "01100", "01100", "00000"],
        '/' => ["00001", "00010", "00010", "00100", "01000", "01000", "10000"],
        '_' => ["00000", "00000", "00000", "00000", "00000", "00000", "11111"],
        '#' => ["01010", "01010", "11111", "01010", "11111", "01010", "01010"],
        '!' => ["00100", "00100", "00100", "00100", "00100", "00000", "00100"],
        ' ' => ["00000", "00000", "00000", "00000", "00000", "00000", "00000"],
        _ => return None,
    };
    let mut out = [0u8; 7];
    for (i, r) in rows.iter().enumerate() {
        out[i] = u8::from_str_radix(r, 2).unwrap_or(0);
    }
    Some(out)
}

#[allow(dead_code)]
fn _assert_rgba(_: Rgba<u8>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> Vec<u8> {
        let img = RgbaImage::from_pixel(540, 1200, Rgba([30, 30, 34, 255]));
        let mut buf: Vec<u8> = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn annotates_click_and_changes_pixels() {
        let png = sample_png();
        let m = Marker {
            kind: MarkerKind::Click,
            points: vec![(270, 600)],
            bounds: Some(Rect::new(40, 560, 500, 640)),
            number: 7,
            label: "CLICK".into(),
        };
        let out = annotate_png(&png, &[m]).unwrap();
        assert!(!out.is_empty());
        assert_ne!(out, png);
        let (w, h) = png_size(&out).unwrap();
        assert_eq!((w, h), (540, 1200));
        // 中心附近应出现红色像素
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        let px = img.get_pixel(270, 600);
        assert!(px.0[0] > 120, "{:?}", px);
    }

    #[test]
    fn annotates_swipe() {
        let png = sample_png();
        let m = Marker {
            kind: MarkerKind::Swipe,
            points: vec![(270, 900), (270, 300)],
            bounds: None,
            number: 3,
            label: "SWIPE".into(),
        };
        let out = annotate_png(&png, &[m]).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        let px = img.get_pixel(270, 600);
        assert!(px.0[2] > 120 || px.0[0] > 60, "{:?}", px);
    }

    #[test]
    fn glyph_bits_ok() {
        assert_eq!(glyph('A').unwrap()[0] >> 3, 0b1);
        assert!(glyph('中').is_none());
    }
}
