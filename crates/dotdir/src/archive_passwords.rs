use crate::archive::Location;
use anyhow::{Result, anyhow, bail, ensure};
use dotdir_archive_api::{Password, PasswordAccess, Zeroizing};
use dotdir_core::copy::CancelToken;
use ring::{
    aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

pub const DEFAULT_MINUTES: u64 = 15;
pub const PASSWORD_REQUIRED: &str = "Archive password required";
const MAX_CACHED_ARCHIVES: usize = 64;
const WAIT_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ArchiveKey {
    pub source: Location,
    pub format: &'static str,
}

pub(crate) struct PasswordRequest {
    pub archive: String,
    pub member: PathBuf,
    pub incorrect: bool,
    pub cancel: CancelToken,
    pub reply: mpsc::Sender<Password>,
}

#[derive(Debug)]
pub(crate) struct UnlockCancelled;

impl std::fmt::Display for UnlockCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Archive unlock cancelled")
    }
}

impl std::error::Error for UnlockCancelled {}

struct Vault {
    key: Zeroizing<[u8; 32]>,
    random: SystemRandom,
}

impl Vault {
    fn new() -> Option<Self> {
        let random = SystemRandom::new();
        let mut key = Zeroizing::new([0; 32]);
        random.fill(key.as_mut()).ok()?;
        Some(Self { key, random })
    }

    fn cipher(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, self.key.as_ref()).unwrap())
    }

    fn seal(&self, password: &[u8]) -> Option<Sealed> {
        let mut nonce = [0; 12];
        self.random.fill(&mut nonce).ok()?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(
            password.len() + CHACHA20_POLY1305.tag_len(),
        ));
        bytes.extend_from_slice(password);
        self.cipher()
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut *bytes,
            )
            .ok()?;
        Some(Sealed {
            nonce,
            bytes,
            accepted: Instant::now(),
            accepted_wall: SystemTime::now(),
        })
    }

    fn open(&self, sealed: &Sealed) -> Option<Password> {
        let mut bytes = Password::new(sealed.bytes.to_vec());
        let size = self
            .cipher()
            .open_in_place(
                Nonce::assume_unique_for_key(sealed.nonce),
                Aad::empty(),
                &mut bytes,
            )
            .ok()?
            .len();
        bytes.truncate(size);
        Some(bytes)
    }
}

struct Sealed {
    nonce: [u8; 12],
    bytes: Zeroizing<Vec<u8>>,
    accepted: Instant,
    accepted_wall: SystemTime,
}

#[derive(Default)]
pub(crate) struct PasswordScope(Mutex<HashMap<ArchiveKey, Arc<Sealed>>>);

struct Cache {
    lifetime: Duration,
    passwords: HashMap<ArchiveKey, Arc<Sealed>>,
    gates: HashMap<ArchiveKey, Weak<PromptGate>>,
}

#[derive(Default)]
struct PromptGate {
    active: Mutex<()>,
    cancelled: AtomicU64,
}

impl Cache {
    fn expire(&mut self) {
        let now = SystemTime::now();
        self.passwords.retain(|_, secret| {
            secret.accepted.elapsed() < self.lifetime
                && now
                    .duration_since(secret.accepted_wall)
                    .is_ok_and(|age| age < self.lifetime)
        });
        self.gates.retain(|_, gate| gate.strong_count() > 0);
    }
}

pub(crate) struct ArchivePasswords {
    vault: Option<Vault>,
    cache: Mutex<Cache>,
    prompt: Option<Box<dyn Fn(PasswordRequest) + Send + Sync>>,
}

impl ArchivePasswords {
    pub fn new(minutes: u64, prompt: impl Fn(PasswordRequest) + Send + Sync + 'static) -> Self {
        Self::with_prompt(minutes, Some(Box::new(prompt)))
    }

    pub fn unattended() -> Self {
        Self::with_prompt(0, None)
    }

