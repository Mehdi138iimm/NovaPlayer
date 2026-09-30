//! Update notification: asks GitHub Releases for the newest NOVA version, betas included.
//!
//! It only tells the user and opens the download: there is no silent/automatic install
//! (tauri-plugin-updater was removed in 1.6.0).
//! * it lists every release: `/releases/latest` never points at a pre-release, so that endpoint hid betas.
//! * the GitHub API gives 60 requests/hour per IP without a token. On Iranian CGNAT lines many users
//!   share one IP, so it often answers 403. The public `releases.atom` feed is the fallback.
use serde::Serialize;
use std::{cmp::Ordering, time::Duration};
use tauri::AppHandle;

const REPO: &str = "Mehdi138iimm/NovaPlayer";
const UA: &str = concat!("NOVA-Player/", env!("CARGO_PKG_VERSION"), " (+https://github.com/Mehdi138iimm/NovaPlayer)");

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GithubUpdate {
    current: String,
    latest: Option<String>,
    newer: bool,
    prerelease: bool,
    /// release page on GitHub
    url: String,
    /// direct link to the installer (-setup.exe), when GitHub listed the assets
    download_url: Option<String>,
    notes: Option<String>,
    published_at: Option<String>,
    /// "api" or "atom": which source answered
    source: &'static str,
}

struct Rel {
    tag: String,
    url: String,
    prerelease: bool,
    download_url: Option<String>,
    notes: Option<String>,
    published_at: Option<String>,
}

/// "v1.6.0-beta.2" -> ([1, 6, 0], ["beta", "2"])
fn parse(v: &str) -> ([u64; 3], Vec<String>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next().unwrap_or("");
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, p),
        None => (v, ""),
    };
    let mut nums = [0u64; 3];
    for (i, part) in core.split('.').take(3).enumerate() {
        nums[i] = part.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
    }
    let pre = pre.split('.').filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase()).collect();
    (nums, pre)
}

/// SemVer order: 1.6.0-beta.1 < 1.6.0-beta.2 < 1.6.0 < 1.6.1
fn cmp_ver(a: &str, b: &str) -> Ordering {
    let (an, ap) = parse(a);
    let (bn, bp) = parse(b);
    an.cmp(&bn).then_with(|| match (ap.is_empty(), bp.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (x, y) in ap.iter().zip(bp.iter()) {
                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(p), Ok(q)) => p.cmp(&q),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            ap.len().cmp(&bp.len())
        }
    })
}

fn is_newer(candidate: &str, current: &str) -> bool {
    cmp_ver(candidate, current) == Ordering::Greater
}

fn looks_prerelease(tag: &str) -> bool {
    !parse(tag).1.is_empty()
}

fn err_text(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(inner) = src {
        s.push_str(" · ");
        s.push_str(&inner.to_string());
        src = std::error::Error::source(inner);
    }
    s
}

