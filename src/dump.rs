//! `uiautomator dump` 输出的 XML 解析。
//!
//! 这里手写了一个极小的 XML 解析器（只处理 `<node ...>` / `<node ... />` / `</node>` /
//! `<hierarchy ...>`），目的是避免依赖体积与版本 API 漂移；dump XML 结构固定，够用且稳。

use crate::model::{Hierarchy, Rect, UiNode};
use anyhow::{bail, Result};

pub fn parse_hierarchy(xml: &str) -> Result<Hierarchy> {
    let bytes = xml.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    let mut h = Hierarchy::default();
    let mut stack: Vec<UiNode> = Vec::new(); // 未闭合的 node
    let mut node_count = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let start = i + 1;
        // 找到本标签结束的 '>'（跳过引号内的内容）
        let mut j = start;
        while j < len {
            if bytes[j] == b'"' {
                j += 1;
                while j < len && bytes[j] != b'"' {
                    j += 1;
                }
            }
            if j >= len {
                break;
            }
            if bytes[j] == b'>' {
                break;
            }
            j += 1;
        }
        if j >= len {
            break;
        }
        let raw = xml[start..j].trim();
        i = j + 1;

        if raw.starts_with('!') || raw.starts_with('?') {
            continue;
        }

        let (name, rest) = if raw.starts_with('/') {
            // 闭合标签
            (&raw[1..], "")
        } else {
            let self_closing = raw.ends_with('/');
            let body = if self_closing { raw.trim_end_matches('/') } else { raw };
            let mut it = body.splitn(2, char::is_whitespace);
            let n = it.next().unwrap_or("");
            let r = it.next().unwrap_or("");
            if self_closing {
                (n, r)
            } else {
                // 用 (name, attrs, is_close=false) 的形态继续处理
                (n, r)
            }
        };
        let is_close = raw.starts_with('/');
        let self_closing = !is_close && raw.ends_with('/');

        match name {
            "hierarchy" => {
                if !is_close {
                    let attrs = parse_attrs(rest);
                    if let Some(v) = attrs.get("width") {
                        h.width = v.parse().unwrap_or(0);
                    }
                    if let Some(v) = attrs.get("height") {
                        h.height = v.parse().unwrap_or(0);
                    }
                    if let Some(v) = attrs.get("rotation") {
                        h.rotation = v.parse().unwrap_or(0);
                    }
                }
            }
            "node" => {
                if is_close {
                    if let Some(node) = stack.pop() {
                        match stack.last_mut() {
                            Some(parent) => parent.children.push(node),
                            None => h.root = Some(node),
                        }
                    }
                } else {
                    let node = node_from_attrs(rest);
                    node_count += 1;
                    if self_closing {
                        match stack.last_mut() {
                            Some(parent) => parent.children.push(node),
                            None => h.root = Some(node),
                        }
                    } else {
                        stack.push(node);
                    }
                }
            }
            _ => {}
        }
    }

    // 未正常闭合的残留（dump 被截断时）
    while let Some(node) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => h.root = Some(node),
        }
    }

    if node_count == 0 {
        bail!("控件树为空（dump 内容中没有 node 节点）");
    }
    if h.width <= 0 || h.height <= 0 {
        // 老版本 dump 可能没有 width/height，用根节点 bounds 兜底
        if let Some(r) = &h.root {
            h.width = r.bounds.x2;
            h.height = r.bounds.y2;
        }
    }
    Ok(h)
}

fn node_from_attrs(rest: &str) -> UiNode {
    let attrs = parse_attrs(rest);
    let mut n = UiNode::default();
    n.index = attrs.get("index").and_then(|v| v.parse().ok()).unwrap_or(0);
    n.text = attrs.get("text").cloned().unwrap_or_default();
    n.resource_id = attrs.get("resource-id").cloned().unwrap_or_default();
    n.class = attrs.get("class").cloned().unwrap_or_default();
    n.package = attrs.get("package").cloned().unwrap_or_default();
    n.content_desc = attrs.get("content-desc").cloned().unwrap_or_default();
    n.bounds = attrs.get("bounds").and_then(|v| Rect::parse(v)).unwrap_or_default();
    n.clickable = attr_bool(&attrs, "clickable");
    n.long_clickable = attr_bool(&attrs, "long-clickable");
    n.scrollable = attr_bool(&attrs, "scrollable");
    n.checkable = attr_bool(&attrs, "checkable");
    n.checked = attr_bool(&attrs, "checked");
    // 缺省视为可用：部分设备的 dump 不输出 enabled 属性，
    // 若默认 false 会把整棵树都当成 disabled 而过滤掉（实测会吞掉全部候选动作）
    n.enabled = attrs.get("enabled").map(|v| v == "true").unwrap_or(true);
    n.focused = attr_bool(&attrs, "focused");
    n.selected = attr_bool(&attrs, "selected");
    n.password = attr_bool(&attrs, "password");
    n
}

