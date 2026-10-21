//! Paired phones (TRU-151): one-time pairing codes, device tokens and the
//! failed-attempt throttle behind the agent's phone web app.
//!
//! - A pairing code is 128 random bits, single use, and expires after
//!   `PAIRING_CODE_TTL`. Codes live only in memory (an agent restart voids
//!   the outstanding ones), held as SHA-256 digests.
//! - Each pairing also has a short code: 8 Crockford base32 characters
//!   (40 bits), shown as `XXXX-XXXX` for a phone to type. A home-screen
//!   app on iOS keeps its storage apart from Safari's, so it cannot take
//!   the long code from the QR link; it pairs by the short code instead.
//!   Either code spends the same pairing. Failed attempts are throttled
//!   per address (`Throttle`), which keeps 40 bits out of reach within the
//!   ten minutes a code lives.
//! - Redeeming a code issues a device token of 256 random bits. The agent
//!   keeps only the token's SHA-256 digest, with the device's id, name,
//!   scope and times, in `phones.json` (mode 0600) in its config dir.
//!   Tokens are high-entropy, so a plain digest is enough (no password
//!   hashing); digests are compared in constant time.
//! - Revoking a device drops its record and cancels its `CancellationToken`,
//!   which every open socket of that device watches.
//! - Neither codes nor tokens are ever logged.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// How long a pairing code can be redeemed.
pub const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Random bytes in a pairing code (128 bits).
const CODE_BYTES: usize = 16;
/// Characters in a short pairing code: 8 Crockford base32 symbols, 40 bits.
const SHORT_CODE_CHARS: usize = 8;
/// Crockford base32: digits and letters without I, L, O and U.
const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Random bytes in a device token (256 bits).
const TOKEN_BYTES: usize = 32;
/// Outstanding codes kept at once; creating another drops the oldest.
const MAX_PENDING_CODES: usize = 16;
/// Longest device name accepted (characters).
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// A device's last-seen time is written to disk at most this often.
const LAST_SEEN_PERSIST_INTERVAL: u64 = 5 * 60;

pub const PHONES_FILE_NAME: &str = "phones.json";

/// What a paired phone may do. A pairing grants the scope it was created
/// with; a phone that should act is paired again (no in-place upgrade).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneScope {
    /// Read chats; never send anything to Claude.
    View,
    /// Also send messages, answer permission prompts and questions, and
    /// stop the running turn of a chat on the agent.
    Act,
}

impl PhoneScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Act => "act",
        }
    }

    /// `""` is `View`.
    pub fn parse(label: &str) -> Result<Self, String> {
        match label.trim() {
            "" | "view" => Ok(Self::View),
            "act" => Ok(Self::Act),
            other => Err(format!("unknown phone scope {other:?}; use view or act")),
        }
    }

    /// The scope a pairing grants when `requested` is asked for: the one
    /// asked for.
    pub fn granted(requested: Self) -> Self {
        requested
    }

    /// Whether a phone with this scope may send a command into a chat.
    pub fn may_send_commands(self) -> Result<(), &'static str> {
        match self {
            Self::View => Err("this phone is paired to view only; it cannot send to Claude"),
            Self::Act => Ok(()),
        }
    }
}

/// One paired phone as `phones.json` stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhoneRecord {
    pub id: String,
    pub name: String,
    pub scope: PhoneScope,
    /// Hex SHA-256 of the device token.
    pub token_sha256: String,
    pub created_unix_secs: u64,
    #[serde(default)]
    pub last_seen_unix_secs: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PhonesFile {
    #[serde(default)]
    phones: Vec<PhoneRecord>,
}

/// A paired phone, without its token digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneInfo {
    pub id: String,
    pub name: String,
    pub scope: PhoneScope,
    pub created_unix_secs: u64,
    pub last_seen_unix_secs: u64,
}

impl From<&PhoneRecord> for PhoneInfo {
    fn from(record: &PhoneRecord) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            scope: record.scope,
            created_unix_secs: record.created_unix_secs,
            last_seen_unix_secs: record.last_seen_unix_secs,
        }
    }
}

