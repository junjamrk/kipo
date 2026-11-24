use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use htmd::node::{Node, NodeType};
use htmd::options::{BulletListMarker, CodeBlockFence, CodeBlockStyle, HeadingStyle, Options};
use htmd::HtmlToMarkdown;
use kp_derive::{base_fact, Extractor, FunnelOptions, Memory};
use kp_model::{AnchorSpec, ContentHash, TrustTier, ValidityWindow};
use kp_store::{ChunkInput, RawSourceInput, Store};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::doc_router::AddedDocument;
use crate::{text, IngestError, Result, MAX_INPUT_BYTES};

pub(crate) const MAX_REDIRECT_HOPS: usize = 5;
pub(crate) const MAX_HTML_NESTING_DEPTH: usize = 256;

pub(crate) fn is_disallowed_ip(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_v4(v4, allow_loopback),
        IpAddr::V6(v6) => is_disallowed_v6(v6, allow_loopback),
    }
}

fn is_disallowed_v4(v4: Ipv4Addr, allow_loopback: bool) -> bool {
    if v4.is_loopback() {
        return !allow_loopback;
    }
    let o = v4.octets();
    o[0] == 0
        || v4.is_private()
        || v4.is_link_local()
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || v4.is_documentation()
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || v4.is_multicast()
        || o[0] >= 240
}

fn is_disallowed_v6(v6: Ipv6Addr, allow_loopback: bool) -> bool {
    if v6.is_loopback() {
        return !allow_loopback;
    }
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_disallowed_v4(v4, allow_loopback);
    }
    let s = v6.segments();
    if s[0..6] == [0, 0, 0, 0, 0, 0] {
        let embedded = Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        );
        return is_disallowed_v4(embedded, allow_loopback);
    }
    if (s[0..6] == [0x0064, 0xff9b, 0, 0, 0, 0]) || (s[0..3] == [0x0064, 0xff9b, 1]) {
        let embedded = Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        );
        return is_disallowed_v4(embedded, allow_loopback);
    }
    if (s[0] & 0xe000) != 0x2000 {
        return true;
    }
    (s[0] == 0x2001
        && (s[1] == 0x0000
            || (s[1] == 0x0002 && s[2] == 0x0000)
            || s[1] == 0x0db8
            || (s[1] & 0xfff0) == 0x0010
            || (s[1] & 0xfff0) == 0x0020))
        || s[0] == 0x2002
}

fn is_disallowed_hostname(raw_host: &str, allow_loopback: bool) -> bool {
    let h = raw_host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return !allow_loopback;
    }
    h.ends_with(".local") || h.ends_with(".internal") || h.ends_with(".home.arpa")
}

#[derive(Debug)]
struct SsrfSafeResolver {
    allow_loopback: bool,
}

impl Resolve for SsrfSafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let host = name.as_str().trim_end_matches('.');
            if is_disallowed_hostname(host, allow_loopback) {
                return Err("hostname blocked by SSRF guard".into());
            }
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
                .await?
                .map(|sa| SocketAddr::new(sa.ip(), 0))
                .collect();
            if resolved.is_empty()
                || resolved
                    .iter()
                    .any(|sa| is_disallowed_ip(sa.ip(), allow_loopback))
            {
                return Err("DNS resolved to private or special-purpose IP".into());
            }
            let addrs: Addrs = Box::new(resolved.into_iter());
            Ok(addrs)
        })
    }
}

fn validate_hop_url(url: &mut reqwest::Url, allow_loopback: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(IngestError::Invalid(
            "URL must be http or https without credentials",
        ));
    }
    url.set_fragment(None);
    let host = url
        .host_str()
        .ok_or(IngestError::Invalid("URL missing host"))?;
    if is_disallowed_hostname(host, allow_loopback) {
        return Err(IngestError::Invalid(
            "loopback or local-domain URL blocked by SSRF guard",
        ));
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if is_disallowed_ip(ip, allow_loopback) {
            return Err(IngestError::Invalid(
                "private or special-purpose IP URL blocked by SSRF guard",
            ));
        }
    }
    Ok(())
}

fn resolve_redirect_hop(
    current: &reqwest::Url,
    loc_hdr: &str,
    allow_loopback: bool,
) -> Result<reqwest::Url> {
    let mut next = current
        .join(loc_hdr.trim())
        .map_err(|_| IngestError::Invalid("invalid redirect Location URL"))?;
    if current.scheme() == "https" && next.scheme() == "http" {
        return Err(IngestError::Invalid(
            "HTTPS to HTTP redirect downgrade blocked",
        ));
    }
    validate_hop_url(&mut next, allow_loopback)?;
    Ok(next)
}

