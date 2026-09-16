//! `crepus web dev` / `crepus web build` docs hook (same CLI as crepuscularity `web_docs_hook`).

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pulldown_cmark::{html, Options, Parser};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ThemeCss {
    accent: String,
    #[allow(dead_code)]
    accent_soft: String,
    surface: String,
    text: String,
    muted: String,
    border: String,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let docs_src = arg_value(&args, "--docs-src")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --docs-src"))?;
    let out_dir = arg_value(&args, "--out-dir")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --out-dir"))?;
    let site_name = arg_value(&args, "--site-name").unwrap_or_else(|| "inauguration".into());
    let theme_json = arg_value(&args, "--theme-json").unwrap_or_default();
    let theme: ThemeCss = if theme_json.is_empty() {
        ThemeCss {
            accent: "#3b82f6".into(),
            accent_soft: "#60a5fa".into(),
            surface: "#09090b".into(),
            text: "#f4f4f5".into(),
            muted: "#a1a1aa".into(),
            border: "#27272a".into(),
        }
    } else {
        serde_json::from_str(&theme_json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("theme-json: {e}")))?
    };

    generate_docs(
        Path::new(&docs_src),
        Path::new(&out_dir),
        &theme,
        &site_name,
    )?;
    Ok(())
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// Sidebar sections (label, page key, short nav label).
const NAV_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Language & IR",
        &[
            ("in-language", "inlang (.in)"),
            ("multi-frontend-ir", "Multi-front Core IR"),
            ("languages", "Language fronts"),
            ("parser-surface", "Parser surface"),
        ],
    ),
    (
        "Compiler",
        &[
            ("general-compiler", "General compiler"),
            ("native-backend", "Native / JIT"),
            ("docs-site", "Docs-site"),
        ],
    ),
    ("Project", &[("changelog", "Changelog")]),
    (
        "Benchmarks",
        &[
            ("benchmarks/README", "Overview"),
            ("benchmarks/jit", "JIT"),
            ("benchmarks/polyglot-compilers", "Polyglot vs native"),
            ("benchmarks/self-host-vs-native", "Self-host vs rustc"),
        ],
    ),
];

fn generate_docs(
    src_dir: &Path,
    out_dir: &Path,
    theme: &ThemeCss,
    site_name: &str,
) -> io::Result<()> {
    if !src_dir.is_dir() {
        return Ok(());
    }

    let mut files: Vec<(String, String)> = Vec::new();
    collect_markdown(src_dir, src_dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let pages_with_body: Vec<(String, String, String)> = files
        .iter()
        .map(|(stem, content)| {
            let (title, body_html) = render_md(content);
            (stem.clone(), title, body_html)
        })
        .collect();

    let pages: Vec<(String, String)> = pages_with_body
        .iter()
        .map(|(stem, title, _)| (stem.clone(), title.clone()))
        .collect();

    fs::create_dir_all(out_dir)?;
    for (web_key, title, body_html) in &pages_with_body {
        let nav = render_nav(&pages, web_key);
        let page = render_shell(body_html, title, &nav, theme, site_name, web_key);
        let out_path = out_dir.join(format!("{web_key}.html"));
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out_path, page)?;
    }

    if pages.is_empty() {
        return Ok(());
    }

    let idx: Vec<serde_json::Value> = pages
        .iter()
        .map(|(web_key, title)| serde_json::json!({"path": format!("{web_key}.html"), "title": title}))
        .collect();
    fs::write(
        out_dir.join("docs-search-index.json"),
        serde_json::to_string_pretty(&serde_json::json!(idx))?,
    )?;

    fs::write(
        out_dir.join("index.html"),
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0; url=./in-language.html">
<link rel="canonical" href="./in-language.html">
<title>Documentation — inauguration</title>
</head>
<body><p><a href="./in-language.html">Documentation</a></p></body>
</html>
"#,
    )?;

    Ok(())
}

use rayon::prelude::*;

