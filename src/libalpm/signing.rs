//! OpenPGP signature checks, as libalpm's signing.c, done by running
//! `gpg --verify` against the keyring in GPGDir and reading its status
//! lines instead of linking gpgme. Missing keys can be imported, with the
//! front end's consent, from a Web Key Directory or the keyserver gpg is
//! configured with.
use super::log::{self, Question};
use super::{ErrorKind, Handle, Result};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigStatus {
    Valid,
    KeyExpired,
    SigExpired,
    KeyUnknown,
    KeyDisabled,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigValidity {
    Full,
    Marginal,
    Never,
    Unknown,
}

/// One signature's verdict. `key` is the signer's user id, or the key id
/// or fingerprint when the key is not in the keyring. `fingerprint` is the
/// signing key's primary fingerprint when gpg reports it, else its key id.
#[derive(Clone, Debug)]
pub struct SigResult {
    pub key: String,
    pub uid: Option<String>,
    pub fingerprint: String,
    pub status: SigStatus,
    pub validity: SigValidity,
}

/// Read gpg's `--status-fd` output into one result per signature.
fn parse_status(text: &str) -> Vec<SigResult> {
    let mut results: Vec<SigResult> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("[GNUPG:] ") else { continue };
        let mut words = rest.splitn(3, ' ');
        let (keyword, first, tail) = (words.next().unwrap_or(""), words.next().unwrap_or(""), words.next().unwrap_or(""));
        if keyword == "NEWSIG" {
            results.push(SigResult {
                key: String::new(),
                uid: None,
                fingerprint: String::new(),
                status: SigStatus::Invalid,
                validity: SigValidity::Unknown,
            });
            continue;
        }
        let Some(current) = results.last_mut() else { continue };
        let name = if tail.is_empty() { first.to_string() } else { tail.to_string() };
        if matches!(keyword, "GOODSIG" | "EXPSIG" | "EXPKEYSIG" | "REVKEYSIG" | "BADSIG") {
            current.uid = (!tail.is_empty()).then(|| tail.to_string());
            if current.fingerprint.is_empty() {
                current.fingerprint = first.to_string();
            }
        }
        match keyword {
            "GOODSIG" => (current.status, current.key) = (SigStatus::Valid, name),
            "EXPSIG" => (current.status, current.key) = (SigStatus::SigExpired, name),
            "EXPKEYSIG" => (current.status, current.key) = (SigStatus::KeyExpired, name),
            "REVKEYSIG" | "BADSIG" => (current.status, current.key) = (SigStatus::Invalid, name),
            // VALIDSIG <fpr> <date> ... <primary key fpr>
            "VALIDSIG" => {
                if let Some(primary) = rest.split(' ').nth(10).or(Some(first)).filter(|f| !f.is_empty()) {
                    current.fingerprint = primary.to_string();
                }
            }
            "ERRSIG" => {
                current.key = first.to_string();
                // ERRSIG <keyid> <algo> <hash> <class> <time> <rc> <fpr>
                current.fingerprint = match rest.split(' ').nth(7) {
                    Some(fpr) if !fpr.is_empty() && fpr != "-" => fpr.to_string(),
                    _ => first.to_string(),
                };
                // ERRSIG <keyid> <algo> <hash> <class> <time> <rc>: rc 9 is a missing key.
                current.status = if rest.split(' ').nth(6) == Some("9") {
                    SigStatus::KeyUnknown
                } else {
                    SigStatus::Invalid
                };
            }
            "NO_PUBKEY" => current.status = SigStatus::KeyUnknown,
            "TRUST_ULTIMATE" | "TRUST_FULLY" => current.validity = SigValidity::Full,
            "TRUST_MARGINAL" => current.validity = SigValidity::Marginal,
            "TRUST_NEVER" => current.validity = SigValidity::Never,
            "TRUST_UNDEFINED" => current.validity = SigValidity::Unknown,
            _ => {}
        }
    }
    results
}