async fn fetch_url_ssrf_safe(
    raw_url: &str,
    allow_loopback: bool,
) -> Result<(reqwest::Url, reqwest::Response)> {
    let mut current = reqwest::Url::parse(raw_url.trim())
        .map_err(|_| IngestError::Invalid("invalid HTTP/HTTPS URL"))?;
    validate_hop_url(&mut current, allow_loopback)?;

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(SsrfSafeResolver { allow_loopback }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent("kipo-ingest/0.1")
        .build()
        .map_err(|_| IngestError::Invalid("HTTP client initialization failed"))?;

    let mut visited = BTreeSet::new();
    for hop in 0..=MAX_REDIRECT_HOPS {
        if !visited.insert(current.as_str().to_owned()) {
            return Err(IngestError::Invalid("HTTP redirect loop detected"));
        }
        let resp =
            client.get(current.clone()).send().await.map_err(|_| {
                IngestError::Invalid("URL fetch failed or blocked by SSRF DNS guard")
            })?;

        if matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if hop == MAX_REDIRECT_HOPS {
                return Err(IngestError::Invalid("URL exceeded max 5 redirect hops"));
            }
            let loc_hdr = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(IngestError::Invalid("redirect missing Location header"))?;
            current = resolve_redirect_hop(&current, loc_hdr, allow_loopback)?;
            continue;
        }

        if !resp.status().is_success() {
            return Err(IngestError::Invalid("URL returned non-success HTTP status"));
        }
        return Ok((current, resp));
    }
    Err(IngestError::Invalid("URL exceeded max 5 redirect hops"))
}

fn is_invisible_injection_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{E0000}'..='\u{E007F}'
            | '\u{200B}'..='\u{200D}'
            | '\u{2060}'
            | '\u{FEFF}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn scrub_invisible_unicode(input: &str) -> String {
    if !input.chars().any(is_invisible_injection_char) {
        return input.to_owned();
    }
    input
        .chars()
        .filter(|c| !is_invisible_injection_char(*c))
        .collect()
}

fn is_void_html_tag(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

fn check_html_nesting_depth(html: &str, max_depth: usize) -> Result<()> {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut stack: Vec<String> = Vec::with_capacity(64);
    let mut i = 0usize;

    while i < len {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            match bytes[i + 4..].windows(3).position(|w| w == b"-->") {
                Some(rel) => i += 4 + rel + 3,
                None => break,
            }
            continue;
        }
        if bytes.get(i + 1).is_some_and(|b| matches!(b, b'!' | b'?')) {
            match bytes[i + 2..].iter().position(|&b| b == b'>') {
                Some(rel) => i += 2 + rel + 1,
                None => break,
            }
            continue;
        }

        let mut cur = i + 1;
        let is_closing = if bytes.get(cur) == Some(&b'/') {
            cur += 1;
            true
        } else {
            false
        };
        while cur < len && bytes[cur].is_ascii_whitespace() {
            cur += 1;
        }
        let name_start = cur;
        while cur < len
            && (bytes[cur].is_ascii_alphanumeric() || matches!(bytes[cur], b'-' | b':' | b'_'))
        {
            cur += 1;
        }
        if cur == name_start {
            i += 1;
            continue;
        }
        let tag_lower = String::from_utf8_lossy(&bytes[name_start..cur]).to_ascii_lowercase();

        let mut quote: Option<u8> = None;
        let mut tag_end = None;
        let mut k = cur;
        while k < len {
            let b = bytes[k];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b'>' {
                tag_end = Some(k);
                break;
            }
            k += 1;
        }
        let Some(end_gt) = tag_end else {
            break;
        };
        let mut before_gt = end_gt;
        while before_gt > cur && bytes[before_gt - 1].is_ascii_whitespace() {
            before_gt -= 1;
        }
        let self_closing = before_gt > cur && bytes[before_gt - 1] == b'/';
        i = end_gt + 1;

        if is_closing {
            if let Some(pos) = stack.iter().rposition(|t| *t == tag_lower) {
                stack.truncate(pos);
            }
            continue;
        }
        if self_closing || is_void_html_tag(&tag_lower) {
            continue;
        }
        if matches!(
            tag_lower.as_str(),
            "script" | "style" | "textarea" | "title" | "xmp"
        ) {
            let close_pat = format!("</{tag_lower}").into_bytes();
            if let Some(rel) = find_ascii_case_insensitive(&bytes[i..], &close_pat) {
                let after = i + rel + close_pat.len();
                if let Some(gt) = bytes[after..].iter().position(|&b| b == b'>') {
                    i = after + gt + 1;
                    continue;
                }
            }
            break;
        }

        if matches!(
            tag_lower.as_str(),
            "p" | "li" | "dt" | "dd" | "option" | "tr" | "td" | "th"
        ) && stack.last().is_some_and(|top| *top == tag_lower)
        {
            stack.pop();
        }
        stack.push(tag_lower);
        if stack.len() > max_depth {
            return Err(IngestError::Invalid(
                "HTML nesting depth exceeds safety limit",
            ));
        }
    }
    Ok(())
}

fn is_pruned_tag(tag: &str) -> bool {
    matches!(
        tag,
        "head"
            | "title"
            | "script"
            | "style"
            | "noscript"
            | "template"
            | "svg"
            | "canvas"
            | "iframe"
            | "frame"
            | "object"
            | "embed"
            | "audio"
            | "video"
            | "nav"
            | "footer"
            | "dialog"
            | "form"
            | "button"
            | "select"
            | "textarea"
            | "input"
    )
}

