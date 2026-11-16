//! Fetching packages and databases, as libalpm's dload.c: each file is tried
//! from its servers in order, written to `<name>.part` and renamed into
//! place, with its `.sig` when signatures are wanted. Servers may be
//! `file://` paths or http(s) URLs. Up to `ParallelDownloads` files are
//! fetched at once, interrupted package downloads resume from their
//! `.part`, and root downloads as `DownloadUser` in a sandbox.
use super::log;
use super::sandbox;
use super::{ErrorKind, Handle, Result};
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// One file to fetch.
pub(crate) struct Payload {
    /// The file name on the servers and in the destination directory.
    pub remote_name: String,
    /// A full URL to use instead of the servers (`pacman -U <url>`).
    pub fileurl: Option<String>,
    pub servers: Vec<String>,
    /// Tried first, quietly.
    pub cache_servers: Vec<String>,
    /// Refuse responses larger than this; 0 for no limit.
    pub max_size: u64,
    pub download_signature: bool,
    pub signature_optional: bool,
    /// Fetch even when the local copy is as new as the server's.
    pub force: bool,
    /// Continue a previous partial download from its `.part` (packages).
    pub allow_resume: bool,
}

/// What a successful fetch did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    Downloaded,
    UpToDate,
}

/// How one attempt at one URL failed, which decides what happens to the
/// server and to the partial file.
enum Failure {
    /// The server answered with an HTTP error.
    Status(String),
    /// The file is larger than the payload allows.
    TooBig,
    /// The host could not be resolved: skip it from now on.
    Host(String),
    Other(String),
}

impl Failure {
    fn message(&self) -> &str {
        match self {
            Failure::Status(m) | Failure::Host(m) | Failure::Other(m) => m,
            Failure::TooBig => "expected download size exceeded",
        }
    }
}

/// Soft errors from one host before it is skipped, as `server_error_limit`.
const SERVER_ERROR_LIMIT: i32 = 3;

/// Errors per host this session; -1 after a hard error. As dload.c's
/// `server_errors`, keyed by host since one host often serves many repos.
#[derive(Default)]
pub(crate) struct ServerErrors(Mutex<HashMap<String, i32>>);

impl ServerErrors {
    fn count(&self, url: &str) -> i32 {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&host(url)).unwrap_or(&0)
    }

    /// Servers are skipped after a hard error or too many soft ones.
    fn skip_server(&self, url: &str) -> bool {
        let n = self.count(url);
        n < 0 || n >= SERVER_ERROR_LIMIT
    }

    /// Cache servers are skipped only after a hard error.
    fn skip_cache_server(&self, url: &str) -> bool {
        self.count(url) < 0
    }

    fn soft(&self, url: &str) {
        let host = host(url);
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(host.clone()).or_insert(0);
        if *n < 0 || *n >= SERVER_ERROR_LIMIT {
            return;
        }
        *n += 1;
        if *n >= SERVER_ERROR_LIMIT {
            log::warning(&format!("too many errors from {host}, skipping for the remainder of this transaction\n"));
        }
    }

    fn hard(&self, url: &str) {
        let host = host(url);
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(host.clone()).or_insert(0);
        if *n != -1 {
            *n = -1;
            log::warning(&format!("fatal error from {host}, skipping for the remainder of this transaction\n"));
        }
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .user_agent(&format!("pacman/7.1.0 (zap libalpm/{})", super::LIBALPM_VERSION))
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(10))
            .redirects(10)
            .build()
    })
}

/// The host part of a URL, for messages, as `curl_gethost`.
fn host(url: &str) -> String {
    if url.starts_with("file://") {
        return "disk".into();
    }
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest[..rest.find('/').unwrap_or(rest.len())].to_string()
}