/// first asset whose name ends with `suffix` (case-insensitive)
fn pick_asset(assets: &[serde_json::Value], suffix: &str) -> Option<String> {
    assets.iter().find_map(|a| {
        let name = a["name"].as_str()?.to_ascii_lowercase();
        if name.ends_with(suffix) {
            a["browser_download_url"].as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// installer that fits the OS this build runs on
fn pick_for_os(assets: &[serde_json::Value]) -> Option<String> {
    let order: &[&str] = if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") { &["aarch64.dmg", "universal.dmg", ".dmg"] } else { &["x64.dmg", "universal.dmg", ".dmg"] }
    } else if cfg!(target_os = "linux") {
        &[".appimage", ".deb", ".rpm"]
    } else {
        &["setup.exe", ".exe", ".msi"]
    };
    order.iter().find_map(|s| pick_asset(assets, s))
}

async fn from_api(c: &reqwest::Client) -> Result<Vec<Rel>, String> {
    let r = c
        .get(format!("https://api.github.com/repos/{REPO}/releases?per_page=30"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| err_text(&e))?;
    if !r.status().is_success() {
        return Err(format!("GitHub API: HTTP {}", r.status().as_u16()));
    }
    let list: Vec<serde_json::Value> = r.json().await.map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .filter(|x| !x["draft"].as_bool().unwrap_or(false))
        .filter_map(|x| {
            let assets = x["assets"].as_array().cloned().unwrap_or_default();
            let download_url = pick_for_os(&assets);
            Some(Rel {
                tag: x["tag_name"].as_str()?.to_string(),
                url: x["html_url"].as_str().unwrap_or_default().to_string(),
                prerelease: x["prerelease"].as_bool().unwrap_or(false),
                download_url,
                notes: x["body"].as_str().map(|s| s.chars().take(4000).collect()),
                published_at: x["published_at"].as_str().map(str::to_string),
            })
        })
        .collect())
}

async fn from_atom(c: &reqwest::Client) -> Result<Vec<Rel>, String> {
    let r = c
        .get(format!("https://github.com/{REPO}/releases.atom"))
        .send()
        .await
        .map_err(|e| err_text(&e))?;
    if !r.status().is_success() {
        return Err(format!("releases.atom: HTTP {}", r.status().as_u16()));
    }
    let body = r.text().await.map_err(|e| e.to_string())?;
    let marker = "/releases/tag/";
    let mut out: Vec<Rel> = Vec::new();
    let mut rest = body.as_str();
    while let Some(i) = rest.find(marker) {
        let after = &rest[i + marker.len()..];
        let end = after.find(['"', '<', '\'', ' ']).unwrap_or(after.len());
        let tag = after[..end].to_string();
        if !tag.is_empty() && !out.iter().any(|x| x.tag == tag) {
            out.push(Rel {
                url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
                // the feed does not say which ones are pre-releases; "v1.7.0-beta" style tags still get the badge
                prerelease: looks_prerelease(&tag),
                tag,
                download_url: None,
                notes: None,
                published_at: None,
            });
        }
        rest = &after[end..];
    }
    if out.is_empty() {
        return Err("releases.atom: هیچ نسخه‌ای پیدا نشد".into());
    }
    Ok(out)
}

/// Newest published release (pre-releases included) compared with the running version.
#[tauri::command]
pub async fn check_github_update(app: AppHandle) -> Result<GithubUpdate, String> {
    let current = app.package_info().version.to_string();
    let c = reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let (list, source) = match from_api(&c).await {
        Ok(l) if !l.is_empty() => (l, "api"),
        api => {
            let why = api.err().unwrap_or_else(|| "لیست خالی".into());
            match from_atom(&c).await {
                Ok(l) => (l, "atom"),
                Err(e) => return Err(format!("{why} · {e}")),
            }
        }
    };
    // highest version wins, not the newest date: a hotfix for an older line must not look like an update
    let best = list.into_iter().max_by(|a, b| cmp_ver(&a.tag, &b.tag));
    let newer = best.as_ref().map(|b| is_newer(&b.tag, &current)).unwrap_or(false);
    let fallback = format!("https://github.com/{REPO}/releases");
    Ok(match best {
        Some(b) => GithubUpdate {
            current,
            newer,
            prerelease: b.prerelease,
            url: if b.url.is_empty() { fallback } else { b.url },
            download_url: b.download_url,
            notes: b.notes,
            published_at: b.published_at,
            latest: Some(b.tag),
            source,
        },
        None => GithubUpdate {
            current,
            latest: None,
            newer: false,
            prerelease: false,
            url: fallback,
            download_url: None,
            notes: None,
            published_at: None,
            source,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("v1.6.0", "1.5.0"));
        assert!(is_newer("v1.10.0", "1.9.9"));
        assert!(is_newer("v2.0.0-beta", "1.6.0"));
        assert!(is_newer("v1.6.0", "1.6.0-beta.2"));
        assert!(is_newer("v1.6.0-beta.2", "1.6.0-beta.1"));
        assert!(is_newer("v1.6.0-beta.10", "1.6.0-beta.9"));
        assert!(is_newer("v1.6.0-rc.1", "1.6.0-beta.3"));
        assert!(!is_newer("v1.6.0", "1.6.0"));
        assert!(!is_newer("v1.6.0-beta.1", "1.6.0"));
        assert!(!is_newer("v1.5.0", "1.6.0"));
        assert!(looks_prerelease("v1.7.0-beta"));
        assert!(!looks_prerelease("v1.7.0"));
    }
}
