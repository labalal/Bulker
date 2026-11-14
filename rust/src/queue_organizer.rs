use anyhow::{Context, Result};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

/// Queue organizer with priority-based scheduling, deduplication, and quality analysis
pub struct QueueOrganizer {
    priority_index: Arc<RwLock<BTreeMap<i64, Vec<String>>>>,
    pending_items: Arc<DashMap<String, QueueItem>>,
    fingerprints: Arc<RwLock<PrefixTrie>>,
    pub quality_analyzer: QualityAnalyzer,
    pub config: OrganizerConfig,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub url: String,
    pub title: String,
    pub artist: Option<String>,
    pub priority: i64,
    pub status: String,
    pub estimated_size: i64,
    pub actual_size: Option<i64>,
    pub retry_count: i32,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub quality_score: Option<f32>,
    pub metadata: ItemMetadata,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct ItemMetadata {
    pub duration: Option<f64>,
    pub bitrate: Option<i32>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub codec: Option<String>,
    pub file_size: Option<i64>,
    pub format_id: Option<String>,
    pub is_premium: bool,
    pub quality_score: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct OrganizerConfig {
    pub max_queue_size: usize,
    pub priority_boost_large: i64,
    pub priority_penalty_retry: i64,
    pub dedup_threshold: f32,
    pub min_quality_score: f32,
    pub max_concurrent_large: usize,
    pub large_threshold_bytes: i64,
}

impl Default for OrganizerConfig {
    fn default() -> Self {
        Self {
            max_queue_size: 10000,
            priority_boost_large: 1000,
            priority_penalty_retry: -500,
            dedup_threshold: 0.85,
            min_quality_score: 0.3,
            max_concurrent_large: 2,
            large_threshold_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Quality analyzer for song sampling rate, bitrate, and overall quality
pub struct QualityAnalyzer {
    target_sample_rate: i32,
    target_bitrate: i32,
    target_bit_depth: i32,
}

impl QualityAnalyzer {
    pub fn new() -> Self {
        Self {
            target_sample_rate: 48000,
            target_bitrate: 256,
            target_bit_depth: 16,
        }
    }
    
    pub fn analyze(&self, metadata: &ItemMetadata) -> QualityReport {
        let mut score: f32 = 1.0;
        let mut issues = Vec::new();
        let mut recommendations = Vec::new();
        
        if let Some(sr) = metadata.sample_rate {
            if sr < self.target_sample_rate {
                let penalty = (self.target_sample_rate - sr) as f32 / self.target_sample_rate as f32 * 0.3;
                score -= penalty;
                issues.push(format!("Sample rate {}Hz below target {}Hz", sr, self.target_sample_rate));
                recommendations.push("Re-download at higher sample rate".to_string());
            }
        }
        
        if let Some(br) = metadata.bitrate {
            if br < self.target_bitrate {
                let penalty = (self.target_bitrate - br) as f32 / self.target_bitrate as f32 * 0.4;
                score -= penalty;
                issues.push(format!("Bitrate {}kbps below target {}kbps", br, self.target_bitrate));
                recommendations.push("Re-download at 256kbps Opus".to_string());
            }
        }
        
        if !metadata.is_premium {
            score -= 0.2;
            issues.push("Not from Premium source (likely free tier)".to_string());
            recommendations.push("Use Premium cookies for 256kbps Opus".to_string());
        }
        
        score = score.max(0.0).min(1.0);
        
        QualityReport {
            score,
            sample_rate: metadata.sample_rate,
            bitrate: metadata.bitrate,
            bit_depth: None,
            is_premium: metadata.is_premium,
            issues,
            recommendations,
            estimated_quality: Self::classify_quality(score),
        }
    }
    
    pub fn compare_quality(&self, a: &ItemMetadata, b: &ItemMetadata) -> std::cmp::Ordering {
        let report_a = self.analyze(a);
        let report_b = self.analyze(b);
        report_b.score.partial_cmp(&report_a.score).unwrap_or(std::cmp::Ordering::Equal)
    }
    
    fn classify_quality(score: f32) -> QualityClass {
        match score {
            s if s >= 0.9 => QualityClass::Excellent,
            s if s >= 0.7 => QualityClass::Good,
            s if s >= 0.5 => QualityClass::Fair,
            s if s >= 0.3 => QualityClass::Poor,
            _ => QualityClass::VeryPoor,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct QualityReport {
    pub score: f32,
    pub sample_rate: Option<i32>,
    pub bitrate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub is_premium: bool,
    pub issues: Vec<String>,
    pub recommendations: Vec<String>,
    pub estimated_quality: QualityClass,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum QualityClass {
    Excellent,
    Good,
    Fair,
    Poor,
    VeryPoor,
}

/// Prefix trie for longest prefix matching (deduplication)
#[derive(Clone, Debug, Default)]
struct PrefixTrie {
    root: TrieNode,
}

#[derive(Clone, Debug, Default)]
struct TrieNode {
    children: HashMap<char, TrieNode>,
    video_ids: Vec<String>,
    is_terminal: bool,
}

impl PrefixTrie {
    fn new() -> Self {
        Self::default()
    }
    
    fn insert(&mut self, key: &str, video_id: String) {
        let mut node = &mut self.root;
        for ch in key.chars() {
            node = node.children.entry(ch).or_default();
        }
        node.is_terminal = true;
        node.video_ids.push(video_id);
    }
    
    fn find_longest_prefix(&self, key: &str) -> Vec<String> {
        let mut node = &self.root;
        let mut best_match = Vec::new();
        let mut matched_chars = 0;
        
        for ch in key.chars() {
            if let Some(child) = node.children.get(&ch) {
                node = child;
                matched_chars += 1;
                if node.is_terminal && !node.video_ids.is_empty() {
                    best_match = node.video_ids.clone();
                }
            } else {
                break;
            }
        }
        
        if matched_chars < key.len() / 2 {
            return Vec::new();
        }
        
        best_match
    }
    
    fn find_similar(&self, key: &str, threshold: f32) -> Vec<(String, f32)> {
        let mut results = Vec::new();
        self.dfs_search(&self.root, key, 0, 0, key.len() as f32, threshold, &mut results);
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        results
    }
    
    fn dfs_search(&self, node: &TrieNode, key: &str, idx: usize, matched: usize, key_len: f32, threshold: f32, results: &mut Vec<(String, f32)>) {
        if idx >= key.len() {
            if node.is_terminal {
                for vid in &node.video_ids {
                    let similarity = matched as f32 / key_len.max(1.0);
                    if similarity >= threshold {
                        results.push((vid.clone(), similarity));
                    }
                }
            }
            return;
        }
        
        let ch = key.chars().nth(idx).unwrap();
        
        if let Some(child) = node.children.get(&ch) {
            self.dfs_search(child, key, idx + 1, matched + 1, key_len, threshold, results);
        }
        
        for (other_ch, child) in &node.children {
            if *other_ch != ch {
                self.dfs_search(child, key, idx + 1, matched, key_len, threshold, results);
            }
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SongFingerprint {
    pub video_id: String,
    pub title_hash: String,
    pub artist_hash: String,
    pub duration: Option<f64>,
    pub prefix_hash: String,
}

impl QueueOrganizer {
    pub fn new(config: OrganizerConfig) -> Self {
        Self {
            priority_index: Arc::new(RwLock::new(BTreeMap::new())),
            pending_items: Arc::new(DashMap::new()),
            fingerprints: Arc::new(RwLock::new(PrefixTrie::new())),
            quality_analyzer: QualityAnalyzer::new(),
            config,
        }
    }
    
    pub fn add_item(&self, mut item: QueueItem) -> Result<AddResult> {
        let fingerprint = self.generate_fingerprint(&item);
        item.metadata.quality_score = Some(self.quality_analyzer.analyze(&item.metadata).score);
        
        if let Some(existing_id) = self.check_duplicate(&fingerprint, &item) {
            return Ok(AddResult::Duplicate { existing_id, fingerprint });
        }
        
        self.calculate_priority(&mut item);
        
        self.pending_items.insert(item.id.clone(), item.clone());
        
        self.priority_index.write().entry(item.priority)
            .or_default()
            .push(item.id.clone());
        
        self.fingerprints.write().insert(&fingerprint, item.id.clone());
        
        Ok(AddResult::Added { item })
    }
    
    fn generate_fingerprint(&self, item: &QueueItem) -> String {
        let title_norm = normalize_for_matching(&item.title);
        let artist_norm = item.artist.as_ref().map(|s| normalize_for_matching(s)).unwrap_or_default();
        format!("{}|{}", artist_norm, title_norm).to_lowercase()
    }
    
    fn check_duplicate(&self, fingerprint: &str, item: &QueueItem) -> Option<String> {
        let trie = self.fingerprints.read();
        let similar = trie.find_similar(fingerprint, self.config.dedup_threshold);
        
        for (existing_id, similarity) in similar {
            if similarity >= self.config.dedup_threshold {
                if let Some(existing) = self.pending_items.get(&existing_id) {
                    let quality_cmp = self.quality_analyzer.compare_quality(&existing.metadata, &item.metadata);
                    if quality_cmp == std::cmp::Ordering::Greater || quality_cmp == std::cmp::Ordering::Equal {
                        return Some(existing_id);
                    }
                }
            }
        }
        None
    }
    
    fn calculate_priority(&self, item: &mut QueueItem) {
        let mut priority: i64 = 0;
        
        if item.estimated_size > self.config.large_threshold_bytes {
            priority += self.config.priority_boost_large;
        }
        
        if let Some(qs) = item.metadata.quality_score {
            priority += (qs * 1000.0) as i64;
        }
        
        if item.metadata.is_premium {
            priority += 500;
        }
        
        if item.metadata.bitrate.unwrap_or(0) >= 256 {
            priority += 300;
        }
        
        if item.metadata.sample_rate.unwrap_or(0) >= 48000 {
            priority += 200;
        }
        
        priority += self.config.priority_penalty_retry * item.retry_count as i64;
        
        let created = chrono::DateTime::parse_from_rfc3339(&item.created_at)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        let age_hours = (chrono::Utc::now().timestamp() - created) / 3600;
        priority -= age_hours.min(1000) as i64;
        
        item.priority = priority;
        item.metadata.quality_score = Some(self.quality_analyzer.analyze(&item.metadata).score);
    }
    
    pub fn get_next_batch(&self, count: usize, max_large: Option<usize>) -> Vec<QueueItem> {
        let mut result = Vec::new();
        let mut large_count = 0;
        let max_large = max_large.unwrap_or(self.config.max_concurrent_large);
        
        let mut index = self.priority_index.write();
        let priorities: Vec<i64> = index.keys().rev().cloned().collect();
        
        for priority in priorities {
            if result.len() >= count {
                break;
            }
            
            if let Some(ids) = index.get_mut(&priority) {
                let mut i = 0;
                while i < ids.len() && result.len() < count {
                    let id = &ids[i];
                    if let Some(item) = self.pending_items.get(id) {
                        let is_large = item.estimated_size > self.config.large_threshold_bytes;
                        
                        if is_large && large_count >= max_large {
                            i += 1;
                            continue;
                        }
                        
                        if item.status == "pending" {
                            let mut item_clone = item.clone();
                            item_clone.status = "scheduled".to_string();
                            item_clone.started_at = Some(chrono::Utc::now().to_rfc3339());
                            
                            self.pending_items.insert(id.clone(), item_clone.clone());
                            result.push(item_clone);
                            
                            if is_large {
                                large_count += 1;
                            }
                        }
                    }
                    ids.remove(i);
                }
                
                if ids.is_empty() {
                    index.remove(&priority);
                }
            }
        }
        
        result
    }
    
    pub fn requeue_failed(&self, id: &str, _error: &str) -> Result<()> {
        if let Some(mut item) = self.pending_items.get_mut(id) {
            item.retry_count += 1;
            item.status = "pending".to_string();
            item.started_at = None;
            item.completed_at = None;
            
            if item.retry_count > 5 {
                item.status = "failed".to_string();
                item.completed_at = Some(chrono::Utc::now().to_rfc3339());
            } else {
                self.calculate_priority(&mut item);
                self.priority_index.write().entry(item.priority)
                    .or_default()
                    .push(id.to_string());
            }
        }
        Ok(())
    }
    
    pub fn mark_completed(&self, id: &str, actual_size: i64) -> Result<()> {
        if let Some(mut item) = self.pending_items.get_mut(id) {
            item.status = "completed".to_string();
            item.completed_at = Some(chrono::Utc::now().to_rfc3339());
            item.actual_size = Some(actual_size);
            
            if let Some(pos) = self.priority_index.write().get_mut(&item.priority) {
                pos.retain(|x| x != id);
            }
        }
        Ok(())
    }
    
    pub fn get_stats(&self) -> OrganizerStats {
        let pending = self.pending_items.len();
        let by_status = self.pending_items.iter()
            .fold(HashMap::new(), |mut acc, item| {
                *acc.entry(item.status.clone()).or_insert(0) += 1;
                acc
            });
        
        OrganizerStats {
            total_pending: pending,
            by_status,
            priority_levels: self.priority_index.read().len(),
            fingerprint_count: self.fingerprints.read().root.children.len(),
        }
    }
    
    pub fn rebalance_priorities(&self) {
        let items: Vec<_> = self.pending_items.iter()
            .filter(|i| i.status == "pending" || i.status == "scheduled")
            .map(|i| i.key().clone())
            .collect();
        
        for id in items {
            if let Some(mut item) = self.pending_items.get_mut(&id) {
                let old_priority = item.priority;
                self.calculate_priority(&mut item);
                
                if old_priority != item.priority {
                    if let Some(pos) = self.priority_index.write().get_mut(&old_priority) {
                        pos.retain(|x| x != &id);
                    }
                    self.priority_index.write().entry(item.priority)
                        .or_default()
                        .push(id.clone());
                }
            }
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub enum AddResult {
    Added { item: QueueItem },
    Duplicate { existing_id: String, fingerprint: String },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct OrganizerStats {
    pub total_pending: usize,
    pub by_status: HashMap<String, usize>,
    pub priority_levels: usize,
    pub fingerprint_count: usize,
}

fn normalize_for_matching(s: &str) -> String {
    let mut normalized = s.to_lowercase();
    
    let patterns = [
        r"\(official.*?\)",
        r"\[official.*?\]",
        r"\(music video\)",
        r"\[music video\]",
        r"\(lyric.*?\)",
        r"\[lyric.*?\]",
        r"\(audio\)",
        r"\[audio\]",
        r"\(visualizer\)",
        r"\[visualizer\]",
        r"\(live.*?\)",
        r"\[live.*?\]",
        r"\(remix.*?\)",
        r"\[remix.*?\]",
        r"\(cover.*?\)",
        r"\[cover.*?\]",
        r"\(slowed.*?\)",
        r"\[slowed.*?\]",
        r"\(sped up.*?\)",
        r"\[sped up.*?\]",
        r"\(reverb.*?\)",
        r"\[reverb.*?\]",
        r"\(8d.*?\)",
        r"\[8d.*?\]",
        r"feat\..*",
        r"ft\..*",
        r"featuring.*",
        r"with.*",
        r"x\s+.*",
        r"vs\..*",
        r"presents?.*",
        r"pres\..*",
        r"[\[\(].*?[\]\)]",
        r"\s+",
    ];
    
    for pattern in patterns {
        let re = regex::Regex::new(pattern).unwrap();
        normalized = re.replace_all(&normalized, " ").to_string();
    }
    
    normalized = normalized
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    
    normalized.trim().to_string()
}