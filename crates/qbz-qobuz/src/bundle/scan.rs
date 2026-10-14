//! The bundle scrape's matchers, without a regex engine.
//!
//! Each pattern the scrape looks for is a fixed anchor followed by a simple
//! field (digits, hex, base64ish word characters), so `str::find` on the
//! anchor plus a few byte checks does the job. This replaced `regex`, which
//! was the only thing pulling a regex engine into the build for this crate;
//! `regex-lite`, the cheap-to-compile alternative, took 650 ms over a 9 MB
//! bundle against 35 ms here, and this runs in an async task on first start.
//!
//! Every function documents the regex it replaced, and matches what that regex
//! matched: same fields, same order, non-overlapping where the regex was. The
//! one deliberate difference is `\w`, which is ASCII here (letters, digits,
//! `_`) where the regex's was Unicode; the values are base64 and the bundle is
//! minified JavaScript, so nothing that was found is lost.

/// The leading run of `s` whose bytes satisfy `ok`.
fn take_while(s: &str, ok: impl Fn(u8) -> bool) -> &str {
    let end = s.bytes().position(|b| !ok(b)).unwrap_or(s.len());
    &s[..end]
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `[\w=]`, the alphabet of the seed, info and extras fields.
fn is_word_or_eq(b: u8) -> bool {
    is_word(b) || b == b'='
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// `<script src="(/resources/\d+\.\d+\.\d+-[a-z]\d{3}/bundle\.js)"></script>`
pub(super) fn bundle_url(html: &str) -> Option<&str> {
    const OPEN: &str = "<script src=\"";
    for (at, _) in html.match_indices(OPEN) {
        let rest = &html[at + OPEN.len()..];
        let Some(path) = rest.split('"').next() else {
            continue;
        };
        if rest[path.len()..].starts_with("\"></script>") && is_bundle_path(path) {
            return Some(path);
        }
    }
    None
}

/// `/resources/\d+\.\d+\.\d+-[a-z]\d{3}/bundle\.js`
fn is_bundle_path(path: &str) -> bool {
    let Some(version) = path
        .strip_prefix("/resources/")
        .and_then(|p| p.strip_suffix("/bundle.js"))
    else {
        return false;
    };
    let Some((numbers, build)) = version.split_once('-') else {
        return false;
    };
    let parts: Vec<&str> = numbers.split('.').collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let build = build.as_bytes();
    parts.len() == 3
        && parts.iter().all(|p| digits(p))
        && build.len() == 4
        && build[0].is_ascii_lowercase()
        && build[1..].iter().all(u8::is_ascii_digit)
}

/// `production:\{api:\{appId:"(\d{9})"`
pub(super) fn app_id(bundle: &str) -> Option<&str> {
    const ANCHOR: &str = "production:{api:{appId:\"";
    bundle.match_indices(ANCHOR).find_map(|(at, _)| {
        let rest = &bundle[at + ANCHOR.len()..];
        let id = take_while(rest, |b| b.is_ascii_digit());
        (id.len() == 9 && rest[9..].starts_with('"')).then_some(id)
    })
}

/// `[a-z]\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)`, as
/// `(seed, timezone)` in bundle order.
pub(super) fn seeds(bundle: &str) -> Vec<(&str, &str)> {
    const ANCHOR: &str = ".initialSeed(\"";
    const MIDDLE: &str = "\",window.utimezone.";
    let mut out = Vec::new();
    for (at, _) in bundle.match_indices(ANCHOR) {
        if at == 0 || !bundle.as_bytes()[at - 1].is_ascii_lowercase() {
            continue;
        }
        let rest = &bundle[at + ANCHOR.len()..];
        let seed = take_while(rest, is_word_or_eq);
        let Some(rest) = rest[seed.len()..].strip_prefix(MIDDLE) else {
            continue;
        };
        let zone = take_while(rest, |b| b.is_ascii_lowercase());
        if !seed.is_empty() && !zone.is_empty() && rest[zone.len()..].starts_with(')') {
            out.push((seed, zone));
        }
    }
    out
}

/// `name:"\w+/(Zone1|Zone2|...)",info:"([\w=]+)",extras:"([\w=]+)"`, as
/// `(zone, info, extras)` in bundle order. `zones` are as the bundle writes
/// them here, capitalized.
pub(super) fn infos<'a>(bundle: &'a str, zones: &[String]) -> Vec<(&'a str, &'a str, &'a str)> {
    const ANCHOR: &str = "name:\"";
    let mut out = Vec::new();
    for (at, _) in bundle.match_indices(ANCHOR) {
        let rest = &bundle[at + ANCHOR.len()..];
        let region = take_while(rest, is_word);
        let Some(rest) = rest[region.len()..].strip_prefix('/') else {
            continue;
        };
        if region.is_empty() {
            continue;
        }
        // The regex's alternation: the first listed zone that the text
        // continues with and is followed by the info field.
        for zone in zones {
            let Some(after) = rest
                .strip_prefix(zone.as_str())
                .and_then(|r| r.strip_prefix("\",info:\""))
            else {
                continue;
            };
            let info = take_while(after, is_word_or_eq);
            let Some(after) = after[info.len()..].strip_prefix("\",extras:\"") else {
                continue;
            };
            let extras = take_while(after, is_word_or_eq);
            if !info.is_empty() && !extras.is_empty() && after[extras.len()..].starts_with('"') {
                out.push((&rest[..zone.len()], info, extras));
                break;
            }
        }
    }
    out
}

/// `appSecret:"([a-f0-9]{32})"`, in bundle order.
pub(super) fn app_secret_literals(bundle: &str) -> Vec<&str> {
    const ANCHOR: &str = "appSecret:\"";
    bundle
        .match_indices(ANCHOR)
        .filter_map(|(at, _)| {
            let rest = &bundle[at + ANCHOR.len()..];
            let hex = take_while(rest, is_lower_hex);
            (hex.len() == 32 && rest[32..].starts_with('"')).then_some(hex)
        })
        .collect()
}

/// `"([0-9a-f]{32})"`, non-overlapping like the regex's `captures_iter`: a
/// literal's closing quote cannot open the next. Each as `(byte offset of the
/// value, value)`.
pub(super) fn hex32_literals(bundle: &str) -> Vec<(usize, &str)> {
    let bytes = bundle.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(q) = bundle[from..].find('"').map(|i| from + i) {
        let start = q + 1;
        let end = start + 32;
        if end < bytes.len()
            && bytes[end] == b'"'
            && bytes[start..end].iter().all(|b| is_lower_hex(*b))
        {
            out.push((start, &bundle[start..end]));
            from = end + 1;
        } else {
            from = start;
        }
    }
    out
}

/// `privateKey:\s*"([A-Za-z0-9]{6,30})"`
pub(super) fn private_key(bundle: &str) -> Option<&str> {
    const ANCHOR: &str = "privateKey:";
    bundle.match_indices(ANCHOR).find_map(|(at, _)| {
        let rest = bundle[at + ANCHOR.len()..].trim_start();
        let rest = rest.strip_prefix('"')?;
        let key = take_while(rest, |b| b.is_ascii_alphanumeric());
        ((6..=30).contains(&key.len()) && rest[key.len()..].starts_with('"')).then_some(key)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_url_wants_the_whole_shape() {
        let html = r#"<script src="/x.js"></script><script src="/resources/8.2.0-b034/bundle.js"></script>"#;
        assert_eq!(bundle_url(html), Some("/resources/8.2.0-b034/bundle.js"));
        for bad in [
            r#"<script src="/resources/8.2-b034/bundle.js"></script>"#,
            r#"<script src="/resources/8.2.0-B034/bundle.js"></script>"#,
            r#"<script src="/resources/8.2.0-b34/bundle.js"></script>"#,
            r#"<script src="/resources/8.2.0-b034/bundle.js" defer></script>"#,
        ] {
            assert_eq!(bundle_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn app_id_is_exactly_nine_digits() {
        assert_eq!(app_id(r#"production:{api:{appId:"12345678"}"#), None);
        assert_eq!(app_id(r#"production:{api:{appId:"1234567890"}"#), None);
        assert_eq!(
            app_id(r#"production:{api:{appId:"1234567890"} production:{api:{appId:"123456789""#),
            Some("123456789"),
            "a bad first occurrence must not hide a good second"
        );
    }

    #[test]
    fn seeds_need_a_lowercase_receiver_and_the_whole_call() {
        let bundle = r#"a.initialSeed("c2VlZA==",window.utimezone.berlin)
                        X.initialSeed("bm8=",window.utimezone.london)
                        b.initialSeed("aGk=",window.utimezone.paris"#;
        assert_eq!(seeds(bundle), vec![("c2VlZA==", "berlin")]);
    }

    #[test]
    fn infos_match_only_the_listed_zones() {
        let zones = vec!["Berlin".to_string(), "London".to_string()];
        let bundle = r#"name:"Europe/Berlin",info:"aW5m",extras:"ZXh0"
                        name:"Europe/Paris",info:"eA==",extras:"eQ=="
                        name:"Europe/London",info:"bG9u",extras:"ZG9u""#;
        assert_eq!(
            infos(bundle, &zones),
            vec![("Berlin", "aW5m", "ZXh0"), ("London", "bG9u", "ZG9u")]
        );
    }

    #[test]
    fn hex_literals_do_not_overlap() {
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        // The closing quote of the first cannot open the second.
        let bundle = format!(r#""{a}"{b}" "{b}""#);
        let got: Vec<&str> = hex32_literals(&bundle)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        assert_eq!(got, vec![a.as_str(), b.as_str()]);
        assert!(
            hex32_literals(&format!(r#""{}""#, "A".repeat(32))).is_empty(),
            "lowercase only"
        );
    }

    #[test]
    fn private_key_allows_space_and_bounds_the_length() {
        assert_eq!(private_key(r#"privateKey: "abc123""#), Some("abc123"));
        assert_eq!(private_key(r#"privateKey:"abc12""#), None);
        assert_eq!(
            private_key(&format!(r#"privateKey:"{}""#, "k".repeat(31))),
            None
        );
    }
}
