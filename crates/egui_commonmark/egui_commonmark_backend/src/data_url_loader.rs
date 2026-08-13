use egui::load::{Bytes, BytesLoadResult, BytesLoader, BytesPoll, LoadError};
use egui::mutex::Mutex;

use std::collections::HashMap;
use std::sync::{mpsc, Arc, LazyLock, Mutex as StdMutex};
use std::task::Poll;

pub fn install_loader(ctx: &egui::Context) {
    if !ctx.is_loader_installed(DataUrlLoader::ID) {
        ctx.add_bytes_loader(std::sync::Arc::new(DataUrlLoader::default()));
    }
}

#[derive(Clone)]
struct Data {
    bytes: Arc<[u8]>,
    mime: Option<String>,
}

type Entry = Poll<Result<Data, String>>;

struct CacheEntry {
    value: Entry,
    last_used: u64,
}

#[derive(Default)]
struct DataUrlCache {
    entries: HashMap<String, CacheEntry>,
    use_tick: u64,
}

struct DecodeJob {
    cache: Arc<Mutex<DataUrlCache>>,
    uri: String,
    ctx: egui::Context,
}

const MAX_DATA_URL_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_DATA_URL_ENCODED_BYTES: usize = 16 * 1024 * 1024;

fn data_url_too_large(uri: &str) -> bool {
    uri.len() > MAX_DATA_URL_ENCODED_BYTES
}

/// Match the scheme normalization used by `data_url::DataUrl::process`
/// without scanning the header or body. The URL parser trims leading C0/space,
/// compares the scheme case-insensitively, and ignores tabs/newlines in it.
fn has_data_url_scheme(uri: &str) -> bool {
    let mut bytes = uri
        .trim_start_matches(|ch| ch <= ' ')
        .bytes()
        .filter(|byte| !matches!(byte, b'\t' | b'\n' | b'\r'));
    b"data:"
        .iter()
        .all(|expected| bytes.next().is_some_and(|actual| actual.eq_ignore_ascii_case(expected)))
}

static DECODE_JOB_TX: LazyLock<mpsc::SyncSender<DecodeJob>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::sync_channel::<DecodeJob>(4);
    let rx = Arc::new(StdMutex::new(rx));
    for worker in 0..2 {
        let rx = Arc::clone(&rx);
        if std::thread::Builder::new()
            .name(format!("data-url-decode-{worker}"))
            .spawn(move || loop {
                let job = match rx.lock() {
                    Ok(receiver) => match receiver.recv() {
                        Ok(job) => job,
                        Err(_) => break,
                    },
                    Err(_) => break,
                };
                decode_job(job);
            })
            .is_err()
        {
            break;
        }
    }
    tx
});

fn entry_bytes(entry: &Entry) -> usize {
    match entry {
        Poll::Ready(Ok(file)) => file.bytes.len() + file.mime.as_ref().map_or(0, String::len),
        Poll::Ready(Err(error)) => error.len(),
        Poll::Pending => 0,
    }
}

fn trim_cache(cache: &mut DataUrlCache, keep: &str) {
    trim_cache_to(cache, keep, MAX_DATA_URL_CACHE_BYTES);
}

fn trim_cache_to(cache: &mut DataUrlCache, keep: &str, max_bytes: usize) {
    while cache
        .entries
        .iter()
        .map(|(uri, entry)| uri.len() + entry_bytes(&entry.value))
        .sum::<usize>()
        > max_bytes
    {
        let victim = cache
            .entries
            .iter()
            .filter(|(uri, entry)| uri.as_str() != keep && !entry.value.is_pending())
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(uri, _)| uri.clone());
        let Some(victim) = victim else { break };
        cache.entries.remove(&victim);
    }
}

fn decode_job(job: DecodeJob) {
    let result = data_url::DataUrl::process(&job.uri)
        .map_err(|error| error.to_string())
        .and_then(|url| {
            url.decode_to_vec()
                .map(|(decoded, _)| {
                    let mime = url.mime_type().to_string();
                    Data {
                        bytes: decoded.into(),
                        mime: (!mime.is_empty()).then_some(mime),
                    }
                })
                .map_err(|error| error.to_string())
        });
    let mut cache = job.cache.lock();
    cache.use_tick = cache.use_tick.wrapping_add(1);
    let tick = cache.use_tick;
    cache.entries.insert(
        job.uri.clone(),
        CacheEntry {
            value: Poll::Ready(result),
            last_used: tick,
        },
    );
    trim_cache(&mut cache, &job.uri);
    drop(cache);
    job.ctx.request_repaint();
}