fn collect_paths(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    if rel.components().any(|c| c.as_os_str() == "internal") {
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            collect_paths(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let web_key = rel
            .with_extension("")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if web_key == "README" {
            continue;
        }
        out.push(path);
    }
    Ok(())
}

fn collect_markdown(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> io::Result<()> {
    let mut paths = Vec::new();
    collect_paths(root, dir, &mut paths)?;

    let local_out: Result<Vec<(String, String)>, io::Error> = paths
        .into_par_iter()
        .map(|path| {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let web_key = rel
                .with_extension("")
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let content = fs::read_to_string(&path)?;
            Ok((web_key, content))
        })
        .collect();

    let mut res = local_out?;
    out.append(&mut res);

    Ok(())
}

fn render_md(md: &str) -> (String, String) {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_GFM);

    let mut title = String::new();
    for line in md.lines() {
        if line.starts_with("# ") {
            title = line[2..].trim().to_string();
            break;
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(md, opts));
    if title.is_empty() {
        title = "Documentation".into();
    }
    (title, body)
}

fn doc_href(from_key: &str, to_key: &str) -> String {
    let from_depth = from_key.matches('/').count();
    let to_depth = to_key.matches('/').count();
    if from_depth == 0 && to_depth == 0 {
        return format!("{to_key}.html");
    }
    if from_depth == 0 && to_depth > 0 {
        return format!("{to_key}.html");
    }
    if from_depth > 0 && to_depth == 0 {
        return format!("../{to_key}.html");
    }
    let (from_dir, _) = from_key.rsplit_once('/').unwrap_or(("", from_key));
    let (to_dir, to_file) = to_key.rsplit_once('/').unwrap_or(("", to_key));
    if from_dir == to_dir {
        return format!("{to_file}.html");
    }
    format!("../{to_key}.html")
}

fn brand_href(web_key: &str) -> String {
    let ups = web_key.matches('/').count() + 1;
    format!("{}index.html", "../".repeat(ups))
}

fn nav_link_class(key: &str, current: &str) -> String {
    let nested = key.starts_with("benchmarks/") && key != "benchmarks/README";
    if key == current {
        if nested {
            return " class=\"active doc-nav-nested\"".into();
        }
        return " class=\"active\"".into();
    }
    if nested {
        return " class=\"doc-nav-nested\"".into();
    }
    String::new()
}

fn render_nav(pages: &[(String, String)], current: &str) -> String {
    let present: HashSet<&str> = pages.iter().map(|(k, _)| k.as_str()).collect();

    let mut out = String::from("<nav aria-label=\"Documentation\" class=\"doc-nav\">");
    for (section, entries) in NAV_SECTIONS {
        let mut section_items = String::new();
        for (key, label) in *entries {
            if !present.contains(key) {
                continue;
            }
            let href = doc_href(current, key);
            let cls = nav_link_class(key, current);
            section_items.push_str(&format!(
                "<li><a href=\"{href}\"{cls}>{}</a></li>",
                esc(label)
            ));
        }
        if section_items.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "<div class=\"doc-nav-section\">{}</div><ul class=\"doc-nav-list\">{section_items}</ul>",
            esc(section)
        ));
    }

    let listed: HashSet<&str> = NAV_SECTIONS
        .iter()
        .flat_map(|(_, e)| e.iter().map(|(k, _)| *k))
        .collect();
    let mut extras = String::new();
    for (web_key, page_title) in pages {
        if listed.contains(web_key.as_str()) {
            continue;
        }
        let href = doc_href(current, web_key);
        let cls = nav_link_class(&web_key, current);
        extras.push_str(&format!(
            "<li><a href=\"{href}\"{cls}>{}</a></li>",
            esc(page_title)
        ));
    }
    if !extras.is_empty() {
        out.push_str(&format!(
            "<div class=\"doc-nav-section\">More</div><ul class=\"doc-nav-list\">{extras}</ul>"
        ));
    }

    out.push_str("</nav>");
    out
}