fn has_pruned_role(node: &Node<'_>) -> bool {
    node.attr("role").is_some_and(|r| {
        matches!(
            r.trim().to_ascii_lowercase().as_str(),
            "navigation" | "banner" | "contentinfo" | "dialog" | "search"
        )
    })
}

fn is_zero_css_number(val: &str) -> bool {
    let trimmed = val.trim().trim_start_matches(['+', '-']);
    if trimmed.is_empty() {
        return false;
    }
    let num_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    if num_end == 0 {
        return false;
    }
    trimmed[..num_end]
        .parse::<f64>()
        .is_ok_and(|v| v.abs() == 0.0)
}

fn is_large_negative_px(val: &str) -> bool {
    let trimmed = val.trim();
    let Some(rest) = trimmed.strip_prefix('-') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits >= 4
}

fn has_hidden_inline_css(style: &str) -> bool {
    let mut height_zero = false;
    let mut overflow_hidden = false;

    for decl in style.split(';') {
        let Some((raw_prop, raw_val)) = decl.split_once(':') else {
            continue;
        };
        let prop = raw_prop.trim().to_ascii_lowercase();
        let val_full = raw_val.trim().to_ascii_lowercase();
        let val = val_full
            .strip_suffix("!important")
            .map(str::trim)
            .unwrap_or(val_full.as_str());

        match prop.as_str() {
            "display" if val == "none" => return true,
            "visibility" if matches!(val, "hidden" | "collapse") => return true,
            "opacity" if is_zero_css_number(val) => return true,
            "font-size" if is_zero_css_number(val) => return true,
            "height" | "max-height" if is_zero_css_number(val) => height_zero = true,
            "overflow" | "overflow-y" if val.split_whitespace().any(|w| w == "hidden") => {
                overflow_hidden = true;
            }
            "clip" if val.replace(' ', "").starts_with("rect(0") => return true,
            "clip-path" if val.replace(' ', "").starts_with("inset(100%") => return true,
            "left" | "text-indent" if is_large_negative_px(val) => return true,
            _ => {}
        }
    }
    height_zero && overflow_hidden
}

fn is_hidden_or_pruned_element(node: &Node<'_>) -> bool {
    let Some(tag) = node.tag_name() else {
        return false;
    };
    if is_pruned_tag(tag) || has_pruned_role(node) {
        return true;
    }
    if node
        .attr("hidden")
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("false"))
    {
        return true;
    }
    if node
        .attr("inert")
        .is_some_and(|v| !v.trim().eq_ignore_ascii_case("false"))
    {
        return true;
    }
    if node
        .attr("aria-hidden")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    {
        return true;
    }
    node.attr("style")
        .is_some_and(|s| has_hidden_inline_css(&s))
}

fn push_children_rev<'a>(node: &Node<'a>, stack: &mut Vec<Node<'a>>) {
    let mut cur = node.last_child();
    while let Some(ch) = cur {
        let prev = ch.prev_sibling();
        stack.push(ch);
        cur = prev;
    }
}

fn children_of<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cur = node.first_child();
    while let Some(ch) = cur {
        let next = ch.next_sibling();
        out.push(ch);
        cur = next;
    }
    out
}

fn collect_plain_text_iterative(root: &Node<'_>) -> String {
    let mut out = String::new();
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        match node.node_type() {
            NodeType::Text => {
                if let Some(t) = node.text() {
                    out.push_str(&t);
                }
            }
            NodeType::Element | NodeType::Document => {
                push_children_rev(&node, &mut stack);
            }
            _ => {}
        }
    }
    out
}

fn count_visible_non_ws_chars(root: &Node<'_>) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        match node.node_type() {
            NodeType::Text => {
                if let Some(t) = node.text() {
                    count += t
                        .chars()
                        .filter(|c| !c.is_whitespace() && !is_invisible_injection_char(*c))
                        .count();
                }
            }
            NodeType::Element => {
                if is_hidden_or_pruned_element(&node) {
                    continue;
                }
                push_children_rev(&node, &mut stack);
            }
            NodeType::Document => {
                push_children_rev(&node, &mut stack);
            }
            _ => {}
        }
    }
    count
}

struct DomAudit<'a> {
    title: String,
    body_node: Option<Node<'a>>,
    main_candidates: Vec<Node<'a>>,
    role_main_candidates: Vec<Node<'a>>,
    article_candidates: Vec<Node<'a>>,
    doc_class_candidates: Vec<Node<'a>>,
    noscript_text: String,
    external_or_module_scripts: usize,
    has_bot_marker: bool,
    has_empty_spa_mount: bool,
}