/// An HTTP date, as `Last-Modified` and `If-Modified-Since` use.
fn http_date(time: SystemTime) -> String {
    let secs = time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    let mut buf = [0u8; 64];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%a, %d %b %Y %H:%M:%S GMT".as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn parse_http_date(text: &str) -> Option<SystemTime> {
    let c = std::ffi::CString::new(text).ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let end = unsafe { libc::strptime(c.as_ptr(), c"%a, %d %b %Y %H:%M:%S".as_ptr(), &mut tm) };
    if end.is_null() {
        return None;
    }
    let secs = unsafe { libc::timegm(&mut tm) };
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Copy `from` to `out`, which already holds `start` bytes, failing once
/// the file would pass `max_size` bytes.
fn copy_limited(from: &mut dyn Read, out: &mut std::fs::File, start: u64, max_size: u64) -> std::result::Result<(), Failure> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = start;
    loop {
        let n = from.read(&mut buf).map_err(|e| Failure::Other(super::strerror(&e)))?;
        if n == 0 {
            return Ok(());
        }
        total += n as u64;
        if max_size != 0 && total > max_size {
            return Err(Failure::TooBig);
        }
        out.write_all(&buf[..n]).map_err(|e| Failure::Other(super::strerror(&e)))?;
    }
}

/// One attempt to fetch `url` to `dest`, by way of `dest.part`. With
/// `newer_than`, a source no newer than it is left alone; with `resume`, a
/// non-empty `.part` is continued. The file takes the source's
/// modification time. Also returns the URL after redirects. A failed
/// attempt leaves the `.part` for the caller.
fn fetch_url(
    name: &str,
    url: &str,
    dest: &str,
    max_size: u64,
    newer_than: Option<SystemTime>,
    resume: bool,
) -> std::result::Result<(Fetched, String), Failure> {
    let part = format!("{dest}.part");
    let initial = match (resume, newer_than) {
        (true, None) => std::fs::metadata(&part).map_or(0, |m| m.len()),
        _ => 0,
    };
    if initial != 0 && initial == max_size {
        // The .part file is complete.
        std::fs::rename(&part, dest).map_err(|e| Failure::Other(format!("could not rename {part} to {dest} ({})", super::strerror(&e))))?;
        return Ok((Fetched::Downloaded, url.to_string()));
    }
    let mut out = if initial != 0 {
        log::debug(&format!("{name}: tempfile found, attempting continuation from {initial} bytes\n"));
        std::fs::File::options().append(true).open(&part)
    } else {
        std::fs::File::create(&part)
    }
    .map_err(|e| Failure::Other(format!("could not open file {part}: {}", super::strerror(&e))))?;
    let mut effective = url.to_string();
    let mtime;
    if let Some(path) = url.strip_prefix("file://") {
        let mut src = std::fs::File::open(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Failure::Other(format!("Couldn't open file {path}")),
            _ => Failure::Other(super::strerror(&e)),
        })?;
        let meta = src.metadata().map_err(|e| Failure::Other(super::strerror(&e)))?;
        mtime = meta.modified().ok();
        if let (Some(cond), Some(mtime)) = (newer_than, mtime) {
            if mtime <= cond {
                drop(out);
                let _ = std::fs::remove_file(&part);
                return Ok((Fetched::UpToDate, effective));
            }
        }
        if initial > meta.len() {
            return Err(Failure::Other("Couldn't resume download".into()));
        }
        src.seek(std::io::SeekFrom::Start(initial)).map_err(|e| Failure::Other(super::strerror(&e)))?;
        copy_limited(&mut src, &mut out, initial, max_size)?;
    } else {
        let mut request = agent().get(url);
        if let Some(cond) = newer_than {
            request = request.set("If-Modified-Since", &http_date(cond));
        }
        if initial != 0 {
            request = request.set("Range", &format!("bytes={initial}-"));
        }
        let response = match request.call() {
            Ok(r) if r.status() == 304 => {
                drop(out);
                let _ = std::fs::remove_file(&part);
                return Ok((Fetched::UpToDate, effective));
            }
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(Failure::Status(format!("The requested URL returned error: {code}"))),
            Err(ureq::Error::Transport(t)) => {
                let message = t.message().map(str::to_string).unwrap_or_else(|| t.kind().to_string());
                return Err(match t.kind() {
                    ureq::ErrorKind::Dns => Failure::Host(message),
                    _ => Failure::Other(message),
                });
            }
        };
        if initial != 0 && response.status() != 206 {
            return Err(Failure::Other("HTTP server doesn't seem to support byte ranges. Cannot resume.".into()));
        }
        let length = response.header("Content-Length").and_then(|l| l.parse::<u64>().ok());
        if let Some(length) = length {
            if max_size != 0 && initial + length > max_size {
                return Err(Failure::TooBig);
            }
        }
        effective = response.get_url().to_string();
        mtime = response.header("Last-Modified").and_then(parse_http_date);
        let before = out.metadata().map_or(initial, |m| m.len());
        copy_limited(&mut response.into_reader(), &mut out, initial, max_size)?;
        let got = out.metadata().map_or(0, |m| m.len()) - before;
        if let Some(length) = length.filter(|l| *l != got) {
            log::error(&format!("{name} appears to be truncated: {got}/{length} bytes\n"));
            return Err(Failure::Other(format!("{name} appears to be truncated")));
        }
    }
    drop(out);
    if let Some(mtime) = mtime {
        let _ = std::fs::File::options().write(true).open(&part).and_then(|f| f.set_modified(mtime));
    }
    std::fs::rename(&part, dest).map_err(|e| Failure::Other(format!("could not rename {part} to {dest} ({})", super::strerror(&e))))?;
    Ok((Fetched::Downloaded, effective))
}