/// A new pairing. `code` is shown once (in the pairing URL) and
/// `short_code` once (in the desktop's pairing panel); neither is kept
/// anywhere in the clear.
#[derive(Debug, Clone)]
pub struct NewPairing {
    pub code: String,
    /// The same pairing as `code`, for typing: `XXXX-XXXX`.
    pub short_code: String,
    pub expires: SystemTime,
    pub scope: PhoneScope,
}

/// A redeemed code: the device's token (shown once, to the phone) and its
/// record.
#[derive(Debug, Clone)]
pub struct Paired {
    pub token: String,
    pub phone: PhoneInfo,
}

/// Why a code could not be redeemed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// Unknown, used or expired. One answer for all three, so a caller
    /// learns nothing about which.
    InvalidCode,
    /// The code was good but the device could not be saved; the code is
    /// spent.
    Storage(String),
}

struct PendingCode {
    digest: [u8; 32],
    /// Digest of the normalized short code (`normalize_short_code`).
    short_digest: [u8; 32],
    device_name: String,
    scope: PhoneScope,
    expires: SystemTime,
}

struct Inner {
    phones: Vec<PhoneRecord>,
    pending: Vec<PendingCode>,
    /// Cancelled when the phone is revoked; sockets hold child tokens.
    revocations: HashMap<String, CancellationToken>,
    /// Last-seen value last written to disk, per phone id.
    persisted_seen: HashMap<String, u64>,
}

pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The agent's paired phones and outstanding pairing codes.
pub struct PhoneRegistry {
    path: PathBuf,
    inner: Mutex<Inner>,
    rng: SystemRandom,
    clock: Clock,
}

impl PhoneRegistry {
    /// Load `phones.json` from `dir` (absent: no phones yet).
    pub fn load(dir: &Path) -> Result<Arc<Self>, String> {
        Self::load_with_clock(dir, Arc::new(SystemTime::now))
    }

