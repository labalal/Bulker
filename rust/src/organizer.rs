use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ARTIST_ALIASES: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static KNOWN_ARTISTS: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicMetadata {
    pub title: String,
    pub artist: String,
    pub duration: Option<f64>,
    pub is_long_form: bool,
    pub video_id: String,
    pub source_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizedPath {
    pub relative_path: PathBuf,
    pub full_path: PathBuf,
    pub is_long_form: bool,
    pub artist_folder: String,
}

pub struct MusicOrganizer {
    base_dir: PathBuf,
    long_form_threshold_secs: f64,
    use_musicbrainz: bool,
    musicbrainz_cache: Mutex<HashMap<String, MusicBrainzArtist>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MusicBrainzArtist {
    id: String,
    name: String,
    sort_name: String,
    disambiguation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzSearchResponse {
    artists: Vec<MusicBrainzArtistResult>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzArtistResult {
    id: String,
    name: String,
    #[serde(rename = "sort-name")]
    sort_name: String,
    disambiguation: Option<String>,
    score: Option<i32>,
}

impl MusicOrganizer {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            long_form_threshold_secs: 30.0 * 60.0,
            use_musicbrainz: true,
            musicbrainz_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_long_form_threshold(mut self, minutes: f64) -> Self {
        self.long_form_threshold_secs = minutes * 60.0;
        self
    }

    pub fn with_musicbrainz(mut self, enabled: bool) -> Self {
        self.use_musicbrainz = enabled;
        self
    }

    pub fn organize(&self, meta: &MusicMetadata) -> Result<OrganizedPath> {
        let artist = self.canonicalize_artist(&meta.artist)?;
        let is_long_form = meta.is_long_form || meta.duration.map_or(false, |d| d >= self.long_form_threshold_secs);

        let artist_folder = self.sanitize_filename(&artist);

        let filename = self.build_filename(meta, &artist)?;
        let filename = self.sanitize_filename(&filename);

        let relative_path = if is_long_form {
            let mut path = PathBuf::from("Long_Form");
            path.push(&artist_folder);
            path.push(&filename);
            path
        } else {
            let mut path = PathBuf::from(&artist_folder);
            path.push(&filename);
            path
        };

        let full_path = self.base_dir.join(&relative_path);

        Ok(OrganizedPath {
            relative_path,
            full_path,
            is_long_form,
            artist_folder,
        })
    }

    fn canonicalize_artist(&self, raw_artist: &str) -> Result<String> {
        let cleaned = self.clean_artist_name(raw_artist);
        
        if let Some(alias) = ARTIST_ALIASES.lock().unwrap().get(&cleaned.to_lowercase()) {
            return Ok(alias.clone());
        }

        if self.use_musicbrainz {
            if let Ok(canonical) = self.lookup_musicbrainz(&cleaned) {
                ARTIST_ALIASES.lock().unwrap().insert(cleaned.to_lowercase(), canonical.clone());
                return Ok(canonical);
            }
        }

        let known = KNOWN_ARTISTS.lock().unwrap();
        for known_artist in known.iter() {
            if self.fuzzy_match(&cleaned, known_artist) {
                ARTIST_ALIASES.lock().unwrap().insert(cleaned.to_lowercase(), known_artist.clone());
                return Ok(known_artist.clone());
            }
        }

        Ok(cleaned)
    }

    fn clean_artist_name(&self, raw: &str) -> String {
        let mut cleaned = raw.trim().to_string();
        
        let patterns = [
            r"(?i)\s*\(official.*?\)",
            r"(?i)\s*\[official.*?\]",
            r"(?i)\s*\(music video\)",
            r"(?i)\s*\[music video\]",
            r"(?i)\s*\(lyric.*?\)",
            r"(?i)\s*\[lyric.*?\]",
            r"(?i)\s*\(audio\)",
            r"(?i)\s*\[audio\]",
            r"(?i)\s*\(visualizer\)",
            r"(?i)\s*\[visualizer\]",
            r"(?i)\s*\(live.*?\)",
            r"(?i)\s*\[live.*?\]",
            r"(?i)\s*\(remix.*?\)",
            r"(?i)\s*\[remix.*?\]",
            r"(?i)\s*\(cover.*?\)",
            r"(?i)\s*\[cover.*?\]",
            r"(?i)\s*\(slowed.*?\)",
            r"(?i)\s*\[slowed.*?\]",
            r"(?i)\s*\(sped up.*?\)",
            r"(?i)\s*\[sped up.*?\]",
            r"(?i)\s*\(reverb.*?\)",
            r"(?i)\s*\[reverb.*?\]",
            r"(?i)\s*\(8d.*?\)",
            r"(?i)\s*\[8d.*?\]",
            r"(?i)\s*feat\..*",
            r"(?i)\s*ft\..*",
            r"(?i)\s*featuring.*",
            r"(?i)\s*with.*",
            r"(?i)\s*x\s+.*",
            r"(?i)\s*vs\..*",
            r"(?i)\s*presents?.*",
            r"(?i)\s*pres\..*",
            r"\s*[\[\(].*?[\]\)]\s*$",
            r"\s{2,}",
        ];

        for pattern in patterns {
            let re = Regex::new(pattern).unwrap();
            cleaned = re.replace_all(&cleaned, "").to_string();
        }

        cleaned = cleaned
            .replace("&", "and")
            .replace("’", "'")
            .replace("‘", "'")
            .replace("–", "-")
            .replace("—", "-")
            .trim()
            .to_string();

        if cleaned.is_empty() {
            "Unknown Artist".to_string()
        } else {
            cleaned
        }
    }

    fn lookup_musicbrainz(&self, artist: &str) -> Result<String> {
        if let Some(cached) = self.musicbrainz_cache.lock().unwrap().get(artist) {
            return Ok(cached.name.clone());
        }

        let url = format!(
            "https://musicbrainz.org/ws/2/artist/?query=artist:\"{}\"&fmt=json&limit=5",
            urlencoding::encode(artist)
        );

        let response = ureq::get(&url)
            .set("User-Agent", "Bulker/1.0 (https://github.com/user/bulker)")
            .call();

        if let Ok(resp) = response {
            if let Ok(data) = resp.into_json::<MusicBrainzSearchResponse>() {
                if let Some(best) = data.artists.into_iter().max_by_key(|a| a.score.unwrap_or(0)) {
                    let canonical = best.name.clone();
                    self.musicbrainz_cache.lock().unwrap().insert(artist.to_string(), MusicBrainzArtist {
                        id: best.id,
                        name: canonical.clone(),
                        sort_name: best.sort_name,
                        disambiguation: best.disambiguation,
                    });
                    return Ok(canonical);
                }
            }
        }

        Err(anyhow::anyhow!("No MusicBrainz match"))
    }

    fn fuzzy_match(&self, a: &str, b: &str) -> bool {
        let a_lower = a.to_lowercase();
        let b_lower = b.to_lowercase();
        
        if a_lower == b_lower {
            return true;
        }
        
        let a_words: Vec<&str> = a_lower.split_whitespace().collect();
        let b_words: Vec<&str> = b_lower.split_whitespace().collect();
        
        if a_words.is_empty() || b_words.is_empty() {
            return false;
        }
        
        let common = a_words.iter().filter(|w| b_words.contains(w)).count();
        let max_len = a_words.len().max(b_words.len());
        
        common as f64 / max_len as f64 > 0.7
    }

    fn build_filename(&self, meta: &MusicMetadata, _artist: &str) -> Result<String> {
        Ok(format!("{} [{}].opus", meta.title, meta.video_id))
    }

    fn sanitize_filename(&self, name: &str) -> String {
        name.chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                c if c.is_control() => '_',
                c => c,
            })
            .collect::<String>()
            .trim()
            .to_string()
    }

    pub fn register_known_artist(&self, artist: &str) {
        let cleaned = self.clean_artist_name(artist);
        KNOWN_ARTISTS.lock().unwrap().push(cleaned);
    }

    pub fn add_alias(&self, raw: &str, canonical: &str) {
        ARTIST_ALIASES.lock().unwrap().insert(raw.to_lowercase(), canonical.to_string());
    }

    pub fn ensure_dirs(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
        Ok(())
    }
}

pub fn extract_metadata_from_ytdlp(entry: &crate::ytdlp::Entry, duration: Option<f64>) -> MusicMetadata {
    let (title, artist) = parse_title_artist(&entry.title, &entry.uploader);
    
    let is_long_form = duration.map_or(false, |d| d >= 30.0 * 60.0);

    MusicMetadata {
        title,
        artist,
        duration,
        is_long_form,
        video_id: crate::ytdlp::query_param(&entry.url, "v").unwrap_or_default(),
        source_url: entry.url.clone(),
    }
}

fn parse_title_artist(title: &str, uploader: &str) -> (String, String) {
    let patterns = [
        Regex::new(r"^(.+?)\s*[-–—]\s*(.+?)\s*(?:\(|\[|$)").unwrap(),
        Regex::new(r"^(.+?)\s*\|\s*(.+?)\s*(?:\(|\[|$)").unwrap(),
        Regex::new(r"^(.+?)\s*–\s*(.+?)\s*(?:\(|\[|$)").unwrap(),
    ];

    for re in &patterns {
        if let Some(caps) = re.captures(title) {
            let potential_title = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let potential_artist = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
            
            if !potential_artist.is_empty() && potential_artist.len() < 100 {
                return (potential_title.to_string(), potential_artist.to_string());
            }
        }
    }

    if !uploader.is_empty() && uploader != "Unknown Artist" {
        let clean_uploader = uploader
            .replace(" - Topic", "")
            .replace("VEVO", "")
            .replace("Official", "")
            .trim()
            .to_string();
        
        if !clean_uploader.is_empty() {
            return (title.to_string(), clean_uploader);
        }
    }

    (title.to_string(), "Unknown Artist".to_string())
}