impl Handle {
    /// `_alpm_gpgme_checksig`: verify `path` against the base64 signature
    /// from a sync database or, failing that, the detached `<path>.sig`.
    /// `Err(SigMissing)` when there is no signature at all.
    fn checksig(&self, path: &str, base64_sig: Option<&str>) -> Result<Vec<SigResult>> {
        use base64::Engine;
        let sig = match base64_sig.filter(|s| !s.is_empty()) {
            Some(text) => match base64::engine::general_purpose::STANDARD.decode(text.trim()) {
                Ok(bytes) => bytes,
                Err(_) => {
                    log::error(&format!("{path}: signature format error\n"));
                    return Err(ErrorKind::SigInvalid.into());
                }
            },
            None => match std::fs::read(format!("{path}.sig")) {
                Ok(bytes) => bytes,
                Err(_) => {
                    log::debug(&format!("sig path {path}.sig could not be opened\n"));
                    return Err(ErrorKind::SigMissing.into());
                }
            },
        };
        if !std::path::Path::new(&format!("{}pubring.gpg", self.gpgdir)).exists()
            && !std::path::Path::new(&format!("{}pubring.kbx", self.gpgdir)).exists()
        {
            log::error("Public keyring not found; have you run 'pacman-key --init'?\n");
            return Err(ErrorKind::NotAFile.into());
        }
        log::debug(&format!("checking signature for {path}\n"));
        let child = Command::new("gpg")
            .args(["--homedir", &self.gpgdir, "--batch", "--no-tty", "--status-fd", "1", "--verify", "-", path])
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => {
                log::error(&format!("could not run gpg ({})\n", super::strerror(&e)));
                return Err(ErrorKind::Gpgme.into());
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&sig);
        }
        let output = child.wait_with_output().map_err(|_| ErrorKind::Gpgme)?;
        let results = parse_status(&String::from_utf8_lossy(&output.stdout));
        if results.is_empty() {
            log::error(&format!("{path}: signature format error\n"));
            return Err(ErrorKind::SigInvalid.into());
        }
        Ok(results)
    }

    /// `_alpm_check_pgp_helper`: whether the signatures on `path` are
    /// acceptable for the given trust options. The results come back either
    /// way so failures can be explained with `process_siglist`.
    pub(crate) fn check_pgp(
        &self,
        path: &str,
        base64_sig: Option<&str>,
        optional: bool,
        marginal: bool,
        unknown: bool,
    ) -> (bool, Vec<SigResult>) {
        match self.checksig(path, base64_sig) {
            Err(e) if e.kind == ErrorKind::SigMissing => {
                log::debug(if optional { "missing optional signature\n" } else { "missing required signature\n" });
                (optional, Vec::new())
            }
            Err(_) => {
                log::debug("signature check failed\n");
                (false, Vec::new())
            }
            Ok(results) => {
                let ok = results.iter().all(|r| match r.status {
                    SigStatus::Valid | SigStatus::KeyExpired => match r.validity {
                        SigValidity::Full => true,
                        SigValidity::Marginal => marginal,
                        SigValidity::Unknown => unknown,
                        SigValidity::Never => false,
                    },
                    _ => false,
                });
                log::debug(if ok { "signature is valid\n" } else { "signature is not valid\n" });
                (ok, results)
            }
        }
    }
}

impl Handle {
    /// `_alpm_process_siglist`: explain why signatures were refused, and
    /// offer to import missing or expired keys. True when a key was
    /// imported, so checking again may succeed.
    pub(crate) fn process_siglist(&self, identifier: &str, results: &[SigResult], optional: bool, marginal: bool, unknown: bool) -> bool {
        if !optional && results.is_empty() {
            log::error(&format!("{identifier}: missing required signature\n"));
        }
        let mut retry = false;
        for r in results {
            let name = &r.key;
            let message = match r.status {
                SigStatus::Valid => match r.validity {
                    SigValidity::Full => None,
                    SigValidity::Marginal if !marginal => Some(format!("signature from \"{name}\" is marginal trust")),
                    SigValidity::Unknown if !unknown => Some(format!("signature from \"{name}\" is unknown trust")),
                    SigValidity::Never => Some(format!("signature from \"{name}\" should never be trusted")),
                    _ => None,
                },
                SigStatus::KeyExpired => {
                    log::error(&format!("{identifier}: signature from \"{name}\" is expired\n"));
                    retry |= self.key_import(r.uid.as_deref(), &r.fingerprint);
                    None
                }
                SigStatus::KeyUnknown => {
                    // It may have been imported on an earlier call.
                    if self.key_in_keychain(&r.fingerprint) != Some(true) {
                        log::error(&format!("{identifier}: key \"{name}\" is unknown\n"));
                        retry |= self.key_import(r.uid.as_deref(), &r.fingerprint);
                    }
                    None
                }
                SigStatus::SigExpired => Some(format!("signature from \"{name}\" is expired")),
                SigStatus::KeyDisabled => Some(format!("key \"{name}\" is disabled")),
                SigStatus::Invalid => Some(format!("signature from \"{name}\" is invalid")),
            };
            if let Some(message) = message {
                log::error(&format!("{identifier}: {message}\n"));
            }
        }
        retry
    }

    fn gpg(&self) -> Command {
        let mut command = Command::new("gpg");
        command.args(["--homedir", &self.gpgdir, "--batch", "--no-tty"]).env("LC_ALL", "C").stdin(Stdio::null());
        command
    }