impl Handle {
    /// Fetch one payload into `dir`, trying each server until one works,
    /// then its signature. None when it failed; the reasons are logged.
    fn fetch_payload(&self, payload: &Payload, dir: &str, newer_than: Option<SystemTime>) -> Option<Fetched> {
        let dest = format!("{dir}{}", payload.remote_name);
        let part = format!("{dest}.part");
        let url_of = |s: &String| format!("{}/{}", s.trim_end_matches('/'), payload.remote_name);
        // (url, whether it is a cache server)
        let urls: Vec<(String, bool)> = match &payload.fileurl {
            Some(url) => vec![(url.clone(), false)],
            None => payload
                .cache_servers
                .iter()
                .map(|s| (url_of(s), true))
                .chain(payload.servers.iter().map(|s| (url_of(s), false)))
                .collect(),
        };
        let mut tried = false;
        let mut outcome = None;
        let mut unlink_part = !payload.allow_resume;
        for (url, cache) in &urls {
            let skip = if *cache { self.server_errors.skip_cache_server(url) } else { self.server_errors.skip_server(url) };
            if payload.fileurl.is_none() && skip {
                continue;
            }
            tried = true;
            log::debug(&format!("{}: url is {url}\n", payload.remote_name));
            match fetch_url(&payload.remote_name, url, &dest, payload.max_size, newer_than, payload.allow_resume) {
                Ok((fetched, effective)) => {
                    // As curl_check_finished_download: the signature
                    // follows a redirect only to a package or database name.
                    let name = effective.rsplit('/').next().unwrap_or("");
                    let name = name.split('?').next().unwrap_or(name);
                    let sig_base = if name.contains(".db") || name.contains(".pkg") { effective } else { url.clone() };
                    outcome = Some((fetched, sig_base));
                    break;
                }
                Err(failure) => {
                    let message = format!("failed retrieving file '{}' from {} : {}\n", payload.remote_name, host(url), failure.message());
                    match &failure {
                        Failure::Host(_) => {
                            log::error(&message);
                            self.server_errors.hard(url);
                        }
                        Failure::TooBig => {
                            log::error(&message);
                            self.server_errors.soft(url);
                        }
                        _ if *cache => log::debug(&message),
                        _ => {
                            log::error(&message);
                            self.server_errors.soft(url);
                        }
                    }
                    // As dload.c's unlink_on_fail: an error status or an
                    // oversized file is not worth resuming.
                    unlink_part = !payload.allow_resume || matches!(failure, Failure::Status(_) | Failure::TooBig);
                }
            }
        }
        let Some((fetched, url)) = outcome else {
            if !tried {
                log::error(&format!("{}: {}\n", ErrorKind::ServerNone.message(), payload.remote_name));
            }
            let empty = std::fs::metadata(&part).is_ok_and(|m| m.len() == 0);
            if unlink_part || empty {
                let _ = std::fs::remove_file(&part);
            }
            return None;
        };
        if payload.download_signature && fetched == Fetched::Downloaded {
            let sig_dest = format!("{dest}.sig");
            if let Err(why) = fetch_url(&format!("{}.sig", payload.remote_name), &format!("{url}.sig"), &sig_dest, 16 * 1024, None, false) {
                let _ = std::fs::remove_file(format!("{sig_dest}.part"));
                let message = format!("failed retrieving file '{}.sig' from {} : {}\n", payload.remote_name, host(&url), why.message());
                if payload.signature_optional {
                    log::debug(&message);
                } else {
                    log::error(&message);
                    return None;
                }
            }
        }
        Some(fetched)
    }

