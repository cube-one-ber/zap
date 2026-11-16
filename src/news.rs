//! Arch Linux news, from the official RSS feed.
use crate::errors::{self, Kind, Result};
use crate::ui::{self, NoteKind, Tone};
use crate::net;

const FEED: &str = "https://archlinux.org/feeds/news/";
const SHOWN: usize = 10;

#[derive(Debug)]
pub struct Item {
    pub title: String,
    pub link: String,
    pub date: String,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Deliberately limited to Arch's RSS format: no DTD, external entities, HTML
/// renderer, scripts, or general XML entity expansion.
pub fn decode(bytes: &[u8]) -> Result<Vec<Item>> {
    let invalid = || errors::fail(Kind::InvalidNewsFeed);
    if bytes.len() > 1024 * 1024 {
        return invalid();
    }
    if [&b"<!DOCTYPE"[..], b"<!ENTITY"].iter().any(|f| find(bytes, f).is_some())
        || [&b"<rss "[..], b"</rss>"].iter().any(|r| find(bytes, r).is_none())
    {
        return invalid();
    }
    let mut items = Vec::new();
    let mut rest = bytes;
    while let Some(start) = find(rest, b"<item>") {
        rest = &rest[start + b"<item>".len()..];
        let Some(stop) = find(rest, b"</item>") else { return invalid() };
        let item = &rest[..stop];
        let link = text(field(item, "link")?)?;
        // Links are shown for the reader to open: they must stay on archlinux.org.
        if !link.starts_with("https://archlinux.org/news/") || link.contains(['\r', '\n', '\t', ' ', '<', '>', '"', '\\']) {
            return invalid();
        }
        items.push(Item { title: text(field(item, "title")?)?, link, date: text(field(item, "pubDate")?)? });
        if items.len() > 100 {
            return invalid();
        }
        rest = &rest[stop + b"</item>".len()..];
    }
    if items.is_empty() {
        return invalid();
    }
    Ok(items)
}

fn field<'a>(item: &'a [u8], name: &str) -> Result<&'a [u8]> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let start = find(item, open.as_bytes()).ok_or(Kind::InvalidNewsFeed)? + open.len();
    let stop = start + find(&item[start..], close.as_bytes()).ok_or(Kind::InvalidNewsFeed)?;
    if stop - start > 4096 {
        return errors::fail(Kind::InvalidNewsFeed);
    }
    Ok(&item[start..stop])
}

/// Decode the five predefined entities and numeric references; reject markup.
fn text(input: &[u8]) -> Result<String> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'<' => return errors::fail(Kind::InvalidNewsFeed),
            b'&' => {
                let stop = i + input[i..].iter().position(|&b| b == b';').ok_or(Kind::InvalidNewsFeed)?;
                let entity = std::str::from_utf8(&input[i + 1..stop]).map_err(|_| Kind::InvalidNewsFeed)?;
                let named = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => None,
                };
                let ch = match (named, entity.strip_prefix('#')) {
                    (Some(ch), _) => ch,
                    (None, Some(number)) => {
                        let point = match number.strip_prefix('x') {
                            Some(hex) => u32::from_str_radix(hex, 16),
                            None => number.parse::<u32>(),
                        };
                        point.ok().and_then(char::from_u32).ok_or(Kind::InvalidNewsFeed)?
                    }
                    (None, None) => return errors::fail(Kind::InvalidNewsFeed),
                };
                out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                i = stop;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The timestamp of an RFC 822 date such as `Wed, 21 Oct 2026 13:58:42 +0000`,
/// to the day. `None` when the text has another shape.
fn published(date: &str) -> Option<i64> {
    let mut words = date.split([' ', ',']).filter(|w| !w.is_empty());
    words.next()?;
    let day: u8 = words.next()?.parse().ok()?;
    let month_name = words.next()?;
    let year: u16 = words.next()?.parse().ok()?;
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|m| *m == month_name)? as i64 + 1;
    if day == 0 || day > 31 || !(1970..=9999).contains(&year) {
        return None;
    }
    // Days since the epoch for a civil date (Howard Hinnant's algorithm).
    let y = year as i64 - (month <= 2) as i64;
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some((era * 146097 + day_of_era - 719468) * 86400)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Urgency {
    None,
    Security,
    Intervention,
}

