//! HTML rewriting: injects cosmetic-filter CSS and scriptlet JS, and extracts
//! classes/ids for cosmetic filtering.

pub(crate) fn inject_into_html(html: &[u8], css: &str, script: &str) -> Option<Vec<u8>> {
    if css.is_empty() && script.is_empty() {
        return None;
    }
    let s = std::str::from_utf8(html).ok()?;
    let lower = s.to_ascii_lowercase();

    let style = (!css.is_empty()).then(|| format!("<style type=\"text/css\">{css}</style>"));
    let script_tag =
        (!script.is_empty()).then(|| format!("<script>try{{\n{script}\n}}catch(e){{}}</script>"));

    let mut out = String::with_capacity(s.len() + 256);
    let mut cursor = 0usize;

    if let Some(tag) = script_tag {
        let after_head = lower
            .find("<head")
            .and_then(|h| lower[h..].find('>').map(|g| h + g + 1));
        match after_head {
            Some(p) => {
                out.push_str(&s[..p]);
                out.push_str(&tag);
                cursor = p;
            }
            None => out.push_str(&tag),
        }
    }

    if let Some(style) = style {
        let pos = lower[cursor..]
            .find("</head>")
            .or_else(|| lower[cursor..].find("</body>"))
            .map(|p| cursor + p);
        match pos {
            Some(p) => {
                out.push_str(&s[cursor..p]);
                out.push_str(&style);
                cursor = p;
            }
            None => {
                out.push_str(&style);
            }
        }
    }

    out.push_str(&s[cursor..]);
    Some(out.into_bytes())
}

pub(crate) fn html_classes_and_ids(html: &str) -> (Vec<String>, Vec<String>) {
    let lower = html.to_ascii_lowercase();
    let mut classes = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for (attr, out, split) in [
        ("class", &mut classes, true),
        ("id", &mut ids, false),
    ] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(attr) {
            let start = from + p;
            from = start + attr.len();
            if !lower[..start].ends_with(|c: char| c.is_ascii_whitespace()) {
                continue;
            }
            let rest = &html[start + attr.len()..];
            let Some(rest) = rest.strip_prefix('=') else { continue };
            let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
                continue;
            };
            let value = &rest[1..];
            let Some(end) = value.find(quote) else { continue };
            let value = &value[..end];
            if split {
                out.extend(value.split_ascii_whitespace().map(str::to_string));
            } else if !value.is_empty() {
                out.insert(value.to_string());
            }
        }
    }
    (classes.into_iter().collect(), ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_class_and_id_collection_is_pragmatic() {
        let (mut classes, mut ids) = html_classes_and_ids(
            r#"<html><body>
                <div class="adbox banner_ads" id="Top">
                <DIV CLASS='textads'>
                <span data-id="not-an-id"班id="nope">
                <p id="">empty</p>
                <a class="adbox">dup</a>
            </body></html>"#,
        );
        classes.sort();
        ids.sort();
        assert_eq!(classes, vec!["adbox", "banner_ads", "textads"], "split, dedup, case kept");
        assert_eq!(ids, vec!["Top"], "data-id and empty ids are not ids");
    }

    #[test]
    fn inject_style_before_head_close() {
        let out =
            inject_into_html(b"<html><head></head><body>x</body></html>", ".ad{}", "").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "<html><head><style type=\"text/css\">.ad{}</style></head><body>x</body></html>"
        );
    }

    #[test]
    fn inject_style_falls_back_to_body_then_front() {
        let s = String::from_utf8(inject_into_html(b"<body>x</body>", "c", "").unwrap()).unwrap();
        assert!(s.starts_with("<body>x<style"));
        let s = String::from_utf8(inject_into_html(b"plain", "c", "").unwrap()).unwrap();
        assert!(s.starts_with("<style"));
        assert!(s.ends_with("plain"));
    }

    #[test]
    fn inject_script_runs_early_and_before_style() {
        let out = inject_into_html(
            b"<html><head><script>page()</script></head><body>x</body></html>",
            ".ad{}",
            "hook()",
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        let ours = s.find("hook()").unwrap();
        let page = s.find("page()").unwrap();
        let style = s.find("<style").unwrap();
        assert!(ours < page, "scriptlet must precede page script");
        assert!(ours < style, "scriptlet must precede style");
        assert!(s.contains("try{\nhook()\n}catch(e){}"), "wrapped: {s}");
    }

    #[test]
    fn inject_nothing_returns_none() {
        assert!(inject_into_html(b"<html></html>", "", "").is_none());
    }

    #[test]
    fn inject_leaves_non_utf8_alone() {
        assert!(inject_into_html(&[0xff, 0xfe, b'<'], "c", "s").is_none());
    }
}