    /// `_alpm_key_in_keychain`: whether an unexpired key `fpr` is in the
    /// keyring; None when gpg could not be asked.
    pub(crate) fn key_in_keychain(&self, fpr: &str) -> Option<bool> {
        let mut known = self.known_keys.lock().unwrap_or_else(|e| e.into_inner());
        if known.iter().any(|k| k == fpr) {
            log::debug(&format!("key {fpr} found in cache\n"));
            return Some(true);
        }
        log::debug(&format!("looking up key {fpr} locally\n"));
        let output = self.gpg().args(["--with-colons", "--list-keys", "--", fpr]).stderr(Stdio::null()).output();
        let output = match output {
            Ok(output) => output,
            Err(e) => {
                log::debug(&format!("gpg error: {}\n", super::strerror(&e)));
                return None;
            }
        };
        let listing = String::from_utf8_lossy(&output.stdout);
        // pub:<validity>:...; validity "e" is an expired key.
        let Some(validity) = listing.lines().find_map(|l| l.strip_prefix("pub:")).and_then(|l| l.split(':').next()) else {
            log::debug("key lookup failed, unknown key\n");
            return Some(false);
        };
        if validity == "e" {
            log::debug("key lookup success, but key is expired\n");
            return Some(false);
        }
        log::debug("key lookup success, key exists\n");
        known.push(fpr.to_string());
        Some(true)
    }