    /// `curl_download_internal`: fetch the payloads into `dir`, the largest
    /// first, up to `parallel_downloads` at a time. After a failure no new
    /// downloads start; those never started count as failed.
    fn fetch_all(&self, payloads: &[Payload], dir: &str, newer: &[Option<SystemTime>]) -> Vec<Option<Fetched>> {
        let mut order: Vec<usize> = (0..payloads.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(payloads[i].max_size));
        let results: Vec<Mutex<Option<Fetched>>> = payloads.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let work = || loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) else { return };
            let fetched = self.fetch_payload(&payloads[i], dir, newer[i]);
            if fetched.is_none() {
                stop.store(true, Ordering::Relaxed);
            }
            *results[i].lock().unwrap_or_else(|e| e.into_inner()) = fetched;
        };
        let workers = (self.parallel_downloads.max(1) as usize).min(payloads.len());
        if workers <= 1 {
            work();
        } else {
            std::thread::scope(|s| {
                for _ in 0..workers {
                    s.spawn(work);
                }
            });
        }
        results.into_iter().map(|r| r.into_inner().unwrap_or_else(|e| e.into_inner())).collect()
    }

    /// `_alpm_download`: fetch each payload into `dir`. With `if_newer`,
    /// an existing file is only replaced by a newer one (databases).
    /// Failures are reported and make the result an error.
    pub(crate) fn download(&self, payloads: &[Payload], dir: &str, if_newer: bool) -> Result<Vec<Fetched>> {
        // As prepare_resumable_downloads: only a non-empty file sets a time condition.
        let newer: Vec<Option<SystemTime>> = payloads
            .iter()
            .map(|p| {
                let meta = std::fs::metadata(format!("{dir}{}", p.remote_name)).ok()?;
                (if_newer && !p.force && meta.len() != 0).then(|| meta.modified().ok()).flatten()
            })
            .collect();
        let results = if self.use_sandbox() {
            self.download_sandboxed(payloads, dir, &newer)?
        } else {
            self.fetch_all(payloads, dir, &newer)
        };
        results.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| ErrorKind::Retrieve.into())
    }

    /// `curl_download_internal_sandboxed` with `_alpm_download_dir_setup`
    /// and `finalize_download_locations`: a child running as the download
    /// user fetches into a temporary directory under `dir`, then the files
    /// move into `dir`, owned by root. Partial packages move both ways so
    /// they can resume.
    fn download_sandboxed(&self, payloads: &[Payload], dir: &str, newer: &[Option<SystemTime>]) -> Result<Vec<Option<Fetched>>> {
        let name = self.sandboxuser.as_deref().unwrap_or_default();
        let user = match sandbox::lookup_user(name) {
            Ok(Some(user)) => user,
            Ok(None) => {
                log::error(&format!("download user '{name}' does not exist\n"));
                return Err(ErrorKind::RetrievePrepare.into());
            }
            Err(errno) => {
                log::error(&format!("failed to get download user '{name}': {}\n", super::util::errno_str(errno)));
                return Err(ErrorKind::RetrievePrepare.into());
            }
        };
        let mut template = format!("{dir}download-XXXXXX\0").into_bytes();
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            let template = String::from_utf8_lossy(&template[..template.len() - 1]).into_owned();
            log::error(&format!("failed to create temporary download directory {template}: {}\n", last_error()));
            return Err(ErrorKind::RetrievePrepare.into());
        }
        let tmp = String::from_utf8_lossy(&template[..template.len() - 1]).into_owned();
        if std::os::unix::fs::chown(&tmp, Some(user.uid), Some(user.gid)).is_err() {
            log::error(&format!("failed to chown temporary download directory {tmp}: {}\n", last_error()));
            let _ = std::fs::remove_dir(&tmp);
            return Err(ErrorKind::RetrievePrepare.into());
        }
        let tmp = format!("{tmp}/");
        // Hand earlier partial downloads to the child.
        for payload in payloads {
            let src = format!("{dir}{}.part", payload.remote_name);
            let to = format!("{tmp}{}.part", payload.remote_name);
            if std::fs::metadata(&src).is_ok_and(|m| m.len() != 0) && std::fs::rename(&src, &to).is_ok() {
                let _ = std::os::unix::fs::chown(&to, Some(user.uid), Some(user.gid));
            }
        }

        let (results, childsig) = self.run_sandbox_child(payloads, &tmp, newer, user);
        let mut results = results.unwrap_or_else(|| vec![None; payloads.len()]);

        // Move what the child fetched (or partly fetched) into place.
        for (i, (payload, result)) in payloads.iter().zip(results.iter_mut()).enumerate() {
            let file = format!("{tmp}{}", payload.remote_name);
            let found = [file.clone(), format!("{file}.part")].into_iter().find(|f| std::path::Path::new(f).exists());
            if let Some(found) = found {
                if let Err(e) = move_file(&found, dir) {
                    if newer[i].is_none() {
                        log::error(&format!("could not move {found} into {dir} ({})\n", super::strerror(&e)));
                        *result = None;
                    }
                }
            }
            if payload.download_signature {
                let sig = format!("{file}.sig");
                if move_file(&sig, dir).is_err() {
                    let _ = move_file(&format!("{sig}.part"), dir);
                }
            }
        }
        remove_temporary_download_dir(&tmp);
        // Pass on an interrupt only once the partial files are safe.
        if let Some(signal) = childsig {
            unsafe { libc::raise(signal) };
        }
        Ok(results)
    }

    /// Fork the download child and relay its messages. Returns its result
    /// (None if it failed to report one) and the signal that killed it.
    fn run_sandbox_child(
        &self,
        payloads: &[Payload],
        tmp: &str,
        newer: &[Option<SystemTime>],
        user: sandbox::User,
    ) -> (Option<Vec<Option<Fetched>>>, Option<i32>) {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            log::error(&format!("could not create a pipe for the download sandbox: {}\n", last_error()));
            return (None, None);
        }
        let (read_end, write_end) = unsafe {
            use std::os::fd::FromRawFd;
            (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1]))
        };
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            log::error(&format!("could not fork the download sandbox: {}\n", last_error()));
            return (None, None);
        }
        if pid == 0 {
            drop(read_end);
            let pipe = std::sync::Arc::new(sandbox::PipeFrontend(Mutex::new(write_end)));
            log::set_frontend(pipe.clone());
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT] {
                unsafe { libc::signal(signal, libc::SIG_DFL) };
            }
            let code = if std::env::set_current_dir(tmp).is_err() {
                log::error(&format!("could not chdir to download directory {tmp}\n"));
                2
            } else if !sandbox::setup_child(self, user, tmp) {
                log::error(&format!("switching to sandbox user '{}' failed!\n", self.sandboxuser.as_deref().unwrap_or_default()));
                2
            } else {
                let results = self.fetch_all(payloads, tmp, newer);
                let bytes: Vec<u8> = results
                    .iter()
                    .map(|r| match r {
                        None => 0,
                        Some(Fetched::Downloaded) => 1,
                        Some(Fetched::UpToDate) => 2,
                    })
                    .collect();
                if sandbox::send_result(&pipe.0, &bytes) {
                    0
                } else {
                    2
                }
            };
            unsafe { libc::_exit(code) };
        }
        drop(write_end);
        let (result, clean) = sandbox::relay(read_end);
        if !clean {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {}
        let childsig = libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status));
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            return (None, childsig);
        }
        let results = result.filter(|r| r.len() == payloads.len()).map(|r| {
            r.iter()
                .map(|b| match b {
                    1 => Some(Fetched::Downloaded),
                    2 => Some(Fetched::UpToDate),
                    _ => None,
                })
                .collect()
        });
        (results, childsig)
    }
}