fn render_shell(
    body: &str,
    title: &str,
    nav: &str,
    theme: &ThemeCss,
    site_name: &str,
    web_key: &str,
) -> String {
    let ttl = esc(title);
    let site = esc(site_name);
    let a = esc(&theme.accent);
    let s = esc(&theme.surface);
    let t = esc(&theme.text);
    let m = esc(&theme.muted);
    let b = esc(&theme.border);
    let brand = esc(&brand_href(web_key));

    format!(
        "<!DOCTYPE html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<title>{ttl} — {site}</title>
<style>
  *{{box-sizing:border-box}}
  body{{margin:0;min-height:100vh;background:{s};color:{t};font-family:\"JetBrains Mono\",ui-monospace,monospace;-webkit-font-smoothing:antialiased;line-height:1.6}}
  a{{color:color-mix(in srgb,{t} 88%,transparent);text-decoration:none}}
  a:hover{{color:{t};text-decoration:underline;text-underline-offset:3px}}
  .doc-shell{{display:grid;grid-template-columns:minmax(220px,280px) 1fr;min-height:100vh}}
  aside{{position:sticky;top:0;height:100vh;overflow-y:auto;padding:1.5rem;border-right:1px solid {b};background:color-mix(in srgb,{s} 92%,white 8%)}}
  .brand{{font-weight:700;font-size:.95rem;color:{t};display:block;margin-bottom:1rem}}
  .brand:hover{{opacity:.85;text-decoration:none}}
  .doc-nav{{font-size:.875rem}}
  .doc-nav-section{{margin-top:1.1rem;font-size:.7rem;font-weight:700;letter-spacing:.08em;text-transform:uppercase;color:{m};opacity:.9}}
  .doc-nav-section:first-of-type{{margin-top:0}}
  .doc-nav-list{{list-style:none;padding:0;margin:.35rem 0 0}}
  .doc-nav-list li{{margin:.28rem 0}}
  .doc-nav a{{color:{m}}}
  .doc-nav a.doc-nav-nested{{padding-left:.65rem;display:inline-block}}
  .doc-nav a.active{{color:{a};font-weight:600}}
  .doc-nav a:hover{{color:{t}}}
  article{{max-width:45rem;margin:0 auto;padding:1.5rem 2rem 3rem}}
  article h1,h2,h3{{color:{t}}}
  article h1{{font-size:1.75rem}}
  article h2{{font-size:1.3rem;border-bottom:1px solid {b};padding-bottom:.35rem}}
  article p,li{{color:{m}}}
  article code{{font-family:\"JetBrains Mono\",monospace;font-size:.85em;background:color-mix(in srgb,{t} 10%,transparent);padding:.12rem .35rem;border-radius:4px}}
  article pre{{background:color-mix(in srgb,{t} 6%,{s});border:1px solid {b};border-radius:8px;padding:1rem;overflow-x:auto;font-size:.82rem}}
  article pre code{{background:none;padding:0}}
  article blockquote{{margin:1rem 0;padding:.5rem 1rem;border-left:3px solid {a};background:color-mix(in srgb,{a} 8%,transparent);border-radius:0 8px 8px 0}}
  article table{{width:100%;border-collapse:collapse;margin:1rem 0;font-size:.875rem}}
  article th,td{{padding:.5rem .75rem;border:1px solid {b}}}
  article th{{background:color-mix(in srgb,{t} 6%,transparent);color:{t}}}
  @media(max-width:640px){{.doc-shell{{grid-template-columns:1fr}}aside{{position:static;height:auto;border-right:none;border-bottom:1px solid {b}}}}}
</style>
</head>
<body>
<div class=\"doc-shell\">
<aside><a class=\"brand\" href=\"{brand}\">{site}</a>{nav}</aside>
<article>{body}</article>
</div>
</body>
</html>"
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn dummy_theme() -> ThemeCss {
        ThemeCss {
            accent: "accent".into(),
            accent_soft: "accent_soft".into(),
            surface: "surface".into(),
            text: "text".into(),
            muted: "muted".into(),
            border: "border".into(),
        }
    }

    struct TempDirGuard {
        path: PathBuf,
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn make_temp_dir(name: &str) -> (PathBuf, TempDirGuard) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("inauguration_docs_gen_test_{}_{}", name, now));
        fs::create_dir_all(&path).unwrap();
        let guard = TempDirGuard { path: path.clone() };
        (path, guard)
    }

    #[test]
    fn test_generate_docs_missing_src_dir() -> io::Result<()> {
        let (out_dir, _out_guard) = make_temp_dir("out_missing");
        let src_dir = std::env::temp_dir().join(format!(
            "docs_missing_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let theme = dummy_theme();
        let res = generate_docs(&src_dir, &out_dir, &theme, "test site");

        // The function returns early with Ok(()) if src_dir is not a directory
        assert!(res.is_ok());

        // Out dir shouldn't have files written to it
        let entries = fs::read_dir(&out_dir)?.count();
        assert_eq!(entries, 0);

        Ok(())
    }

    #[test]
    fn test_generate_docs_empty_dir() -> io::Result<()> {
        let (src_dir, _src_guard) = make_temp_dir("src_empty");
        let (out_dir, _out_guard) = make_temp_dir("out_empty");

        let theme = dummy_theme();
        generate_docs(&src_dir, &out_dir, &theme, "test site")?;

        // Should NOT create docs-search-index.json or index.html if pages is empty
        assert!(!out_dir.join("docs-search-index.json").exists());
        assert!(!out_dir.join("index.html").exists());

        Ok(())
    }

    #[test]
    fn test_generate_docs_happy_path() -> io::Result<()> {
        let (src_dir, _src_guard) = make_temp_dir("src_happy");
        let (out_dir, _out_guard) = make_temp_dir("out_happy");

        fs::write(src_dir.join("README.md"), "# Readme\nThis is a readme")?;
        fs::write(src_dir.join("hello.md"), "# Hello\nWorld")?;

        let sub_dir = src_dir.join("sub");
        fs::create_dir_all(&sub_dir)?;
        fs::write(sub_dir.join("test.md"), "# Test\nTesting 123")?;

        let internal_dir = src_dir.join("internal");
        fs::create_dir_all(&internal_dir)?;
        fs::write(internal_dir.join("secret.md"), "# Secret\nDo not read")?;

        let theme = dummy_theme();
        generate_docs(&src_dir, &out_dir, &theme, "test site")?;

        assert!(out_dir.join("docs-search-index.json").exists());
        assert!(out_dir.join("index.html").exists());
        assert!(out_dir.join("hello.html").exists());
        assert!(out_dir.join("sub").join("test.html").exists());
        assert!(!out_dir.join("README.html").exists());
        assert!(!out_dir.join("internal").exists());

        let hello_html = fs::read_to_string(out_dir.join("hello.html"))?;
        assert!(hello_html.contains("Hello"));
        assert!(hello_html.contains("World"));
        assert!(hello_html.contains("test site"));

        let test_html = fs::read_to_string(out_dir.join("sub").join("test.html"))?;
        assert!(test_html.contains("Test"));
        assert!(test_html.contains("Testing 123"));

        let search_index = fs::read_to_string(out_dir.join("docs-search-index.json"))?;
        let idx: Vec<serde_json::Value> = serde_json::from_str(&search_index).unwrap();
        assert_eq!(idx.len(), 2);

        Ok(())
    }

    #[test]
    fn test_render_nav_basic_sections() {
        let pages = vec![
            ("in-language".to_string(), "inlang (.in)".to_string()),
            (
                "multi-frontend-ir".to_string(),
                "Multi-front Core IR".to_string(),
            ),
        ];
        let nav = render_nav(&pages, "in-language");

        assert!(nav.starts_with("<nav aria-label=\"Documentation\" class=\"doc-nav\">"));
        assert!(nav.ends_with("</nav>"));
        assert!(nav.contains("<div class=\"doc-nav-section\">Language &amp; IR</div>"));
        assert!(
            nav.contains("<li><a href=\"in-language.html\" class=\"active\">inlang (.in)</a></li>")
        );
        assert!(nav.contains("<li><a href=\"multi-frontend-ir.html\">Multi-front Core IR</a></li>"));
        assert!(!nav.contains("Benchmarks"));
    }

    #[test]
    fn test_render_nav_more_section() {
        let pages = vec![
            ("in-language".to_string(), "inlang (.in)".to_string()),
            ("custom-page".to_string(), "Custom Page Title".to_string()),
        ];
        let nav = render_nav(&pages, "in-language");

        assert!(nav.contains("<div class=\"doc-nav-section\">More</div>"));
        assert!(nav.contains("<li><a href=\"custom-page.html\">Custom Page Title</a></li>"));
    }

    #[test]
    fn test_render_nav_empty_pages() {
        let pages: Vec<(String, String)> = vec![];
        let nav = render_nav(&pages, "in-language");
        assert_eq!(
            nav,
            "<nav aria-label=\"Documentation\" class=\"doc-nav\"></nav>"
        );
    }

    #[test]
    fn test_render_nav_nested_class() {
        let pages = vec![
            ("benchmarks/README".to_string(), "Overview".to_string()),
            ("benchmarks/jit".to_string(), "JIT".to_string()),
        ];

        let nav = render_nav(&pages, "benchmarks/README");
        assert!(nav.contains("<li><a href=\"README.html\" class=\"active\">Overview</a></li>"));
        assert!(nav.contains("<li><a href=\"jit.html\" class=\"doc-nav-nested\">JIT</a></li>"));

        let nav_jit_active = render_nav(&pages, "benchmarks/jit");
        assert!(nav_jit_active
            .contains("<li><a href=\"jit.html\" class=\"active doc-nav-nested\">JIT</a></li>"));
    }
}
