use crate::config::config;
use crate::organizer::{MusicMetadata, MusicOrganizer};
use crate::{error, info, warn};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{BoundTaggedFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{Tag, TagType};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Progress reporter handed in by the worker: (step, state, detail).
pub type Reporter<'a> = &'a (dyn Fn(&str, &str, &str) + Send + Sync);

/// The tag/archival pipeline with organized folder structure:
///
///   1. compress the fetched thumbnail to a 320px JPEG with ffmpeg
///   2. embed title + artist + compressed artwork into the .opus file's
///      Vorbis comments natively (lofty instead of mutagen)
///   3. move the finished file into organized Songs_archive/{Artist}/{Album}/{Track}
///      Long-form content (>=30 min) goes to Songs_archive/Long_Form/{Artist}/
///   4. delete thumbnail junk left in temp_workspace
///
/// Returns the final archive path on success.
pub fn finalize_pipeline(
    opus_path: PathBuf,
    thumb_path: Option<PathBuf>,
    title: &str,
    artist: &str,
    cancel: &Arc<AtomicBool>,
    report: Reporter,
    thumb_dim: i64,
    meta: &MusicMetadata,
) -> Result<PathBuf, String> {
    report("Extract Audio", "done", "Audio extracted (Opus)");

    if !opus_path.exists() {
        report("Thumbnail", "failed", "Source audio file missing");
        return Err("Source audio file missing after extraction".to_string());
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&opus_path);
        return Err("__cancelled__".to_string());
    }

    let stem = opus_path
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "untitled".to_string().into());

    // --- 1. thumbnail -----------------------------------------------------
    report("Thumbnail", "active", "Compressing thumbnail (320px)...");
    let mut compressed_thumb: Option<PathBuf> = None;
    if let Some(thumb) = thumb_path.filter(|t| t.exists()) {
        let destination: PathBuf = {
            let mut d = stem.clone().into_string_lossy_ish();
            d.push_str("_small.jpg");
            PathBuf::from(d)
        };
        let out = Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(&thumb)
            .args(["-vf", &format!("scale={}:-1", thumb_dim), "-q:v", "5"])
            .arg(&destination)
            .output();
        match out {
            Ok(out) if out.status.success() => {
                info!("Thumbnail compressed for: {title}");
                report("Thumbnail", "done", "Thumbnail compressed");
                compressed_thumb = Some(destination);
            }
            _ => {
                error!(
                    "ffmpeg thumbnail compression failed for {title}: {:?}",
                    out.map_err(|e| e.to_string())
                );
                report("Thumbnail", "failed", "Thumbnail compression failed");
            }
        }
    } else {
        report("Thumbnail", "done", "No thumbnail found, skipped");
    }

    // --- 2. metadata --------------------------------------------------------
    report("Metadata", "active", "Embedding metadata (lofty/Opus)...");
    if let Err(e) = tag_opus(&opus_path, title, artist, compressed_thumb.as_deref()) {
        error!("lofty tagging failed for {title}: {e}");
        report("Metadata", "failed", &format!("Metadata embed failed: {e}"));
    } else {
        info!("Metadata embedded for: {title}");
        report("Metadata", "done", "Metadata embedded");
    }

    // --- 3. organize & archive ----------------------------------------------
    report("Archive", "active", "Organizing into archive...");
    
    let organizer = MusicOrganizer::new(PathBuf::from(&config().songs_archive_dir))
        .with_long_form_threshold(30.0)
        .with_musicbrainz(true);
    
    let organized = organizer.organize(meta)
        .map_err(|e| format!("Failed to organize: {e}"))?;
    
    organizer.ensure_dirs(&organized.full_path)
        .map_err(|e| format!("Failed to create directories: {e}"))?;

    if let Err(e) = std::fs::rename(&opus_path, &organized.full_path) {
        match std::fs::copy(&opus_path, &organized.full_path) {
            Ok(_) => {
                let _ = std::fs::remove_file(&opus_path);
            }
            Err(_) => {
                error!("Failed to move {} to archive: {e}", opus_path.display());
                report("Archive", "failed", &format!("Move to archive failed: {e}"));
                return Err(format!("Move to archive failed: {e}"));
            }
        }
    }
    
    let folder_type = if organized.is_long_form { "Long_Form" } else { "Music" };
    report("Archive", "done", &format!("Moved to {}/{}", folder_type, organized.relative_path.display()));

    // Sweep thumbnail leftovers for this file out of temp_workspace.
    remove_if_exists(&format!("{}.jpg", stem.to_string_lossy()));
    remove_if_exists(&format!("{}_small.jpg", stem.to_string_lossy()));
    remove_if_exists(&format!("{}.webp", stem.to_string_lossy()));

    Ok(organized.full_path)
}

fn remove_if_exists(path: &str) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("Could not remove leftover {path}: {e}");
        }
    }
}

/// Writes title/artist/cover into the Opus file's Vorbis comments. The
/// offline player reads these tags, so parity with the mutagen version
/// matters: lofty serializes pictures to the same METADATA_BLOCK_PICTURE
/// base64 chunk mutagen wrote.
fn tag_opus(path: &Path, title: &str, artist: &str, cover: Option<&Path>) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut tagged =
        BoundTaggedFile::read_from(file, ParseOptions::new()).map_err(|e| e.to_string())?;

    // An Opus file's metadata packet is mandatory, so a primary Vorbis
    // comments tag effectively always exists.
    if tagged.primary_tag_mut().is_none() {
        tagged.insert_tag(Tag::new(TagType::VorbisComments));
    }
    let Some(tag) = tagged.primary_tag_mut() else {
        return Err("could not obtain Vorbis comments tag".to_string());
    };

    tag.set_title(title.to_owned());
    tag.set_artist(artist.to_owned());

    if let Some(cover) = cover {
        let data = std::fs::read(cover).map_err(|e| e.to_string())?;
        let picture = Picture::new_unchecked(
            PictureType::CoverFront,
            Some(MimeType::Jpeg),
            Some("Cover".to_owned()),
            data,
        );
        tag.push_picture(picture);
    }

    tagged
        .save(WriteOptions::default())
        .map_err(|e| e.to_string())
}

// Small shim so stem + suffix strings stay simple without pulling in
// unicode-encoding crates; OsStr -> lossy String is fine for filenames
// yt-dlp generated from UTF-8 titles we fetched ourselves.
trait IntoStringLossyIsh {
    fn into_string_lossy_ish(&self) -> String;
}

impl IntoStringLossyIsh for std::ffi::OsString {
    fn into_string_lossy_ish(&self) -> String {
        self.to_string_lossy().into_owned()
    }
}