fn attr_bool(attrs: &std::collections::HashMap<String, String>, k: &str) -> bool {
    attrs.get(k).map(|v| v == "true").unwrap_or(false)
}

/// 解析 `k="v" k2='v2'` 形式的属性，处理 5 种 XML 实体与数字实体
fn parse_attrs(rest: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        // 跳过空白
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        // 读 key
        let ks = i;
        while i < n && !chars[i].is_whitespace() && chars[i] != '=' {
            i += 1;
        }
        let key = chars[ks..i].iter().collect::<String>();
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            if !key.is_empty() {
                out.insert(key, String::new());
            }
            continue;
        }
        i += 1; // skip '='
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let quote = chars[i];
        let value = if quote == '"' || quote == '\'' {
            i += 1;
            let vs = i;
            while i < n && chars[i] != quote {
                i += 1;
            }
            let v: String = chars[vs..i].iter().collect();
            i += 1;
            v
        } else {
            let vs = i;
            while i < n && !chars[i].is_whitespace() {
                i += 1;
            }
            chars[vs..i].iter().collect()
        };
        out.insert(key, unescape(&value));
    }
    out
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        // 读取到 ';'
        let mut ent = String::new();
        let mut ok = false;
        for c2 in it.by_ref() {
            if c2 == ';' {
                ok = true;
                break;
            }
            ent.push(c2);
            if ent.len() > 8 {
                break;
            }
        }
        if !ok {
            out.push('&');
            out.push_str(&ent);
            continue;
        }
        match ent.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            other => {
                if let Some(hex) = other.strip_prefix("#x").or_else(|| other.strip_prefix("#X")) {
                    if let Ok(v) = u32::from_str_radix(hex, 16) {
                        if let Some(ch) = char::from_u32(v) {
                            out.push(ch);
                            continue;
                        }
                    }
                } else if let Some(dec) = other.strip_prefix('#') {
                    if let Ok(v) = dec.parse::<u32>() {
                        if let Some(ch) = char::from_u32(v) {
                            out.push(ch);
                            continue;
                        }
                    }
                }
                out.push('&');
                out.push_str(other);
                out.push(';');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0" width="1080" height="2400">
  <node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.demo" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,2400]">
    <node index="0" text="" resource-id="com.demo:id/recycler" class="androidx.recyclerview.widget.RecyclerView" package="com.demo" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="true" focused="false" scrollable="true" long-clickable="false" password="false" selected="false" bounds="[0,120][1080,2300]">
      <node index="0" text="订单 &amp; 物流" resource-id="com.demo:id/title" class="android.widget.TextView" package="com.demo" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[24,140][1056,240]" />
      <node index="1" text="设置" resource-id="com.demo:id/title" class="android.widget.TextView" package="com.demo" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[24,260][1056,360]" />
    </node>
    <node index="1" text="" resource-id="com.demo:id/input" class="android.widget.EditText" package="com.demo" content-desc="搜索框" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="true" password="false" selected="false" bounds="[40,2320][1040,2380]" />
  </node>
</hierarchy>
UI hierchary dumped to: /dev/tty"#;

    #[test]
    fn parses_sample() {
        let h = parse_hierarchy(SAMPLE).unwrap();
        assert_eq!(h.width, 1080);
        assert_eq!(h.height, 2400);
        let nodes = h.nodes();
        assert_eq!(nodes.len(), 5);
        let texts: Vec<&str> = nodes.iter().map(|n| n.text.as_str()).collect();
        assert!(texts.contains(&"订单 & 物流"));
        let tv = nodes.iter().find(|n| n.text == "设置").unwrap();
        assert!(tv.clickable);
        assert_eq!(tv.bounds, Rect::new(24, 260, 1056, 360));
        assert!(tv.click_point(&h.screen()).is_some());
        let et = nodes.iter().find(|n| n.is_edit_text()).unwrap();
        assert_eq!(et.content_desc, "搜索框");
        assert!(et.long_clickable);
        let rv = nodes.iter().find(|n| n.scrollable).unwrap();
        assert_eq!(rv.resource_id, "com.demo:id/recycler");
    }

    #[test]
    fn parses_self_closing_only() {
        let xml = r#"<hierarchy rotation="0" width="480" height="800"><node index="0" text="A" class="android.widget.Button" bounds="[0,0][100,50]" clickable="true" /></hierarchy>"#;
        let h = parse_hierarchy(xml).unwrap();
        assert_eq!(h.nodes().len(), 1);
        assert_eq!(h.nodes()[0].text, "A");
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_hierarchy("UI hierchary dumped to: /dev/tty").is_err());
    }
}