fn audit_dom_iterative<'a>(root: &Node<'a>) -> DomAudit<'a> {
    let mut audit = DomAudit {
        title: String::new(),
        body_node: None,
        main_candidates: Vec::new(),
        role_main_candidates: Vec::new(),
        article_candidates: Vec::new(),
        doc_class_candidates: Vec::new(),
        noscript_text: String::new(),
        external_or_module_scripts: 0,
        has_bot_marker: false,
        has_empty_spa_mount: false,
    };

    let mut stack: Vec<(Node<'a>, bool)> = vec![(root.clone(), false)];
    while let Some((node, ancestor_hidden)) = stack.pop() {
        if node.node_type() == NodeType::Element {
            let tag = node.tag_name().unwrap_or("");
            let id_lower = node
                .attr("id")
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_default();
            let class_lower = node
                .attr("class")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();

            if id_lower == "challenge-running"
                || class_lower.contains("cf-browser-verification")
                || class_lower.contains("cf-turnstile")
                || class_lower.contains("turnstile")
                || class_lower.contains("g-recaptcha")
                || class_lower.contains("h-captcha")
                || id_lower.contains("turnstile")
                || id_lower.contains("g-recaptcha")
                || id_lower.contains("h-captcha")
            {
                audit.has_bot_marker = true;
            }

            match tag {
                "title" if audit.title.is_empty() => {
                    let raw = scrub_invisible_unicode(&collect_plain_text_iterative(&node));
                    audit.title = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                    let t_low = audit.title.to_ascii_lowercase();
                    if t_low == "just a moment..."
                        || t_low.contains("attention required! | cloudflare")
                    {
                        audit.has_bot_marker = true;
                    }
                    continue;
                }
                "noscript" => {
                    audit
                        .noscript_text
                        .push_str(&collect_plain_text_iterative(&node));
                    audit.noscript_text.push(' ');
                    continue;
                }
                "script" => {
                    let has_src = node.attr("src").is_some_and(|s| !s.trim().is_empty());
                    let is_module = node
                        .attr("type")
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("module"));
                    if has_src || is_module {
                        audit.external_or_module_scripts += 1;
                    }
                    let script_body = collect_plain_text_iterative(&node);
                    if script_body.contains("__cf_chl_opt") || script_body.contains("_cf_chl_opt") {
                        audit.has_bot_marker = true;
                    }
                    continue;
                }
                "body" if audit.body_node.is_none() => {
                    audit.body_node = Some(node.clone());
                }
                _ => {}
            }

            let self_hidden = ancestor_hidden || is_hidden_or_pruned_element(&node);
            if !self_hidden {
                if (tag == "app-root"
                    || matches!(
                        id_lower.as_str(),
                        "root"
                            | "app"
                            | "__next"
                            | "__nuxt"
                            | "svelte"
                            | "q-app"
                            | "gatsby-focus-wrapper"
                    ))
                    && count_visible_non_ws_chars(&node) < 32
                {
                    audit.has_empty_spa_mount = true;
                }

                if tag == "main" {
                    audit.main_candidates.push(node.clone());
                } else if node
                    .attr("role")
                    .is_some_and(|r| r.trim().eq_ignore_ascii_case("main"))
                {
                    audit.role_main_candidates.push(node.clone());
                } else if tag == "article" {
                    audit.article_candidates.push(node.clone());
                } else if id_lower == "main-content"
                    || class_lower.split_ascii_whitespace().any(|c| {
                        matches!(
                            c,
                            "markdown-body" | "theme-doc-markdown" | "rst-content" | "doc-content"
                        )
                    })
                {
                    audit.doc_class_candidates.push(node.clone());
                }
            }

            for ch in children_of(&node).into_iter().rev() {
                stack.push((ch, self_hidden));
            }
        } else if node.node_type() == NodeType::Document {
            for ch in children_of(&node).into_iter().rev() {
                stack.push((ch, ancestor_hidden));
            }
        }
    }
    audit
}

fn extract_lang_from_str(s: &str) -> Option<String> {
    for tok in s.split_ascii_whitespace() {
        for prefix in ["language-", "lang-", "highlight-source-", "brush:"] {
            if let Some(rest) = tok.strip_prefix(prefix) {
                let clean: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                    .collect();
                if !clean.is_empty() && clean != "none" && clean != "text" {
                    return Some(clean.to_ascii_lowercase());
                }
            }
        }
    }
    None
}

fn detect_pre_language(pre_node: &Node<'_>) -> Option<String> {
    for attr_name in ["data-language", "data-lang", "lang"] {
        if let Some(val) = pre_node.attr(attr_name) {
            let clean: String = val
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                .collect();
            if !clean.is_empty() {
                return Some(clean.to_ascii_lowercase());
            }
        }
    }
    if let Some(cls) = pre_node.attr("class") {
        if let Some(lang) = extract_lang_from_str(&cls) {
            return Some(lang);
        }
    }
    for ch in children_of(pre_node) {
        if ch.tag_name() == Some("code") {
            for attr_name in ["data-language", "data-lang"] {
                if let Some(val) = ch.attr(attr_name) {
                    let clean: String = val
                        .trim()
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                        .collect();
                    if !clean.is_empty() {
                        return Some(clean.to_ascii_lowercase());
                    }
                }
            }
            if let Some(cls) = ch.attr("class") {
                if let Some(lang) = extract_lang_from_str(&cls) {
                    return Some(lang);
                }
            }
        }
    }
    if let Some(parent) = pre_node.parent() {
        for attr_name in ["data-language", "data-lang"] {
            if let Some(val) = parent.attr(attr_name) {
                let clean: String = val
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                    .collect();
                if !clean.is_empty() {
                    return Some(clean.to_ascii_lowercase());
                }
            }
        }
        if let Some(cls) = parent.attr("class") {
            if let Some(lang) = extract_lang_from_str(&cls) {
                return Some(lang);
            }
        }
    }
    None
}