    /// `key_import_wkd`: fetch the key for `email` from its domain's Web
    /// Key Directory; it only counts if it is the key `fpr`.
    fn key_import_wkd(&self, email: &str, fpr: &str) -> bool {
        log::debug(&format!("looking up key {email} using WKD\n"));
        let found = self.gpg().args(["--locate-external-keys", "--", email]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        if !found.is_ok_and(|s| s.success()) {
            log::debug("gpg error: key lookup failed\n");
            return false;
        }
        if self.key_in_keychain(fpr) == Some(true) {
            return true;
        }
        log::debug("key lookup failed: WKD imported wrong fingerprint or key expired\n");
        false
    }

    /// `_alpm_key_import`: ask the front end, then fetch the key `fpr`,
    /// first by WKD from the address in `uid`, then from the keyserver.
    pub(crate) fn key_import(&self, uid: Option<&str>, fpr: &str) -> bool {
        if super::util::access(&format!("{}pubring.gpg", self.gpgdir), libc::W_OK).is_err() {
            log::error("keyring is not writable\n");
            return false;
        }
        let mut question = Question::ImportKey { fingerprint: fpr, uid: uid.unwrap_or(""), import: false };
        log::question(&mut question);
        let Question::ImportKey { import: true, .. } = question else { return false };
        if let Some(email) = uid.and_then(email_from_uid) {
            if self.key_import_wkd(email, fpr) {
                return true;
            }
        }
        log::debug(&format!("looking up key {fpr} remotely\n"));
        let received = self.gpg().args(["--recv-keys", "--", fpr]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        if !received.is_ok_and(|s| s.success()) {
            log::error(&format!("key \"{fpr}\" could not be looked up remotely\n"));
            return false;
        }
        if self.key_in_keychain(fpr) == Some(true) {
            return true;
        }
        log::error(&format!("key \"{fpr}\" could not be imported\n"));
        false
    }

    /// The keys the detached or embedded signature `sig` was made with that
    /// are missing from the keyring, as `alpm_extract_keyid` plus
    /// `_alpm_key_in_keychain`. None when the signature cannot be parsed.
    pub(crate) fn missing_keys(&self, identifier: &str, sig: &[u8]) -> Option<Vec<String>> {
        let keys = extract_keyid(identifier, sig)?;
        Some(keys.into_iter().filter(|k| self.key_in_keychain(k) == Some(false)).collect())
    }
}

/// `email_from_uid`: the address in "Name <address>".
fn email_from_uid(uid: &str) -> Option<&str> {
    let start = uid.rfind('<')?;
    let end = start + uid[start..].rfind('>')?;
    Some(&uid[start + 1..end])
}

/// Read a big-endian length of `n` bytes at `at`.
fn be(sig: &[u8], at: usize, n: usize) -> usize {
    sig[at..at + n].iter().fold(0, |acc, b| (acc << 8) | *b as usize)
}

/// `alpm_extract_keyid`: the issuer key ids of every v4 signature packet in
/// `sig`, read from its hashed and unhashed subpackets. Malformed input is
/// reported and gives None.
pub(crate) fn extract_keyid(identifier: &str, sig: &[u8]) -> Option<Vec<String>> {
    fn complain<T>(identifier: &str, problem: &str) -> Option<T> {
        log::error(&format!("{identifier}: {problem}\n"));
        None
    }
    // As libalpm's length_check: `a` more bytes must fit after `pos`.
    let fits = |pos: usize, a: usize| a != 0 && pos <= sig.len() && sig.len() - pos > a;
    let mut keys = Vec::new();
    let subpackets = |pos: usize, plen: usize, keys: &mut Vec<String>| -> Option<()> {
        let mut spos = pos;
        while spos < pos + plen {
            let slen;
            if sig[spos] < 192 {
                slen = sig[spos] as usize;
                spos += 1;
            } else if sig[spos] < 255 {
                if !fits(spos, 2) {
                    return complain(identifier, "signature format error");
                }
                slen = ((sig[spos] as usize - 192) << 8) + sig[spos + 1] as usize + 192;
                spos += 2;
            } else {
                if !fits(spos, 5) {
                    return complain(identifier, "signature format error");
                }
                slen = be(sig, spos + 1, 4);
                spos += 5;
            }
            if sig[spos] == 16 {
                // Issuer key id.
                if !fits(spos, 8) {
                    return complain(identifier, "signature format error");
                }
                keys.push(sig[spos + 1..spos + 9].iter().map(|b| format!("{b:02X}")).collect());
                break;
            }
            if !fits(spos, slen) {
                return complain(identifier, "signature format error");
            }
            spos += slen;
        }
        Some(())
    };
    let mut pos = 0;
    while pos < sig.len() {
        if sig[pos] & 0x80 == 0 {
            return complain(identifier, "signature format error");
        }
        let blen;
        if sig[pos] & 0x40 != 0 {
            // New packet format.
            if !fits(pos, 1) {
                return complain(identifier, "signature format error");
            }
            pos += 1;
            if sig[pos] < 192 {
                if !fits(pos, 1) {
                    return complain(identifier, "signature format error");
                }
                blen = sig[pos] as usize;
                pos += 1;
            } else if sig[pos] < 224 {
                if !fits(pos, 2) {
                    return complain(identifier, "signature format error");
                }
                blen = ((sig[pos] as usize - 192) << 8) + sig[pos + 1] as usize + 192;
                pos += 2;
            } else if sig[pos] == 255 {
                if !fits(pos, 5) {
                    return complain(identifier, "signature format error");
                }
                blen = be(sig, pos + 1, 4);
                pos += 5;
            } else {
                // Partial body lengths are not supported.
                return complain(identifier, "unsupported signature format");
            }
        } else {
            // Old packet format.
            let n = match sig[pos] & 0x03 {
                0 => 1,
                1 => 2,
                2 => 4,
                _ => return complain(identifier, "unsupported signature format"),
            };
            if !fits(pos, n + 1) {
                return complain(identifier, "signature format error");
            }
            blen = be(sig, pos + 1, n);
            pos += n + 1;
        }
        if !fits(pos, 2) {
            return complain(identifier, "signature format error");
        }
        if sig[pos] != 4 {
            // Only version 4 signature packets.
            return complain(identifier, "unsupported signature format");
        }
        if sig[pos + 1] != 0x00 {
            // Not a signature of a binary document.
            return complain(identifier, "signature format error");
        }
        pos += 4;
        if !fits(pos, 2) {
            return complain(identifier, "signature format error");
        }
        let hlen = be(sig, pos, 2);
        if !fits(pos, hlen + 2) {
            return complain(identifier, "signature format error");
        }
        pos += 2;
        subpackets(pos, hlen, &mut keys)?;
        pos += hlen;
        let ulen = be(sig, pos, 2);
        if !fits(pos, ulen + 2) {
            return complain(identifier, "signature format error");
        }
        pos += 2;
        subpackets(pos, ulen, &mut keys)?;
        pos += blen.checked_sub(hlen + 8).map_or_else(|| sig.len(), |rest| rest);
    }
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpg_status_lines_become_one_verdict_per_signature() {
        let status = "[GNUPG:] NEWSIG\n\
            [GNUPG:] GOODSIG 76A5EF9054449A5C Pierre <pierre@example.org>\n\
            [GNUPG:] VALIDSIG ABC 2024-01-01\n\
            [GNUPG:] TRUST_FULLY 0 pgp\n\
            [GNUPG:] NEWSIG\n\
            [GNUPG:] ERRSIG 5C2E46A0F53A76ED 17 2 00 1212955144 9 -\n\
            [GNUPG:] NO_PUBKEY 5C2E46A0F53A76ED\n";
        let results = parse_status(status);
        assert_eq!(results.len(), 2);
        assert_eq!((results[0].status, results[0].validity), (SigStatus::Valid, SigValidity::Full));
        assert_eq!(results[0].key, "Pierre <pierre@example.org>");
        assert_eq!(results[1].status, SigStatus::KeyUnknown);
        assert_eq!(results[1].key, "5C2E46A0F53A76ED");
    }
}