fn last_error() -> String {
    super::strerror(&std::io::Error::last_os_error())
}

/// `move_file` with `finalize_download_file`: an empty file is dropped;
/// otherwise it becomes root's, with the umask's permissions, in `dir`.
fn move_file(path: &str, dir: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    if meta.len() == 0 {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    if unsafe { libc::getuid() } == 0 {
        std::os::unix::fs::chown(path, Some(0), Some(0))?;
    }
    let mask = unsafe {
        let mask = libc::umask(0);
        libc::umask(mask);
        mask
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666 & !mask))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    std::fs::rename(path, format!("{dir}{name}"))
}

/// `_alpm_remove_temporary_download_dir`: remove its files, then it.
fn remove_temporary_download_dir(dir: &str) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn payload(name: &str, servers: Vec<String>, resume: bool) -> Payload {
        Payload {
            remote_name: name.into(),
            fileurl: None,
            servers,
            cache_servers: Vec::new(),
            max_size: 0,
            download_signature: false,
            signature_optional: false,
            force: false,
            allow_resume: resume,
        }
    }

    #[test]
    fn a_partial_package_resumes_from_its_part_file() {
        let tmp = TempDir::new();
        let src = tmp.path().join("server");
        let out = tmp.path().join("cache");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("a.pkg.tar"), b"0123456789").unwrap();
        // A previous attempt stopped after four bytes, whose content the
        // resumed download must keep rather than refetch.
        std::fs::write(out.join("a.pkg.tar.part"), b"ABCD").unwrap();
        let server = format!("file://{}", src.display());
        let dir = format!("{}/", out.display());
        let h = Handle::new("/", "/").unwrap();
        let fetched = h.download(&[payload("a.pkg.tar", vec![server.clone()], true)], &dir, false).unwrap();
        assert_eq!(vec![Fetched::Downloaded], fetched);
        assert_eq!(b"ABCD456789", &std::fs::read(out.join("a.pkg.tar")).unwrap()[..]);
        assert!(!out.join("a.pkg.tar.part").exists());

        // Databases never resume.
        std::fs::write(src.join("core.db"), b"database").unwrap();
        std::fs::write(out.join("core.db.part"), b"junk").unwrap();
        h.download(&[payload("core.db", vec![server], false)], &dir, true).unwrap();
        assert_eq!(b"database", &std::fs::read(out.join("core.db")).unwrap()[..]);
    }

    #[test]
    fn parallel_downloads_fetch_everything_and_report_each_failure() {
        let tmp = TempDir::new();
        let src = tmp.path().join("server");
        let out = tmp.path().join("cache");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let server = format!("file://{}", src.display());
        let dir = format!("{}/", out.display());
        let names: Vec<String> = (0..8).map(|i| format!("p{i}.pkg.tar")).collect();
        for name in &names {
            std::fs::write(src.join(name), name.as_bytes()).unwrap();
        }
        let mut h = Handle::new("/", "/").unwrap();
        h.parallel_downloads = 4;
        let payloads: Vec<Payload> = names.iter().map(|n| payload(n, vec![server.clone()], true)).collect();
        assert_eq!(8, h.download(&payloads, &dir, false).unwrap().len());
        for name in &names {
            assert_eq!(name.as_bytes(), &std::fs::read(out.join(name)).unwrap()[..]);
        }
        let missing = [payload("absent.pkg.tar", vec![server], true)];
        assert_eq!(ErrorKind::Retrieve, h.download(&missing, &dir, false).unwrap_err().kind);
    }
}