    pub fn load_with_clock(dir: &Path, clock: Clock) -> Result<Arc<Self>, String> {
        let path = dir.join(PHONES_FILE_NAME);
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<PhonesFile>(&text)
                .map_err(|err| format!("could not parse {}: {err}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => PhonesFile::default(),
            Err(err) => return Err(format!("could not read {}: {err}", path.display())),
        };
        restrict_permissions(&path)?;
        let persisted_seen = file
            .phones
            .iter()
            .map(|phone| (phone.id.clone(), phone.last_seen_unix_secs))
            .collect();
        Ok(Arc::new(Self {
            path,
            inner: Mutex::new(Inner {
                phones: file.phones,
                pending: Vec::new(),
                revocations: HashMap::new(),
                persisted_seen,
            }),
            rng: SystemRandom::new(),
            clock,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("phone registry poisoned")
    }

    /// A one-time pairing for a phone named `device_name`: a long code
    /// and a short one, either of which spends it.
    pub fn create_pairing(
        &self,
        device_name: &str,
        requested: PhoneScope,
    ) -> Result<NewPairing, String> {
        let device_name = validate_device_name(device_name)?;
        let code = self.random_secret(CODE_BYTES)?;
        let now = (self.clock)();
        let expires = now + PAIRING_CODE_TTL;
        let scope = PhoneScope::granted(requested);
        let mut inner = self.lock();
        inner.pending.retain(|pending| pending.expires > now);
        while inner.pending.len() >= MAX_PENDING_CODES {
            inner.pending.remove(0);
        }
        // A short code no outstanding pairing already has, so a typed code
        // names exactly one pairing.
        let (short_code, short_digest) = loop {
            let short_code = self.random_short_code()?;
            let digest = sha256(&short_code);
            if !inner
                .pending
                .iter()
                .any(|pending| pending.short_digest == digest)
            {
                break (short_code, digest);
            }
        };
        inner.pending.push(PendingCode {
            digest: sha256(&code),
            short_digest,
            device_name,
            scope,
            expires,
        });
        Ok(NewPairing {
            code,
            short_code: format!(
                "{}-{}",
                &short_code[..SHORT_CODE_CHARS / 2],
                &short_code[SHORT_CODE_CHARS / 2..]
            ),
            expires,
            scope,
        })
    }

    /// Spend a pairing by its long code or its short code (as typed: see
    /// `normalize_short_code`): a new device and its token. The pairing is
    /// gone after the first attempt that names it, whatever the outcome.
    pub fn redeem(&self, code: &str) -> Result<Paired, PairError> {
        let digest = sha256(code);
        let short_digest = normalize_short_code(code).map(|short| sha256(&short));
        let now = (self.clock)();
        let mut inner = self.lock();
        inner.pending.retain(|pending| pending.expires > now);
        // Every pending code is compared, in constant time each.
        let mut found = None;
        for (index, pending) in inner.pending.iter().enumerate() {
            let long = constant_time_eq(&pending.digest, &digest);
            let short = short_digest
                .as_ref()
                .is_some_and(|short| constant_time_eq(&pending.short_digest, short));
            if long | short {
                found = Some(index);
            }
        }
        let Some(index) = found else {
            return Err(PairError::InvalidCode);
        };
        let pending = inner.pending.remove(index);
        let token = self
            .random_secret(TOKEN_BYTES)
            .map_err(PairError::Storage)?;
        let created = unix_secs(now);
        let record = PhoneRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: pending.device_name,
            scope: pending.scope,
            token_sha256: hex(&sha256(&token)),
            created_unix_secs: created,
            last_seen_unix_secs: created,
        };
        let mut phones = inner.phones.clone();
        phones.push(record.clone());
        write_phones(&self.path, &phones).map_err(PairError::Storage)?;
        inner.phones = phones;
        inner
            .persisted_seen
            .insert(record.id.clone(), record.last_seen_unix_secs);
        Ok(Paired {
            token,
            phone: PhoneInfo::from(&record),
        })
    }

    /// The phone a device token belongs to, marking it seen. `None` for an
    /// unknown (or revoked) token.
    pub fn authenticate(&self, token: &str) -> Option<PhoneInfo> {
        let digest = sha256(token);
        let now = unix_secs((self.clock)());
        let mut inner = self.lock();
        let mut found = None;
        for (index, phone) in inner.phones.iter().enumerate() {
            let Some(stored) = unhex32(&phone.token_sha256) else {
                continue;
            };
            if constant_time_eq(&stored, &digest) {
                found = Some(index);
            }
        }
        let index = found?;
        inner.phones[index].last_seen_unix_secs = now;
        let phone = PhoneInfo::from(&inner.phones[index]);
        let persisted = inner.persisted_seen.get(&phone.id).copied().unwrap_or(0);
        if now.saturating_sub(persisted) >= LAST_SEEN_PERSIST_INTERVAL {
            match write_phones(&self.path, &inner.phones) {
                Ok(()) => {
                    inner.persisted_seen.insert(phone.id.clone(), now);
                }
                // Last-seen is informational; the request still goes on.
                Err(err) => eprintln!(
                    "[phone] could not record when phone {} was last seen: {err}",
                    phone.id
                ),
            }
        }
        Some(phone)
    }

    pub fn list(&self) -> Vec<PhoneInfo> {
        self.lock().phones.iter().map(PhoneInfo::from).collect()
    }

    /// Forget a phone. Its token stops working at once and its sockets are
    /// told to close. `Ok(false)` when no phone has the id. An error means
    /// the phone is revoked for this run but `phones.json` still lists it.
    pub fn revoke(&self, id: &str) -> Result<bool, String> {
        let mut inner = self.lock();
        let Some(index) = inner.phones.iter().position(|phone| phone.id == id) else {
            return Ok(false);
        };
        let record = inner.phones.remove(index);
        inner.persisted_seen.remove(id);
        if let Some(revoked) = inner.revocations.remove(id) {
            revoked.cancel();
        }
        write_phones(&self.path, &inner.phones).map_err(|err| {
            format!(
                "phone {} is revoked until the agent restarts, but {err}",
                record.name
            )
        })?;
        Ok(true)
    }

    /// A token cancelled when phone `id` is revoked (already cancelled
    /// when it is not paired).
    pub fn revocation(&self, id: &str) -> CancellationToken {
        let mut inner = self.lock();
        if !inner.phones.iter().any(|phone| phone.id == id) {
            let gone = CancellationToken::new();
            gone.cancel();
            return gone;
        }
        inner
            .revocations
            .entry(id.to_string())
            .or_default()
            .child_token()
    }

    /// `SHORT_CODE_CHARS` random Crockford base32 symbols, unformatted.
    fn random_short_code(&self) -> Result<String, String> {
        let mut buf = [0u8; SHORT_CODE_CHARS * 5 / 8];
        self.rng
            .fill(&mut buf)
            .map_err(|_| "the system random source failed".to_string())?;
        let bits = buf
            .iter()
            .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
        Ok((0..SHORT_CODE_CHARS)
            .rev()
            .map(|i| char::from(CROCKFORD_ALPHABET[((bits >> (i * 5)) & 0x1f) as usize]))
            .collect())
    }

    fn random_secret(&self, bytes: usize) -> Result<String, String> {
        let mut buf = vec![0u8; bytes];
        self.rng
            .fill(&mut buf)
            .map_err(|_| "the system random source failed".to_string())?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
    }
}

/// Trimmed, non-empty, at most `MAX_DEVICE_NAME_CHARS`, no control
/// characters.
fn validate_device_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("device name must not be empty".into());
    }
    if name.chars().count() > MAX_DEVICE_NAME_CHARS {
        return Err(format!(
            "device name is longer than {MAX_DEVICE_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("device name must not contain control characters".into());
    }
    Ok(name.to_string())
}

/// A short code as a phone typed it, in the form it was issued in: upper
/// case, without spaces or dashes, with O read as 0 and I or L as 1.
/// `None` when that is not `SHORT_CODE_CHARS` Crockford symbols.
pub fn normalize_short_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect();
    let valid =
        code.len() == SHORT_CODE_CHARS && code.bytes().all(|b| CROCKFORD_ALPHABET.contains(&b));
    valid.then_some(code)
}

fn sha256(secret: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Equal digests, compared without an early exit.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write the phone list atomically: a 0600 temp file renamed over
/// `phones.json`, in a 0700 directory.
fn write_phones(path: &Path, phones: &[PhoneRecord]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    if !dir.exists() {
        create_private_dir(dir)?;
    }
    let json = serde_json::to_string_pretty(&PhonesFile {
        phones: phones.to_vec(),
    })
    .map_err(|err| format!("could not encode the phone list: {err}"))?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|err| format!("could not write {}: {err}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("could not write {}: {err}", tmp.display()))?;
    }
    restrict_permissions(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|err| {
        format!(
            "could not move {} to {}: {err}",
            tmp.display(),
            path.display()
        )
    })
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))
}

/// Make an existing file owner-only (0600). A missing file is fine.
fn restrict_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(format!("could not inspect {}: {err}", path.display())),
        };
        if metadata.permissions().mode() & 0o777 != 0o600 {
            eprintln!(
                "[phone] {} was not owner-only; setting it to 0600",
                path.display()
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|err| format!("could not restrict {}: {err}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Failures before an address is made to wait.
pub(crate) const THROTTLE_FREE_FAILURES: u32 = 5;
/// The first wait; each further failure doubles it.
const THROTTLE_BASE_DELAY: Duration = Duration::from_secs(1);
const THROTTLE_MAX_DELAY: Duration = Duration::from_secs(15 * 60);
/// An address whose last failure is this old starts afresh.
const THROTTLE_FORGET_AFTER: Duration = Duration::from_secs(30 * 60);
/// Addresses tracked before old entries are pruned.
const THROTTLE_MAX_ENTRIES: usize = 1024;

struct ThrottleEntry {
    failures: u32,
    blocked_until: Option<Instant>,
    last_failure: Instant,
}

/// Per-address backoff for failed pair and token attempts: after
/// `THROTTLE_FREE_FAILURES` failures an address waits 1s, then 2s, 4s, …
/// up to 15 minutes, before its next attempt is even looked at. A success
/// clears its record.
#[derive(Default)]
pub struct Throttle {
    entries: HashMap<IpAddr, ThrottleEntry>,
}

impl Throttle {
    /// `Err(wait)` while `ip` must wait before another attempt.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let Some(entry) = self.entries.get(&ip) else {
            return Ok(());
        };
        if now.duration_since(entry.last_failure) >= THROTTLE_FORGET_AFTER {
            self.entries.remove(&ip);
            return Ok(());
        }
        match entry.blocked_until {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        if self.entries.len() >= THROTTLE_MAX_ENTRIES {
            self.entries
                .retain(|_, entry| now.duration_since(entry.last_failure) < THROTTLE_FORGET_AFTER);
        }
        let entry = self.entries.entry(ip).or_insert(ThrottleEntry {
            failures: 0,
            blocked_until: None,
            last_failure: now,
        });
        if now.duration_since(entry.last_failure) >= THROTTLE_FORGET_AFTER {
            entry.failures = 0;
        }
        entry.failures += 1;
        entry.last_failure = now;
        if entry.failures >= THROTTLE_FREE_FAILURES {
            let doublings = (entry.failures - THROTTLE_FREE_FAILURES).min(20);
            let delay = THROTTLE_BASE_DELAY
                .saturating_mul(1u32 << doublings)
                .min(THROTTLE_MAX_DELAY);
            entry.blocked_until = Some(now + delay);
        }
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.entries.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A registry on a temp dir with a clock tests can move (seconds).
    fn registry() -> (tempfile::TempDir, Arc<PhoneRegistry>, Arc<AtomicU64>) {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_800_000_000));
        let clock_now = now.clone();
        let registry = PhoneRegistry::load_with_clock(
            dir.path(),
            Arc::new(move || UNIX_EPOCH + Duration::from_secs(clock_now.load(Ordering::SeqCst))),
        )
        .unwrap();
        (dir, registry, now)
    }

    #[test]
    fn codes_and_tokens_have_the_promised_entropy() {
        let (_dir, registry, _) = registry();
        let pairing = registry.create_pairing("phone", PhoneScope::View).unwrap();
        let code = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&pairing.code)
            .unwrap();
        assert_eq!(code.len(), 16);
        let paired = registry.redeem(&pairing.code).unwrap();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&paired.token)
            .unwrap();
        assert_eq!(token.len(), 32);
        let other = registry.create_pairing("phone", PhoneScope::View).unwrap();
        assert_ne!(other.code, pairing.code);
    }

    #[test]
    fn a_code_works_once() {
        let (_dir, registry, _) = registry();
        let pairing = registry
            .create_pairing("Tracey's iPhone", PhoneScope::View)
            .unwrap();
        let paired = registry.redeem(&pairing.code).unwrap();
        assert_eq!(paired.phone.name, "Tracey's iPhone");
        assert_eq!(paired.phone.scope, PhoneScope::View);
        assert_eq!(
            registry.redeem(&pairing.code).unwrap_err(),
            PairError::InvalidCode
        );
        assert_eq!(
            registry.redeem("nonsense").unwrap_err(),
            PairError::InvalidCode
        );
    }

    #[test]
    fn a_code_expires_after_ten_minutes() {
        let (_dir, registry, now) = registry();
        let early = registry.create_pairing("a", PhoneScope::View).unwrap();
        let late = registry.create_pairing("b", PhoneScope::View).unwrap();
        now.fetch_add(PAIRING_CODE_TTL.as_secs() - 1, Ordering::SeqCst);
        assert!(registry.redeem(&early.code).is_ok());
        now.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            registry.redeem(&late.code).unwrap_err(),
            PairError::InvalidCode
        );
    }

    #[test]
    fn a_short_code_redeems_once() {
        let (_dir, registry, _) = registry();
        let pairing = registry.create_pairing("app", PhoneScope::Act).unwrap();
        let short = &pairing.short_code;
        assert_eq!(short.len(), SHORT_CODE_CHARS + 1);
        assert_eq!(short.as_bytes()[4], b'-');
        assert!(short
            .bytes()
            .filter(|b| *b != b'-')
            .all(|b| CROCKFORD_ALPHABET.contains(&b)));
        // Typed loosely: lower case, a space instead of the dash.
        let typed = short.to_ascii_lowercase().replace('-', " ");
        let paired = registry.redeem(&typed).unwrap();
        assert_eq!(paired.phone.name, "app");
        assert_eq!(paired.phone.scope, PhoneScope::Act);
        assert_eq!(registry.redeem(short).unwrap_err(), PairError::InvalidCode);
        // Short codes differ between pairings.
        let a = registry.create_pairing("a", PhoneScope::View).unwrap();
        let b = registry.create_pairing("b", PhoneScope::View).unwrap();
        assert_ne!(a.short_code, b.short_code);
    }

    #[test]
    fn short_codes_are_normalized_as_typed() {
        assert_eq!(
            normalize_short_code("abcd-efgh").as_deref(),
            Some("ABCDEFGH")
        );
        assert_eq!(
            normalize_short_code(" ab cd\tef-gh ").as_deref(),
            Some("ABCDEFGH")
        );
        // O reads as 0; I and L read as 1.
        assert_eq!(
            normalize_short_code("OoIi-Ll00").as_deref(),
            Some("00111100")
        );
        // U is not Crockford; other symbols and lengths are refused.
        assert_eq!(normalize_short_code("UUUU-UUUU"), None);
        assert_eq!(normalize_short_code("ABCD-EFG"), None);
        assert_eq!(normalize_short_code("ABCD-EFGHJ"), None);
        assert_eq!(normalize_short_code("ABCD_EFGH"), None);
        assert_eq!(normalize_short_code(""), None);
        // A short code typed with the confusable letters redeems.
        let (_dir, registry, _) = registry();
        let pairing = registry.create_pairing("app", PhoneScope::View).unwrap();
        let confused: String = pairing
            .short_code
            .chars()
            .map(|c| match c {
                '0' => 'o',
                '1' => 'l',
                other => other,
            })
            .collect();
        assert!(registry.redeem(&confused).is_ok());
    }

    #[test]
    fn the_long_and_short_codes_spend_one_pairing() {
        let (_dir, registry, _) = registry();
        let first = registry.create_pairing("first", PhoneScope::View).unwrap();
        registry.redeem(&first.code).unwrap();
        assert_eq!(
            registry.redeem(&first.short_code).unwrap_err(),
            PairError::InvalidCode
        );
        let second = registry.create_pairing("second", PhoneScope::View).unwrap();
        registry.redeem(&second.short_code).unwrap();
        assert_eq!(
            registry.redeem(&second.code).unwrap_err(),
            PairError::InvalidCode
        );
        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn a_short_code_expires_with_its_pairing() {
        let (_dir, registry, now) = registry();
        let early = registry.create_pairing("a", PhoneScope::View).unwrap();
        let late = registry.create_pairing("b", PhoneScope::View).unwrap();
        now.fetch_add(PAIRING_CODE_TTL.as_secs() - 1, Ordering::SeqCst);
        assert!(registry.redeem(&early.short_code).is_ok());
        now.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            registry.redeem(&late.short_code).unwrap_err(),
            PairError::InvalidCode
        );
    }

    #[test]
    fn a_pairing_grants_the_scope_it_was_created_with() {
        let (dir, registry, _) = registry();
        let act = registry.create_pairing("phone", PhoneScope::Act).unwrap();
        assert_eq!(act.scope, PhoneScope::Act);
        let paired = registry.redeem(&act.code).unwrap();
        assert_eq!(paired.phone.scope, PhoneScope::Act);
        let view = registry.create_pairing("tablet", PhoneScope::View).unwrap();
        assert_eq!(view.scope, PhoneScope::View);
        assert_eq!(
            registry.redeem(&view.code).unwrap().phone.scope,
            PhoneScope::View
        );
        // The scope survives a restart.
        let reloaded = PhoneRegistry::load(dir.path()).unwrap();
        assert_eq!(
            reloaded.authenticate(&paired.token).map(|p| p.scope),
            Some(PhoneScope::Act)
        );
        assert_eq!(PhoneScope::parse("").unwrap(), PhoneScope::View);
        assert_eq!(PhoneScope::parse("act").unwrap(), PhoneScope::Act);
        assert!(PhoneScope::parse("admin").is_err());
    }

    #[test]
    fn only_act_scope_may_send_commands() {
        assert!(PhoneScope::View.may_send_commands().is_err());
        assert_eq!(PhoneScope::Act.may_send_commands(), Ok(()));
    }

    #[test]
    fn device_names_are_checked() {
        let (_dir, registry, _) = registry();
        assert!(registry.create_pairing("  ", PhoneScope::View).is_err());
        assert!(registry.create_pairing("a\nb", PhoneScope::View).is_err());
        assert!(registry
            .create_pairing(&"x".repeat(65), PhoneScope::View)
            .is_err());
        assert_eq!(
            registry
                .create_pairing("  pixel  ", PhoneScope::View)
                .map(|p| p.scope)
                .unwrap(),
            PhoneScope::View
        );
    }

    #[test]
    fn only_the_token_digest_is_stored_owner_only() {
        let (dir, registry, _) = registry();
        let pairing = registry.create_pairing("phone", PhoneScope::View).unwrap();
        let paired = registry.redeem(&pairing.code).unwrap();
        let path = dir.path().join(PHONES_FILE_NAME);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&paired.token));
        assert!(!text.contains(&pairing.code));
        assert!(!text.contains(&pairing.short_code));
        assert!(text.contains(&hex(&sha256(&paired.token))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // A fresh load (an agent restart) still knows the token.
        let reloaded = PhoneRegistry::load(dir.path()).unwrap();
        assert_eq!(
            reloaded.authenticate(&paired.token).map(|p| p.id),
            Some(paired.phone.id)
        );
    }

    #[test]
    fn tokens_authenticate_until_revoked() {
        let (dir, registry, now) = registry();
        let pairing = registry.create_pairing("phone", PhoneScope::View).unwrap();
        let paired = registry.redeem(&pairing.code).unwrap();
        now.fetch_add(600, Ordering::SeqCst);
        let phone = registry.authenticate(&paired.token).unwrap();
        assert_eq!(phone.last_seen_unix_secs, 1_800_000_600);
        assert!(registry.authenticate("wrong").is_none());
        assert!(registry.authenticate("").is_none());

        let socket = registry.revocation(&phone.id);
        assert!(!socket.is_cancelled());
        assert_eq!(registry.revoke(&phone.id), Ok(true));
        assert!(socket.is_cancelled());
        assert!(registry.authenticate(&paired.token).is_none());
        assert!(registry.revocation(&phone.id).is_cancelled());
        assert_eq!(registry.revoke(&phone.id), Ok(false));
        assert!(registry.list().is_empty());
        let reloaded = PhoneRegistry::load(dir.path()).unwrap();
        assert!(reloaded.authenticate(&paired.token).is_none());
    }

    #[test]
    fn throttle_backs_off_after_repeated_failures() {
        let mut throttle = Throttle::default();
        let ip: IpAddr = "100.64.0.9".parse().unwrap();
        let other: IpAddr = "100.64.0.10".parse().unwrap();
        let start = Instant::now();
        for _ in 0..THROTTLE_FREE_FAILURES - 1 {
            throttle.record_failure(ip, start);
            assert_eq!(throttle.check(ip, start), Ok(()));
        }
        throttle.record_failure(ip, start);
        assert_eq!(throttle.check(ip, start), Err(Duration::from_secs(1)));
        assert_eq!(throttle.check(other, start), Ok(()));
        let later = start + Duration::from_secs(1);
        assert_eq!(throttle.check(ip, later), Ok(()));
        throttle.record_failure(ip, later);
        assert_eq!(throttle.check(ip, later), Err(Duration::from_secs(2)));
        // Doubling stops at the cap.
        for _ in 0..40 {
            throttle.record_failure(ip, later);
        }
        assert_eq!(throttle.check(ip, later), Err(THROTTLE_MAX_DELAY));
        // A success clears the address.
        throttle.record_success(ip);
        assert_eq!(throttle.check(ip, later), Ok(()));
        // Old failures are forgotten.
        for _ in 0..THROTTLE_FREE_FAILURES {
            throttle.record_failure(other, start);
        }
        assert!(throttle.check(other, start).is_err());
        assert_eq!(throttle.check(other, start + THROTTLE_FORGET_AFTER), Ok(()));
    }

    #[test]
    fn constant_time_eq_compares_whole_digests() {
        let a = sha256("a");
        let mut b = a;
        assert!(constant_time_eq(&a, &b));
        b[31] ^= 1;
        assert!(!constant_time_eq(&a, &b));
        assert_eq!(unhex32(&hex(&a)), Some(a));
        assert_eq!(unhex32("zz"), None);
    }
}
