use std::path::Path;

use chrono::Datelike as _;
use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, ItemValue, TagItem};

use duralumin_core::{Episode, Feed};

// ---- Error -----------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("lofty error: {0}")]
    Lofty(#[from] lofty::error::LoftyError),
}

// ---- Public API ------------------------------------------------------------

/// Write ID3/Vorbis/MP4 tags to a downloaded audio file.
///
/// Uses BestAttempt parsing so existing malformed tags (e.g. non-standard
/// TDRC timestamps written by other tools) don't abort the write.
/// `cover_art` is raw JPEG or PNG bytes; pass `None` to skip art embedding.
pub fn write_tags(
    path: &Path,
    episode: &Episode,
    feed: &Feed,
    cover_art: Option<&[u8]>,
) -> Result<(), MetadataError> {
    let mut tagged_file = Probe::open(path)?
        .options(ParseOptions::new().parsing_mode(ParsingMode::BestAttempt))
        .read()?;

    let has_primary = tagged_file.primary_tag().is_some();
    let tag = if has_primary {
        tagged_file.primary_tag_mut().unwrap()
    } else {
        match tagged_file.first_tag_mut() {
            Some(t) => t,
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "no writable tag found in file, skipping metadata"
                );
                return Ok(());
            }
        }
    };

    let artist = feed.title.as_deref().unwrap_or(&feed.slug).to_string();

    // Title: original episode title from the RSS feed (not the sanitized filename).
    tag.set_title(episode.title.clone());
    tag.set_artist(artist.clone());
    tag.set_album(artist);

    // Year: TYER (ID3v2.3), YEAR (Vorbis), ©day (MP4).
    tag.insert(TagItem::new(
        ItemKey::Year,
        ItemValue::Text(episode.pub_date.year().to_string()),
    ));
    // Full date: TDRC (ID3v2.4-aware players).
    tag.insert(TagItem::new(
        ItemKey::RecordingDate,
        ItemValue::Text(episode.pub_date.format("%Y-%m-%d").to_string()),
    ));

    if let Some(desc) = &episode.description {
        tag.insert(TagItem::new(
            ItemKey::Comment,
            ItemValue::Text(truncate_utf8(desc, 8192)),
        ));
    }

    if let Some(bytes) = cover_art {
        let mime = sniff_image_mime(bytes);
        let pic = Picture::unchecked(bytes.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build();
        tag.push_picture(pic);
    }

    tracing::info!(
        path = %path.display(),
        title = %episode.title,
        year = episode.pub_date.year(),
        "wrote tags"
    );
    tagged_file.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

// ---- Helpers ---------------------------------------------------------------

fn sniff_image_mime(bytes: &[u8]) -> MimeType {
    if bytes.starts_with(b"\xff\xd8\xff") {
        MimeType::Jpeg
    } else if bytes.starts_with(b"\x89PNG") {
        MimeType::Png
    } else {
        MimeType::Jpeg
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