fn is_line_number_gutter(node: &Node<'_>) -> bool {
    if node.attr("data-line-number").is_some() {
        return true;
    }
    node.attr("class").is_some_and(|cls| {
        cls.split_ascii_whitespace().any(|c| {
            matches!(
                c,
                "line-numbers"
                    | "line-number"
                    | "lineno"
                    | "linenos"
                    | "react-syntax-highlighter-line-number"
            )
        })
    })
}

fn extract_pre_code_text(pre_node: &Node<'_>) -> String {
    let mut out = String::new();
    let mut stack = vec![pre_node.clone()];
    while let Some(node) = stack.pop() {
        match node.node_type() {
            NodeType::Text => {
                if let Some(t) = node.text() {
                    out.push_str(&scrub_invisible_unicode(&t));
                }
            }
            NodeType::Element => {
                if node != *pre_node
                    && (is_hidden_or_pruned_element(&node) || is_line_number_gutter(&node))
                {
                    continue;
                }
                if node.tag_name() == Some("br") {
                    out.push('\n');
                    continue;
                }
                push_children_rev(&node, &mut stack);
            }
            _ => {}
        }
    }
    out
}

fn push_escaped_html_text(dst: &mut String, text: &str) {
    for ch in text.chars() {
        if is_invisible_injection_char(ch) {
            continue;
        }
        match ch {
            '&' => dst.push_str("&amp;"),
            '<' => dst.push_str("&lt;"),
            '>' => dst.push_str("&gt;"),
            other => dst.push(other),
        }
    }
}

fn push_escaped_attr(dst: &mut String, val: &str) {
    for ch in val.chars() {
        if is_invisible_injection_char(ch) {
            continue;
        }
        match ch {
            '&' => dst.push_str("&amp;"),
            '"' => dst.push_str("&quot;"),
            '<' => dst.push_str("&lt;"),
            '>' => dst.push_str("&gt;"),
            other => dst.push(other),
        }
    }
}

fn table_is_layout_or_complex(table_node: &Node<'_>) -> bool {
    if table_node
        .attr("role")
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("presentation"))
        || table_node
            .attr("datatable")
            .is_some_and(|d| d.trim() == "0")
    {
        return true;
    }
    let mut stack = vec![table_node.clone()];
    while let Some(node) = stack.pop() {
        if node != *table_node && matches!(node.tag_name(), Some("pre" | "table")) {
            return true;
        }
        push_children_rev(&node, &mut stack);
    }
    false
}

fn table_has_explicit_th(table_node: &Node<'_>) -> bool {
    let mut stack = vec![table_node.clone()];
    while let Some(node) = stack.pop() {
        if matches!(node.tag_name(), Some("th" | "thead")) {
            return true;
        }
        push_children_rev(&node, &mut stack);
    }
    false
}

fn resolve_safe_href(raw_href: &str, base_url: Option<&reqwest::Url>) -> Option<String> {
    let trimmed = raw_href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
    {
        return None;
    }
    if let Some(base) = base_url {
        if let Ok(joined) = base.join(trimmed) {
            return Some(joined.into());
        }
    }
    Some(trimmed.to_owned())
}