#[derive(Default)]
pub struct DataUrlLoader {
    cache: Arc<Mutex<DataUrlCache>>,
}

impl DataUrlLoader {
    pub const ID: &'static str = egui::generate_loader_id!(DataUrlLoader);
}

impl BytesLoader for DataUrlLoader {
    fn id(&self) -> &str {
        Self::ID
    }

    fn load(&self, ctx: &egui::Context, uri: &str) -> BytesLoadResult {
        if !has_data_url_scheme(uri) {
            return Err(LoadError::NotSupported);
        }
        if data_url_too_large(uri) {
            return Err(LoadError::Loading(format!(
                "data URL exceeds the {} MiB encoded-size limit",
                MAX_DATA_URL_ENCODED_BYTES / (1024 * 1024)
            )));
        }
        if data_url::DataUrl::process(uri).is_err() {
            return Err(LoadError::NotSupported);
        }

        let mut cache = self.cache.lock();
        cache.use_tick = cache.use_tick.wrapping_add(1);
        let tick = cache.use_tick;
        if let Some(entry) = cache.entries.get_mut(uri) {
            entry.last_used = tick;
            let entry = entry.value.clone();
            match entry {
                Poll::Ready(Ok(file)) => Ok(BytesPoll::Ready {
                    size: None,
                    bytes: Bytes::Shared(file.bytes),
                    mime: file.mime,
                }),
                Poll::Ready(Err(err)) => Err(LoadError::Loading(err)),
                Poll::Pending => Ok(BytesPoll::Pending { size: None }),
            }
        } else {
            cache.entries.insert(
                uri.to_owned(),
                CacheEntry {
                    value: Poll::Pending,
                    last_used: tick,
                },
            );
            drop(cache);

            let job = DecodeJob {
                cache: Arc::clone(&self.cache),
                uri: uri.to_owned(),
                ctx: ctx.clone(),
            };
            match DECODE_JOB_TX.try_send(job) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    self.cache.lock().entries.remove(uri);
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.cache.lock().entries.remove(uri);
                    return Err(LoadError::Loading(
                        "data URL decoder workers are unavailable".to_owned(),
                    ));
                }
            }

            Ok(BytesPoll::Pending { size: None })
        }
    }

    fn forget(&self, uri: &str) {
        let _ = self.cache.lock().entries.remove(uri);
    }

    fn forget_all(&self) {
        self.cache.lock().entries.clear();
    }

    fn byte_size(&self) -> usize {
        self.cache
            .lock()
            .entries
            .iter()
            .map(|(uri, entry)| uri.len() + entry_bytes(&entry.value))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_size_limit_is_strict() {
        assert!(!data_url_too_large(&"x".repeat(MAX_DATA_URL_ENCODED_BYTES)));
        assert!(data_url_too_large(
            &"x".repeat(MAX_DATA_URL_ENCODED_BYTES + 1)
        ));
    }

    #[test]
    fn scheme_check_matches_data_url_normalization() {
        assert!(has_data_url_scheme("data:,hello"));
        assert!(has_data_url_scheme("  DATA:,hello"));
        assert!(has_data_url_scheme("\ndata\t:\n,hello"));
        assert!(!has_data_url_scheme("https://example.com"));
        assert!(!has_data_url_scheme("database:value"));
    }

    #[test]
    fn cache_budget_includes_uri_keys() {
        let mut cache = DataUrlCache::default();
        for tick in 1..=5 {
            cache.entries.insert(
                "x".repeat(16 - tick),
                CacheEntry {
                    value: Poll::Ready(Err("invalid".to_owned())),
                    last_used: tick as u64,
                },
            );
        }
        trim_cache_to(&mut cache, "not-present", 32);
        let bytes: usize = cache
            .entries
            .iter()
            .map(|(uri, entry)| uri.len() + entry_bytes(&entry.value))
            .sum();
        assert!(bytes <= 32);
        assert!(cache.entries.len() < 5);
    }
}