    fn with_prompt(
        minutes: u64,
        prompt: Option<Box<dyn Fn(PasswordRequest) + Send + Sync>>,
    ) -> Self {
        Self {
            vault: prompt.as_ref().and_then(|_| Vault::new()),
            cache: Mutex::new(Cache {
                lifetime: Duration::from_secs(minutes.saturating_mul(60)),
                passwords: HashMap::new(),
                gates: HashMap::new(),
            }),
            prompt,
        }
    }

    pub fn set_minutes(&self, minutes: u64) {
        let mut cache = self.cache.lock().unwrap();
        cache.lifetime = Duration::from_secs(minutes.saturating_mul(60));
        cache.expire();
    }

    pub fn expire(&self) {
        self.cache.lock().unwrap().expire();
    }

    fn gate(&self, key: &ArchiveKey) -> Arc<PromptGate> {
        let mut cache = self.cache.lock().unwrap();
        cache.expire();
        if let Some(gate) = cache.gates.get(key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(PromptGate::default());
        cache.gates.insert(key.clone(), Arc::downgrade(&gate));
        gate
    }

    fn cached(&self, key: &ArchiveKey) -> Option<Arc<Sealed>> {
        let mut cache = self.cache.lock().unwrap();
        cache.expire();
        cache.passwords.get(key).cloned()
    }

    fn remember(&self, key: &ArchiveKey, secret: Arc<Sealed>) {
        let mut cache = self.cache.lock().unwrap();
        if cache.lifetime.is_zero() {
            return;
        }
        cache.expire();
        cache.passwords.insert(key.clone(), secret);
        if cache.passwords.len() > MAX_CACHED_ARCHIVES
            && let Some(oldest) = cache
                .passwords
                .iter()
                .min_by_key(|(_, value)| value.accepted)
                .map(|(key, _)| key.clone())
        {
            cache.passwords.remove(&oldest);
        }
    }

    fn reject(&self, key: &ArchiveKey, secret: &Arc<Sealed>) {
        let mut cache = self.cache.lock().unwrap();
        if cache
            .passwords
            .get(key)
            .is_some_and(|cached| Arc::ptr_eq(cached, secret))
        {
            cache.passwords.remove(key);
        }
    }
}

pub(crate) struct ArchiveAccess<'a> {
    pub service: &'a ArchivePasswords,
    pub scope: Option<&'a PasswordScope>,
    pub key: &'a ArchiveKey,
    pub cancel: &'a CancelToken,
    pub interactive: bool,
}

impl ArchiveAccess<'_> {
    fn check_cancelled(&self) -> Result<()> {
        ensure!(!self.cancel.is_cancelled(), UnlockCancelled);
        Ok(())
    }

    fn ask(&self, member: &Path, incorrect: bool) -> Result<Password> {
        self.check_cancelled()?;
        let Some(prompt) = self.service.prompt.as_ref().filter(|_| self.interactive) else {
            bail!(PASSWORD_REQUIRED);
        };
        let (reply, response) = mpsc::channel();
        prompt(PasswordRequest {
            archive: self.key.source.display(),
            member: member.to_path_buf(),
            incorrect,
            cancel: self.cancel.clone(),
            reply,
        });
        loop {
            self.check_cancelled()?;
            match response.recv_timeout(WAIT_INTERVAL) {
                Ok(password) => return Ok(password),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(UnlockCancelled.into()),
            }
        }
    }
}