fn serialize_scrubbed_html_iterative(root: &Node<'_>, base_url: Option<&reqwest::Url>) -> String {
    enum Step<'a> {
        Enter(Node<'a>, bool),
        Exit(&'static str),
    }

    let mut out = String::new();
    let mut stack = vec![Step::Enter(root.clone(), false)];

    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(close_tag) => {
                out.push_str("</");
                out.push_str(close_tag);
                out.push('>');
            }
            Step::Enter(node, promote_first_row_td_to_th) => match node.node_type() {
                NodeType::Text => {
                    if let Some(t) = node.text() {
                        push_escaped_html_text(&mut out, &t);
                    }
                }
                NodeType::Document => {
                    let children = children_of(&node);
                    for ch in children.into_iter().rev() {
                        stack.push(Step::Enter(ch, false));
                    }
                }
                NodeType::Element => {
                    if is_hidden_or_pruned_element(&node) {
                        continue;
                    }
                    let tag = node.tag_name().unwrap_or("");
                    match tag {
                        "pre" => {
                            let code = extract_pre_code_text(&node);
                            let lang = detect_pre_language(&node);
                            out.push_str("<pre><code");
                            if let Some(l) = lang {
                                out.push_str(" class=\"language-");
                                push_escaped_attr(&mut out, &l);
                                out.push('"');
                            }
                            out.push('>');
                            push_escaped_html_text(&mut out, &code);
                            out.push_str("</code></pre>");
                        }
                        "br" => out.push_str("<br>"),
                        "hr" => out.push_str("<hr>"),
                        "img" => {
                            if let Some(alt) = node.attr("alt") {
                                let trimmed = alt.trim();
                                if !trimmed.is_empty() {
                                    push_escaped_html_text(&mut out, trimmed);
                                }
                            }
                        }
                        "a" => {
                            if let Some(href) = node
                                .attr("href")
                                .and_then(|h| resolve_safe_href(&h, base_url))
                            {
                                out.push_str("<a href=\"");
                                push_escaped_attr(&mut out, &href);
                                out.push_str("\">");
                                stack.push(Step::Exit("a"));
                            }
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, false));
                            }
                        }
                        "table" => {
                            if table_is_layout_or_complex(&node) {
                                stack.push(Step::Exit("div"));
                                out.push_str("<div>");
                                let children = children_of(&node);
                                for ch in children.into_iter().rev() {
                                    stack.push(Step::Enter(ch, false));
                                }
                            } else {
                                let needs_th_promo = !table_has_explicit_th(&node);
                                out.push_str("<table>");
                                stack.push(Step::Exit("table"));
                                let children = children_of(&node);
                                let mut marked_first = false;
                                let mut tagged_children = Vec::with_capacity(children.len());
                                for ch in children {
                                    let promo = needs_th_promo
                                        && !marked_first
                                        && matches!(ch.tag_name(), Some("tbody" | "tr"));
                                    if promo {
                                        marked_first = true;
                                    }
                                    tagged_children.push((ch, promo));
                                }
                                for (ch, promo) in tagged_children.into_iter().rev() {
                                    stack.push(Step::Enter(ch, promo));
                                }
                            }
                        }
                        "thead" => {
                            out.push_str("<thead>");
                            stack.push(Step::Exit("thead"));
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, false));
                            }
                        }
                        "tbody" | "tfoot" => {
                            out.push_str("<tbody>");
                            stack.push(Step::Exit("tbody"));
                            let children = children_of(&node);
                            let mut marked_first = false;
                            let mut tagged_children = Vec::with_capacity(children.len());
                            for ch in children {
                                let promo = promote_first_row_td_to_th
                                    && !marked_first
                                    && ch.tag_name() == Some("tr");
                                if promo {
                                    marked_first = true;
                                }
                                tagged_children.push((ch, promo));
                            }
                            for (ch, promo) in tagged_children.into_iter().rev() {
                                stack.push(Step::Enter(ch, promo));
                            }
                        }
                        "tr" => {
                            let inside_clean_table = node.parent().is_some_and(|p| {
                                matches!(p.tag_name(), Some("table" | "thead" | "tbody" | "tfoot"))
                            });
                            if inside_clean_table {
                                out.push_str("<tr>");
                                stack.push(Step::Exit("tr"));
                            } else {
                                out.push_str("<p>");
                                stack.push(Step::Exit("p"));
                            }
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, promote_first_row_td_to_th));
                            }
                        }
                        "th" => {
                            out.push_str("<th>");
                            stack.push(Step::Exit("th"));
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, false));
                            }
                        }
                        "td" => {
                            let cell_tag = if promote_first_row_td_to_th {
                                "th"
                            } else {
                                "td"
                            };
                            out.push('<');
                            out.push_str(cell_tag);
                            out.push('>');
                            stack.push(Step::Exit(cell_tag));
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, false));
                            }
                        }
                        other => {
                            let mapped: Option<&'static str> = match other {
                                "h1" => Some("h1"),
                                "h2" => Some("h2"),
                                "h3" => Some("h3"),
                                "h4" => Some("h4"),
                                "h5" => Some("h5"),
                                "h6" => Some("h6"),
                                "p" => Some("p"),
                                "ul" => Some("ul"),
                                "ol" => Some("ol"),
                                "li" => Some("li"),
                                "blockquote" => Some("blockquote"),
                                "code" => Some("code"),
                                "strong" | "b" => Some("strong"),
                                "em" | "i" => Some("em"),
                                "del" | "s" | "strike" => Some("del"),
                                "dt" => Some("strong"),
                                "dd" => Some("p"),
                                "div" | "section" | "article" | "main" | "header" | "aside"
                                | "figure" | "figcaption" | "details" | "summary" | "dl" => {
                                    Some("div")
                                }
                                _ => None,
                            };
                            if let Some(clean_tag) = mapped {
                                out.push('<');
                                out.push_str(clean_tag);
                                out.push('>');
                                stack.push(Step::Exit(clean_tag));
                            }
                            let children = children_of(&node);
                            for ch in children.into_iter().rev() {
                                stack.push(Step::Enter(ch, false));
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }
    out
}