/// Whether a title announces that an upgrade needs action from the reader,
/// or a security problem that an AUR helper's user should know about.
fn urgency(title: &str) -> Urgency {
    let title = title.to_ascii_lowercase();
    if ["intervention", "action required", "manual", "breaking", "renamed"].iter().any(|p| title.contains(p)) {
        return Urgency::Intervention;
    }
    let security = ["security", "vulnerab", "remote code execution", "man-in-the-middle", "malicious", "compromise"];
    if security.iter().any(|p| title.contains(p)) {
        return Urgency::Security;
    }
    Urgency::None
}

pub fn show() -> Result<()> {
    let items = decode(&net::get(FEED)?)?;
    let latest = &items[..items.len().min(SHOWN)];
    let count = |u| latest.iter().filter(|item| urgency(&item.title) == u).count();
    let (intervention, security) = (count(Urgency::Intervention), count(Urgency::Security));
    ui::heading("Arch Linux news", "latest first");
    // The warnings come first: they are what must not be scrolled past.
    if intervention > 0 {
        let items = ui::count(intervention, "item", "items");
        ui::note(NoteKind::Warning, &format!("{items} marked ! need manual intervention. Read them before running zap upgrade."));
    }
    if security > 0 {
        let items = ui::count(security, "item", "items");
        ui::marked("◆", Tone::Danger, &format!("{items} marked ◆ concern security. Check whether they affect you."), Tone::Plain);
    }
    if intervention == 0 && security == 0 {
        ui::note(NoteKind::Info, "Nothing here asks for manual intervention.");
    }
    for item in latest {
        ui::blank();
        // Every title has a mark, so the list keeps one left edge.
        let (mark, tone) = match urgency(&item.title) {
            Urgency::Intervention => ("!", Tone::Warning),
            Urgency::Security => ("◆", Tone::Danger),
            Urgency::None => ("·", Tone::Muted),
        };
        ui::marked(mark, tone, &item.title, Tone::Bold);
        let date = published(&item.date).map(ui::dated).unwrap_or_else(|| item.date.clone());
        ui::text(&date, 4, Tone::Muted);
        ui::line(&item.link, 4, Tone::Accent);
    }
    ui::blank();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_are_decoded_without_external_expansion_and_links_stay_on_arch() {
        let good = "<rss version=\"2\"><item><title>Action &amp; update &gt; &#x32;</title><link>https://archlinux.org/news/action/</link><pubDate>today</pubDate></item></rss>";
        assert_eq!("Action & update > 2", decode(good.as_bytes()).unwrap()[0].title);
        let fails = |feed: &str| decode(feed.as_bytes()).unwrap_err().kind == Kind::InvalidNewsFeed;
        assert!(fails("<!DOCTYPE rss><rss version=\"2\"></rss>"));
        assert!(fails(&good.replace("https://archlinux.org/news/action/", "https://evil.example/news/")));
        assert!(fails(&good.replace("</item>", "")));
        assert!(fails(&good.replace("&amp;", "&nbsp;")));
    }

    #[test]
    fn titles_that_ask_for_manual_intervention_or_concern_security_are_flagged() {
        assert_eq!(Urgency::Intervention, urgency("Mkinitcpio >=42 requires manual intervention for TPM2-based unlocking"));
        assert_eq!(Urgency::Intervention, urgency("Action required: the foo package moved"));
        assert_eq!(Urgency::Intervention, urgency("Breaking changes for all users of `varnish`, which is renamed to `vinyl-cache`"));
        assert_eq!(Urgency::Security, urgency("Update to php >= 8.5.12 to prevent remote code execution"));
        assert_eq!(Urgency::Security, urgency("Active AUR malicious packages incident"));
        assert_eq!(Urgency::None, urgency("Arch Linux leader election 2026"));
    }

    #[test]
    fn publication_dates_are_read_to_the_day_and_anything_else_is_shown_verbatim() {
        let _lock = ui::test_lock();
        assert_eq!(Some(0), published("Thu, 01 Jan 1970 00:00:00 +0000"));
        assert_eq!("2026-10-21", ui::date(published("Wed, 21 Oct 2026 13:58:42 +0000").unwrap()));
        assert_eq!("2024-02-29", ui::date(published("Thu, 29 Feb 2024 08:00:00 +0000").unwrap()));
        assert_eq!("2000-03-01", ui::date(published("Wed, 01 Mar 2000 00:00:00 +0000").unwrap()));
        for date in ["today", "", "Wed, 21 Smarch 2026", "Wed, 99 Oct 2026", "21 Oct"] {
            assert_eq!(None, published(date), "{date}");
        }
    }
}