impl PasswordAccess for ArchiveAccess<'_> {
    fn unlock(
        &self,
        member: &Path,
        verify: &mut dyn FnMut(&[u8]) -> Result<bool>,
    ) -> Result<Password> {
        let gate = self.service.gate(self.key);
        let generation = gate.cancelled.load(Ordering::Acquire);
        let _guard = loop {
            self.check_cancelled()?;
            ensure!(
                gate.cancelled.load(Ordering::Acquire) == generation,
                UnlockCancelled
            );
            match gate.active.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) if self.interactive => {
                    std::thread::sleep(WAIT_INTERVAL)
                }
                Err(std::sync::TryLockError::WouldBlock) => bail!(PASSWORD_REQUIRED),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(anyhow!("Archive unlock failed"));
                }
            }
        };
        ensure!(
            gate.cancelled.load(Ordering::Acquire) == generation,
            UnlockCancelled
        );
        let scope = self.scope.map(|scope| &scope.0);
        let scoped = scope.and_then(|scope| scope.lock().unwrap().get(self.key).cloned());
        let candidate = scoped.or_else(|| self.service.cached(self.key));
        let mut incorrect = false;
        if let Some(secret) = candidate
            && let Some(vault) = &self.service.vault
            && let Some(password) = vault.open(&secret)
        {
            if verify(&password)? {
                if let Some(scope) = scope {
                    scope.lock().unwrap().insert(self.key.clone(), secret);
                }
                return Ok(password);
            }
            if self.interactive {
                self.service.reject(self.key, &secret);
            }
            if let Some(scope) = scope {
                scope.lock().unwrap().remove(self.key);
            }
            incorrect = true;
        }

        loop {
            let password = match self.ask(member, incorrect) {
                Ok(password) => password,
                Err(error) => {
                    if error.is::<UnlockCancelled>() {
                        gate.cancelled.fetch_add(1, Ordering::AcqRel);
                    }
                    return Err(error);
                }
            };
            self.check_cancelled()?;
            if verify(&password)? {
                if let Some(vault) = &self.service.vault
                    && let Some(secret) = vault.seal(&password)
                {
                    let secret = Arc::new(secret);
                    self.service.remember(self.key, secret.clone());
                    if let Some(scope) = scope {
                        scope.lock().unwrap().insert(self.key.clone(), secret);
                    }
                }
                return Ok(password);
            }
            incorrect = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_expires_and_zero_keeps_only_the_current_operation() {
        let count = Arc::new(AtomicU64::new(0));
        let calls = count.clone();
        let service = ArchivePasswords::new(DEFAULT_MINUTES, move |request| {
            calls.fetch_add(1, Ordering::Relaxed);
            request
                .reply
                .send(Password::new(b"fixture-secret".to_vec()))
                .unwrap();
        });
        let key = ArchiveKey {
            source: PathBuf::from("test.zip").into(),
            format: "zip",
        };
        let cancel = CancelToken::new();
        let unlock = |scope: Option<&PasswordScope>| {
            ArchiveAccess {
                service: &service,
                scope,
                key: &key,
                cancel: &cancel,
                interactive: true,
            }
            .unlock(Path::new("file.txt"), &mut |password| {
                Ok(password == b"fixture-secret")
            })
            .unwrap();
        };

        unlock(None);
        unlock(None);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        {
            let mut cache = service.cache.lock().unwrap();
            let secret = Arc::get_mut(cache.passwords.get_mut(&key).unwrap()).unwrap();
            assert!(
                !secret
                    .bytes
                    .windows(b"fixture-secret".len())
                    .any(|bytes| bytes == b"fixture-secret")
            );
            secret.accepted_wall -= Duration::from_secs(DEFAULT_MINUTES * 60);
        }
        service.expire();
        assert!(service.cached(&key).is_none());
        unlock(None);
        assert_eq!(count.load(Ordering::Relaxed), 2);

        service.set_minutes(0);
        assert!(service.cached(&key).is_none());
        let scope = PasswordScope::default();
        unlock(Some(&scope));
        unlock(Some(&scope));
        assert_eq!(count.load(Ordering::Relaxed), 3);
        unlock(Some(&PasswordScope::default()));
        assert_eq!(count.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn dismissing_the_prompt_cancels_the_request_without_caching() {
        let service = ArchivePasswords::new(DEFAULT_MINUTES, drop);
        let key = ArchiveKey {
            source: PathBuf::from("cancelled.zip").into(),
            format: "zip",
        };
        let cancel = CancelToken::new();
        let error = ArchiveAccess {
            service: &service,
            scope: None,
            key: &key,
            cancel: &cancel,
            interactive: true,
        }
        .unlock(Path::new("file.txt"), &mut |_| unreachable!())
        .unwrap_err();
        assert!(error.is::<UnlockCancelled>());
        assert!(service.cached(&key).is_none());
    }
}