pub(crate) fn extract_html_to_markdown(
    source: &str,
    base_url: Option<&reqwest::Url>,
) -> Result<(String, String)> {
    check_html_nesting_depth(source, MAX_HTML_NESTING_DEPTH)?;

    let converter = HtmlToMarkdown::builder()
        .options(Options {
            heading_style: HeadingStyle::Atx,
            code_block_style: CodeBlockStyle::Fenced,
            code_block_fence: CodeBlockFence::Backticks,
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Options::default()
        })
        .build();

    let tree = converter.html_to_tree(source)?;
    let doc_root = tree.root();
    let audit = audit_dom_iterative(&doc_root);

    let body_ref = audit.body_node.as_ref().unwrap_or(&doc_root);
    let visible_chars = count_visible_non_ws_chars(body_ref);

    if visible_chars < 120 && audit.has_bot_marker {
        return Err(IngestError::Invalid(
            "HTML page returned an anti-bot or CAPTCHA challenge",
        ));
    }

    let noscript_low = audit.noscript_text.to_ascii_lowercase();
    let noscript_js_gate = noscript_low.contains("enable javascript")
        || noscript_low.contains("javascript is required")
        || noscript_low.contains("without javascript")
        || audit.noscript_text.contains("启用 JavaScript")
        || noscript_low.contains("启用javascript")
        || noscript_low.contains("启用 javascript")
        || noscript_low.contains("开启 javascript");

    if visible_chars < 120
        && (audit.has_empty_spa_mount
            || noscript_js_gate
            || (audit.external_or_module_scripts >= 2 && visible_chars < 80))
    {
        return Err(IngestError::Invalid(
            "HTML page is an empty client-rendered SPA shell",
        ));
    }

    let selected_root = audit
        .main_candidates
        .iter()
        .chain(audit.role_main_candidates.iter())
        .chain(audit.article_candidates.iter())
        .chain(audit.doc_class_candidates.iter())
        .find(|cand| count_visible_non_ws_chars(cand) >= 120)
        .unwrap_or(body_ref);

    let scrubbed_html = serialize_scrubbed_html_iterative(selected_root, base_url);
    let raw_md = converter.convert(&scrubbed_html)?;
    let clean_md = scrub_invisible_unicode(&raw_md);
    Ok((audit.title, clean_md))
}

pub(crate) fn parse_html(source: &str) -> Result<Vec<ChunkInput>> {
    let (doc_title, md) = extract_html_to_markdown(source, None)?;
    if md.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks = text::markdown(&md)?;
    for chunk in &mut chunks {
        if chunk.heading.is_empty() && !doc_title.is_empty() {
            chunk.heading = doc_title.clone();
        }
        chunk.anchor = AnchorSpec::default();
    }
    Ok(chunks)
}

