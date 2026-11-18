//! The AUR RPC interface, version 5.
use crate::cli::SearchBy;
use crate::errors::{self, Kind, Result};
use crate::{net, util};
use serde::Deserialize;

const ENDPOINT: &str = "https://aur.archlinux.org/rpc/v5";
/// The RPC caps an info request's URL length; batches stay well below it.
const INFO_BATCH: usize = 100;

/// Field names follow the RPC's JSON.
#[derive(Clone, Debug, Default, Deserialize)]
#[allow(non_snake_case)]
pub struct Package {
    pub Name: String,
    pub PackageBase: String,
    pub Version: String,
    #[serde(default)]
    pub Description: Option<String>,
    #[serde(default)]
    pub URL: Option<String>,
    #[serde(default)]
    pub Maintainer: Option<String>,
    #[serde(default)]
    pub NumVotes: i64,
    #[serde(default)]
    pub Popularity: f64,
    #[serde(default)]
    pub FirstSubmitted: i64,
    #[serde(default)]
    pub LastModified: i64,
    #[serde(default)]
    pub OutOfDate: Option<i64>,
    #[serde(default)]
    pub Depends: Vec<String>,
    #[serde(default)]
    pub MakeDepends: Vec<String>,
    #[serde(default)]
    pub CheckDepends: Vec<String>,
    #[serde(default)]
    pub OptDepends: Vec<String>,
    #[serde(default)]
    pub Provides: Vec<String>,
    #[serde(default)]
    pub Conflicts: Vec<String>,
    #[serde(default)]
    pub License: Vec<String>,
}

#[derive(Deserialize)]
struct Response {
    version: i64,
    r#type: String,
    #[serde(default)]
    resultcount: usize,
    #[serde(default)]
    results: Vec<Package>,
    #[serde(default)]
    error: Option<String>,
}

/// Decode a response, rejecting API errors and names that could not be used
/// safely as paths or arguments.
pub fn decode(bytes: &[u8]) -> Result<Vec<Package>> {
    let response: Response = serde_json::from_slice(bytes).map_err(|_| Kind::InvalidAurResponse)?;
    if response.version != 5 || response.r#type == "error" {
        let message = response.error.as_deref().unwrap_or("unsupported API response");
        if message == "Too many package results." {
            return errors::fail(Kind::TooManyAurResults);
        }
        if message == "Query arg too small." {
            return errors::fail(Kind::AurTermTooShort);
        }
        return Err(errors::with(Kind::AurApiError, message));
    }
    if response.results.len() != response.resultcount {
        return errors::fail(Kind::InvalidAurResponse);
    }
    for p in &response.results {
        if !util::valid_name(&p.Name) || !util::valid_name(&p.PackageBase) || p.LastModified < 0 {
            return errors::fail(Kind::InvalidAurResponse);
        }
    }
    Ok(response.results)
}

/// Percent-encode everything outside the URL-safe unreserved set.
pub fn encode(s: &str) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &ch in s.as_bytes() {
        if ch.is_ascii_alphanumeric() || b"-_.~".contains(&ch) {
            out.push(ch as char);
        } else {
            out.extend(['%', HEX[(ch >> 4) as usize] as char, HEX[(ch & 15) as usize] as char]);
        }
    }
    out
}

pub fn search(term: &str, by: SearchBy) -> Result<Vec<Package>> {
    decode(&net::get(&format!("{ENDPOINT}/search/{}?by={}", encode(term), by.name()))?)
}

/// Full metadata for the named packages. Unknown names are simply absent
/// from the result, which is in no particular order.
pub fn info<S: AsRef<str>>(names: &[S]) -> Result<Vec<Package>> {
    let mut all = Vec::new();
    for batch in names.chunks(INFO_BATCH) {
        let mut url = format!("{ENDPOINT}/info?");
        for name in batch {
            url.push_str("arg[]=");
            url.push_str(&encode(name.as_ref()));
            url.push('&');
        }
        all.extend(decode(&net::get(&url)?)?);
    }
    Ok(all)
}

/// The one package with exactly this name, if the AUR has it.
pub fn find(name: &str) -> Result<Option<Package>> {
    Ok(info(&[name])?.into_iter().find(|p| p.Name == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_retain_last_modification_split_bases_and_dependencies() {
        let result = decode(include_bytes!("fixtures/aur.json")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].PackageBase, "sample-base");
        assert_eq!(result[0].LastModified, 1750000000);
        assert_eq!(result[0].Depends[0], "libfoo>=2");
    }

    #[test]
    fn rpc_rejects_malicious_paths_api_errors_and_malformed_bodies() {
        let kind = |body: &str| decode(body.as_bytes()).unwrap_err().kind;
        let bad_path = r#"{"version":5,"type":"info","resultcount":1,"results":[{"Name":"../bad","PackageBase":"bad","Version":"1"}]}"#;
        assert_eq!(kind(bad_path), Kind::InvalidAurResponse);
        assert_eq!(kind(r#"{"version":5,"type":"error","error":"rate limited"}"#), Kind::AurApiError);
        assert_eq!(kind("<html>gateway timeout</html>"), Kind::InvalidAurResponse);
        assert_eq!(kind(r#"{"version":5,"type":"info","resultcount":2,"results":[]}"#), Kind::InvalidAurResponse);
        assert_eq!(encode("foo+bar&x"), "foo%2Bbar%26x");
    }
}
