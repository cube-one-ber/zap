//! HTTPS GET requests: certificate validation, timeouts, a response size
//! limit, and one connection pool reused across requests.
use crate::errors::{self, Kind, Result};
use crate::util::VERSION;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

const RESPONSE_LIMIT: u64 = 16 * 1024 * 1024;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .https_only(true)
            .user_agent(&format!("zap/{VERSION} (Arch Linux AUR helper)"))
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirects(3)
            .build()
    })
}

/// The body of an https URL, at most 16 MiB.
pub fn get(url: &str) -> Result<Vec<u8>> {
    let response = match agent().get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(429, _)) => return Err(errors::with(Kind::RateLimited, host(url))),
        Err(ureq::Error::Status(code, _)) => {
            return Err(errors::with(Kind::RequestFailed, format!("{} answered HTTP {code}", host(url))))
        }
        Err(ureq::Error::Transport(t)) => {
            let reason = t.message().map(str::to_string).unwrap_or_else(|| t.kind().to_string());
            return Err(errors::with(Kind::RequestFailed, format!("{}: {reason}", host(url))));
        }
    };
    let mut body = Vec::new();
    let read = response.into_reader().take(RESPONSE_LIMIT + 1).read_to_end(&mut body);
    match read {
        Ok(n) if n as u64 <= RESPONSE_LIMIT => Ok(body),
        Ok(_) => Err(errors::with(Kind::RequestFailed, format!("{}: response too large", host(url)))),
        Err(e) => Err(errors::with(Kind::RequestFailed, format!("{}: {e}", host(url)))),
    }
}

/// The host of an https URL, for error messages.
fn host(url: &str) -> &str {
    let Some(rest) = url.strip_prefix("https://") else { return url };
    &rest[..rest.find(['/', '?']).unwrap_or(rest.len())]
}

#[cfg(test)]
mod tests {
    #[test]
    fn request_failures_name_the_host_never_the_full_query() {
        assert_eq!(super::host("https://aur.archlinux.org/rpc/v5/info?arg[]=secret"), "aur.archlinux.org");
        assert_eq!(super::host("https://archlinux.org"), "archlinux.org");
    }
}