pub async fn add_url(
    store: &mut Store,
    raw_url: &str,
    options: &FunnelOptions,
    trusted: bool,
    extractor: Option<&dyn Extractor>,
) -> Result<AddedDocument> {
    let allow_local = std::env::var_os("KP_ALLOW_LOCAL_URL_INGEST").is_some();
    let (final_url, mut resp) = fetch_url_ssrf_safe(raw_url, allow_local).await?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .split(';')
        .next()
        .unwrap_or("text/html")
        .trim()
        .to_ascii_lowercase();

    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| IngestError::Invalid("URL stream read failed"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_INPUT_BYTES {
            return Err(IngestError::Limit(MAX_INPUT_BYTES));
        }
        bytes.extend_from_slice(&chunk);
    }

    let path_lower = final_url.path().to_ascii_lowercase();
    if content_type.starts_with("audio/")
        || content_type.starts_with("video/")
        || matches!(
            content_type.as_str(),
            "text/vtt" | "application/x-subrip" | "application/x-lrc"
        )
        || [
            ".mp4", ".mov", ".mkv", ".webm", ".avi", ".mp3", ".m4a", ".wav", ".flac", ".ogg",
            ".aac", ".vtt", ".srt", ".lrc",
        ]
        .iter()
        .any(|ext| path_lower.ends_with(ext))
    {
        return Err(IngestError::Invalid("unsupported document format"));
    }

    let (media_type, stored_bytes, mut chunks, needs_ocr) =
        if content_type == "application/pdf" || path_lower.ends_with(".pdf") {
            let doc = crate::doc_router::parse(Path::new("remote.pdf"), &bytes)?;
            (doc.media_type, bytes, doc.chunks, doc.needs_ocr)
        } else if content_type == "text/markdown"
            || path_lower.ends_with(".md")
            || path_lower.ends_with(".markdown")
        {
            let decoded = text::decode(&bytes)?;
            let md_bytes = decoded.as_bytes().to_vec();
            let parsed_chunks = text::markdown_bytes(&md_bytes)?;
            ("text/markdown", md_bytes, parsed_chunks, false)
        } else if content_type == "text/csv" || path_lower.ends_with(".csv") {
            let decoded = text::decode(&bytes)?;
            let csv_bytes = decoded.as_bytes().to_vec();
            let parsed_chunks = text::csv_bytes(&csv_bytes)?;
            ("text/csv", csv_bytes, parsed_chunks, false)
        } else if content_type.contains("json") || path_lower.ends_with(".json") {
            let decoded = text::decode(&bytes)?;
            let json_bytes = decoded.as_bytes().to_vec();
            let parsed_chunks = text::plain_text_bytes(&json_bytes)?;
            ("application/json", json_bytes, parsed_chunks, false)
        } else if content_type.contains("html")
            || path_lower.ends_with(".html")
            || path_lower.ends_with(".htm")
        {
            let decoded = text::decode(&bytes)?;
            let (doc_title, mut md) = extract_html_to_markdown(&decoded, Some(&final_url))?;
            if md.trim().is_empty() {
                return Err(IngestError::Invalid(
                    "HTML page contained zero extractable text",
                ));
            }
            if !md.ends_with('\n') {
                md.push('\n');
            }
            let md_bytes = md.into_bytes();
            let mut parsed_chunks = text::markdown_bytes(&md_bytes)?;
            for ch in &mut parsed_chunks {
                if ch.heading.is_empty() && !doc_title.is_empty() {
                    ch.heading = doc_title.clone();
                }
            }
            ("text/markdown", md_bytes, parsed_chunks, false)
        } else {
            let decoded = text::decode(&bytes)?;
            let txt_bytes = decoded.as_bytes().to_vec();
            let parsed_chunks = text::plain_text_bytes(&txt_bytes)?;
            ("text/plain", txt_bytes, parsed_chunks, false)
        };

    let uri = final_url.as_str().to_owned();
    for ch in &mut chunks {
        ch.anchor.file_path = Some(uri.clone());
    }
    let key = format!("source:{uri}:{}", ContentHash::digest(&stored_bytes)).parse()?;
    let mut inputs = Vec::new();
    if let Some(extractor) = extractor {
        for chunk in &chunks {
            for extracted in extractor.extract(&chunk.body, &uri)? {
                let mut input = base_fact(
                    store,
                    Memory {
                        subject: extracted.subject,
                        predicate: extracted.predicate,
                        value: extracted.value,
                        kind: extracted.kind,
                        logical_key: None,
                        tags: extracted.tags,
                        entities: extracted.entities,
                        valid: ValidityWindow {
                            valid_from: options.at,
                            valid_until: kp_model::EpochMs::OPEN_END,
                        },
                    },
                    options,
                )?;
                if !trusted {
                    input.fact.trust_tier = TrustTier::ExternalUntrusted;
                }
                input.fact.source_anchor = Some(chunk.anchor.clone());
                inputs.push(input);
            }
        }
    }
    let chunk_count = chunks.len();
    let raw_input = RawSourceInput {
        uri: uri.clone(),
        media_type: media_type.into(),
        content: stored_bytes,
        readable_by: options.audience.clone(),
        captured_at: options.at,
        chunks,
        self_echo_filtered: false,
    };
    let lib_id = store.library_id().clone();
    let (source, extracted) = if store.status()?.embedding_dims == 0 {
        store.ingest_funnel(
            &lib_id,
            &key,
            &raw_input,
            &kp_store::FtsOnlyEmbedder,
            &mut inputs,
        )?
    } else {
        store.ingest_funnel(
            &lib_id,
            &key,
            &raw_input,
            &kp_store::HashEmbedder,
            &mut inputs,
        )?
    };
    Ok(AddedDocument {
        uri,
        source,
        extracted,
        chunks: chunk_count,
        needs_ocr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_to_http_redirect_downgrade_and_rfc6890_special_ips_are_blocked() -> Result<()> {
        let https_origin = reqwest::Url::parse("https://93.184.216.34/docs/start")
            .map_err(|_| IngestError::Invalid("url"))?;
        assert!(matches!(
            resolve_redirect_hop(&https_origin, "http://93.184.216.34/docs/plain", false),
            Err(IngestError::Invalid(
                "HTTPS to HTTP redirect downgrade blocked"
            ))
        ));
        let upgraded = resolve_redirect_hop(
            &reqwest::Url::parse("http://93.184.216.34/docs")
                .map_err(|_| IngestError::Invalid("url"))?,
            "https://93.184.216.34/docs/v2#install",
            false,
        )?;
        assert_eq!(upgraded.as_str(), "https://93.184.216.34/docs/v2");

        for blocked_ip in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.10",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::a00:1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2001:10::1",
            "2001:20::1",
            "2002:7f00:1::",
        ] {
            let ip: IpAddr = blocked_ip
                .parse()
                .map_err(|_| IngestError::Invalid("ip parse"))?;
            assert!(is_disallowed_ip(ip, false), "must block {blocked_ip}");
        }
        let public_v4: IpAddr = "93.184.216.34"
            .parse()
            .map_err(|_| IngestError::Invalid("ip"))?;
        let public_v6: IpAddr = "2606:2800:220:1:248:1893:25c8:1946"
            .parse()
            .map_err(|_| IngestError::Invalid("ip"))?;
        assert!(!is_disallowed_ip(public_v4, false));
        assert!(!is_disallowed_ip(public_v6, false));

        for host in [
            "localhost",
            "sub.localhost",
            "printer.local",
            "metadata.internal",
            "router.home.arpa",
        ] {
            assert!(is_disallowed_hostname(host, false), "must block {host}");
        }
        assert!(!is_disallowed_hostname("localhost", true));
        assert!(is_disallowed_hostname("metadata.internal", true));
        Ok(())
    }
}
