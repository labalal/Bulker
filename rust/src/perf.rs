use std::sync::Arc;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::hash::Hash;
use lru::LruCache;
use std::num::NonZeroUsize;

/// High-performance LRU cache with TTL support
pub struct TtlLruCache<K, V> {
    inner: RwLock<LruCache<K, (V, Instant)>>,
    ttl: Duration,
    capacity: usize,
}

impl<K, V> TtlLruCache<K, V>
where
    K: Hash + Eq + Clone,
    V: Clone,
{
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(LruCache::new(NonZeroUsize::new(capacity).unwrap())),
            ttl,
            capacity,
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        let mut inner = self.inner.write();
        if let Some((value, inserted)) = inner.get(key) {
            if inserted.elapsed() < self.ttl {
                return Some(value.clone());
            } else {
                inner.pop(key);
            }
        }
        None
    }

    pub fn put(&self, key: K, value: V) {
        let mut inner = self.inner.write();
        inner.put(key, (value, Instant::now()));
        
        if inner.len() > self.capacity {
            while inner.len() > self.capacity {
                inner.pop_lru();
            }
        }
    }

    pub fn invalidate(&self, key: &K) {
        self.inner.write().pop(key);
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

/// MusicBrainz lookup cache
pub static MUSICBRAINZ_CACHE: once_cell::sync::Lazy<TtlLruCache<String, Option<String>>> = once_cell::sync::Lazy::new(|| {
    TtlLruCache::new(5000, Duration::from_secs(86400 * 7))
});

/// Quality analysis cache - defined in queue_organizer to avoid circular dependency
pub fn quality_cache() -> Arc<TtlLruCache<String, crate::queue_organizer::QualityReport>> {
    static CACHE: once_cell::sync::Lazy<Arc<TtlLruCache<String, crate::queue_organizer::QualityReport>>> = once_cell::sync::Lazy::new(|| {
        Arc::new(TtlLruCache::new(10000, Duration::from_secs(3600)))
    });
    CACHE.clone()
}

/// Batch database writer for high-throughput writes
pub struct BatchWriter<T> {
    buffer: Vec<T>,
    batch_size: usize,
    flush_interval: Duration,
    last_flush: Instant,
    flush_fn: Arc<dyn Fn(Vec<T>) -> anyhow::Result<()> + Send + Sync>,
}

impl<T> BatchWriter<T> {
    pub fn new(
        batch_size: usize,
        flush_interval: Duration,
        flush_fn: impl Fn(Vec<T>) -> anyhow::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            buffer: Vec::with_capacity(batch_size),
            batch_size,
            flush_interval,
            last_flush: Instant::now(),
            flush_fn: Arc::new(flush_fn),
        }
    }

    pub fn push(&mut self, item: T) -> anyhow::Result<()> {
        self.buffer.push(item);
        
        if self.buffer.len() >= self.batch_size || self.last_flush.elapsed() >= self.flush_interval {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        
        let batch = std::mem::take(&mut self.buffer);
        self.last_flush = Instant::now();
        (self.flush_fn)(batch)?;
        Ok(())
    }
}

impl<T> Drop for BatchWriter<T> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// SIMD-accelerated string operations using `simdutf8` compatible patterns
#[cfg(target_arch = "x86_64")]
pub mod simd {
    use std::arch::x86_64::*;
    
    /// Fast ASCII lowercase conversion using SIMD
    #[inline]
    pub fn to_lowercase_ascii_simd(input: &[u8]) -> Vec<u8> {
        let mut output: Vec<u8> = Vec::with_capacity(input.len());
        let mut chunks = input.chunks_exact(32);
        let remainder = chunks.remainder();
        
        for chunk in chunks {
            unsafe {
                let v = _mm256_loadu_si256(chunk.as_ptr() as *const __m256i);
                let mask = _mm256_cmpgt_epi8(v, _mm256_set1_epi8(b'@' as i8));
                let add = _mm256_and_si256(mask, _mm256_set1_epi8(0x20));
                let result = _mm256_add_epi8(v, add);
                _mm256_storeu_si256(output.as_mut_ptr().add(output.len()) as *mut __m256i, result);
                output.set_len(output.len() + 32);
            }
        }
        
        for &b in remainder {
            output.push(if b >= b'A' && b <= b'Z' { b + 0x20 } else { b });
        }
        
        output
    }
    
    /// Fast whitespace check
    #[inline]
    pub fn is_whitespace_simd(b: u8) -> bool {
        b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
    }
}

/// Non-SIMD fallback
#[cfg(not(target_arch = "x86_64"))]
pub mod simd {
    pub fn to_lowercase_ascii_simd(input: &[u8]) -> Vec<u8> {
        input.iter().map(|&b| if b >= b'A' && b <= b'Z' { b + 0x20 } else { b }).collect()
    }
    
    #[inline]
    pub fn is_whitespace_simd(b: u8) -> bool {
        b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
    }
}

/// Fast string normalization using SIMD where available
pub fn normalize_string_fast(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(input.len());
    
    let mut word_start = 0;
    let mut in_word = false;
    
    for (i, &b) in bytes.iter().enumerate() {
        let is_alpha = b.is_ascii_alphanumeric();
        
        if is_alpha && !in_word {
            word_start = i;
            in_word = true;
        } else if !is_alpha && in_word {
            if word_start < i {
                if !result.is_empty() {
                    result.push(' ');
                }
                // Add word with lowercase
                let word = &input[word_start..i];
                for c in word.chars() {
                    result.push(c.to_ascii_lowercase());
                }
            }
            in_word = false;
        }
    }
    
    if in_word && word_start < bytes.len() {
        if !result.is_empty() {
            result.push(' ');
        }
        let word = &input[word_start..];
        for c in word.chars() {
            result.push(c.to_ascii_lowercase());
        }
    }
    
    result
}

/// Parallel phone scanner using rayon
#[cfg(feature = "rayon")]
pub mod parallel {
    use rayon::prelude::*;
    use super::*;
    
    pub fn scan_directories_parallel(
        dirs: &[&str],
        scanner: &crate::adb_scanner::AdbScanner,
    ) -> anyhow::Result<Vec<crate::adb_scanner::ScannedSong>> {
        let results: Vec<_> = dirs.par_iter()
            .filter_map(|dir| {
                scanner.scan_directory(dir).ok()
            })
            .flatten()
            .collect();
        
        Ok(results)
    }
}

/// Async batch processor for high-throughput operations
pub struct AsyncBatchProcessor<T> {
    sender: tokio::sync::mpsc::UnboundedSender<T>,
    flush_handle: tokio::task::JoinHandle<()>,
}

impl<T> AsyncBatchProcessor<T>
where
    T: Send + Sync + 'static,
{
    pub fn new(
        batch_size: usize,
        flush_interval: Duration,
        mut processor: impl FnMut(Vec<T>) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        
        let handle = tokio::spawn(async move {
            let mut buffer = Vec::with_capacity(batch_size);
            let mut interval = tokio::time::interval(flush_interval);
            
            loop {
                tokio::select! {
                    Some(item) = rx.recv() => {
                        buffer.push(item);
                        if buffer.len() >= batch_size {
                            let batch = std::mem::take(&mut buffer);
                            if let Err(e) = processor(batch) {
                                eprintln!("Batch processing error: {}", e);
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if !buffer.is_empty() {
                            let batch = std::mem::take(&mut buffer);
                            if let Err(e) = processor(batch) {
                                eprintln!("Batch processing error: {}", e);
                            }
                        }
                    }
                }
            }
        });
        
        Self {
            sender: tx,
            flush_handle: handle,
        }
    }
    
    pub fn send(&self, item: T) -> anyhow::Result<()> {
        self.sender.send(item)?;
        Ok(())
    }
}

impl<T> Drop for AsyncBatchProcessor<T> {
    fn drop(&mut self) {
        self.flush_handle.abort();
    }
}

/// Memory pool for reducing allocations in hot paths
pub struct MemoryPool<T> {
    pool: parking_lot::Mutex<Vec<T>>,
    factory: fn() -> T,
}

impl<T> MemoryPool<T> {
    pub fn new(factory: fn() -> T) -> Self {
        Self {
            pool: parking_lot::Mutex::new(Vec::new()),
            factory,
        }
    }
    
    pub fn get(&self) -> T {
        self.pool.lock().pop().unwrap_or_else(self.factory)
    }
    
    pub fn put(&self, item: T) {
        self.pool.lock().push(item);
    }
}

impl<T> Default for MemoryPool<T>
where
    T: Default,
{
    fn default() -> Self {
        Self::new(T::default)
    }
}

/// Fast path for common operations
pub mod fast_path {
    use super::*;
    
    /// Quick artist name extraction without full parsing
    pub fn extract_artist_quick(title: &str) -> Option<String> {
        let separators = [" - ", " – ", " — ", " | ", " · "];
        for sep in &separators {
            if let Some(pos) = title.find(sep) {
                let potential_artist = &title[pos + sep.len()..].trim();
                if !potential_artist.is_empty() && potential_artist.len() < 100 {
                    return Some(potential_artist.to_string());
                }
            }
        }
        None
    }
    
    /// Quick check if string looks like a YouTube video ID
    pub fn is_video_id(s: &str) -> bool {
        s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
    
    /// Quick bitrate estimation from file size and duration
    pub fn estimate_bitrate(file_size: u64, duration_secs: f64) -> Option<u32> {
        if duration_secs > 0.0 && file_size > 0 {
            let bitrate = (file_size as f64 * 8.0 / duration_secs / 1000.0) as u32;
            Some(bitrate)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_normalize_string_fast() {
        assert_eq!(normalize_string_fast("Hello World"), "hello world");
        assert_eq!(normalize_string_fast("HELLO   WORLD"), "hello world");
        assert_eq!(normalize_string_fast("Test-Feat.Artist"), "test feat artist");
    }
    
    #[test]
    fn test_fast_path() {
        assert_eq!(fast_path::extract_artist_quick("Song - Artist"), Some("Artist".to_string()));
        assert!(fast_path::is_video_id("abc123def45"));
        assert!(!fast_path::is_video_id("too_long_video_id"));
        
        let br = fast_path::estimate_bitrate(5_000_000, 180.0);
        assert!(br.is_some());
    }
}