use crate::config::Config;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

// ──────────────────────────────────────────────────────────────
// Voice profile storage and matching.
//
// Stored in ~/.minutes/voices.db — separate from graph.db
// (which is a rebuildable cache that wipes on rebuild).
// ──────────────────────────────────────────────────────────────

/// Resolve the model version tag for the currently configured embedding model.
/// Falls back to the cam++-lm version string if the config value is unrecognized.
pub fn model_version(config: &Config) -> &'static str {
    crate::diarize::embedding_model_for_config(config).version
}

#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceProfile {
    pub person_slug: String,
    pub name: String,
    pub enrolled_at: String,
    pub updated_at: String,
    pub sample_count: u32,
    pub source: String,
    pub model_version: String,
}

pub struct VoiceProfileWithEmbedding {
    pub person_slug: String,
    pub name: String,
    pub embedding: Vec<f32>,
    pub sample_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceMatch {
    pub person_slug: String,
    pub name: String,
    pub similarity: f32,
    pub runner_up_similarity: Option<f32>,
    pub margin: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeetingEmbeddingRecord {
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub speech_secs: f64,
    #[serde(default)]
    pub segment_count: usize,
    #[serde(default)]
    pub model_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MeetingEmbeddingsSidecar {
    version: u32,
    speakers: HashMap<String, MeetingEmbeddingRecord>,
}

pub fn db_path() -> PathBuf {
    let base = dirs::home_dir()
        .expect("home directory must exist")
        .join(".minutes");
    std::fs::create_dir_all(&base).ok();
    base.join("voices.db")
}

pub fn open_db() -> Result<Connection, VoiceError> {
    open_db_at(&db_path())
}

pub fn open_db_at(path: &Path) -> Result<Connection, VoiceError> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS voice_profiles (
            id INTEGER PRIMARY KEY,
            person_slug TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            embedding BLOB NOT NULL,
            enrolled_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            sample_count INTEGER DEFAULT 1,
            source TEXT NOT NULL,
            model_version TEXT NOT NULL
        );",
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
        }
    }
    Ok(conn)
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn save_profile(
    conn: &Connection,
    slug: &str,
    name: &str,
    embedding: &[f32],
    source: &str,
    model_version: &str,
) -> Result<(), VoiceError> {
    let now = chrono::Local::now().to_rfc3339();
    let blob = embedding_to_bytes(embedding);
    conn.execute(
        "INSERT INTO voice_profiles (person_slug, name, embedding, enrolled_at, updated_at, sample_count, source, model_version)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
         ON CONFLICT(person_slug) DO UPDATE SET
            name = excluded.name, embedding = excluded.embedding, updated_at = excluded.updated_at,
            sample_count = sample_count + 1, source = excluded.source, model_version = excluded.model_version",
        params![slug, name, blob, now, now, source, model_version],
    )?;
    Ok(())
}

pub fn save_profile_blended(
    conn: &Connection,
    slug: &str,
    name: &str,
    new_embedding: &[f32],
    source: &str,
    model_version: &str,
) -> Result<(), VoiceError> {
    if let Some(existing) = load_profile_with_embedding(conn, slug)? {
        let total = existing.sample_count as f32 + 1.0;
        let old_weight = existing.sample_count as f32;
        let blended: Vec<f32> = existing
            .embedding
            .iter()
            .zip(new_embedding.iter())
            .map(|(old, new)| (old * old_weight + new) / total)
            .collect();
        save_profile(conn, slug, name, &blended, source, model_version)
    } else {
        save_profile(conn, slug, name, new_embedding, source, model_version)
    }
}

fn load_profile_with_embedding(
    conn: &Connection,
    slug: &str,
) -> Result<Option<VoiceProfileWithEmbedding>, VoiceError> {
    let mut stmt = conn.prepare("SELECT person_slug, name, embedding, sample_count FROM voice_profiles WHERE person_slug = ?1")?;
    match stmt.query_row(params![slug], |row| {
        let blob: Vec<u8> = row.get(2)?;
        Ok(VoiceProfileWithEmbedding {
            person_slug: row.get(0)?,
            name: row.get(1)?,
            embedding: bytes_to_embedding(&blob),
            sample_count: row.get(3)?,
        })
    }) {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn list_profiles(conn: &Connection) -> Result<Vec<VoiceProfile>, VoiceError> {
    let mut stmt = conn.prepare("SELECT person_slug, name, enrolled_at, updated_at, sample_count, source, model_version FROM voice_profiles ORDER BY updated_at DESC")?;
    let profiles = stmt
        .query_map([], |row| {
            Ok(VoiceProfile {
                person_slug: row.get(0)?,
                name: row.get(1)?,
                enrolled_at: row.get(2)?,
                updated_at: row.get(3)?,
                sample_count: row.get(4)?,
                source: row.get(5)?,
                model_version: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(profiles)
}

pub fn load_all_with_embeddings(
    conn: &Connection,
) -> Result<Vec<VoiceProfileWithEmbedding>, VoiceError> {
    let mut stmt =
        conn.prepare("SELECT person_slug, name, embedding, sample_count FROM voice_profiles")?;
    let profiles = stmt
        .query_map([], |row| {
            let blob: Vec<u8> = row.get(2)?;
            Ok(VoiceProfileWithEmbedding {
                person_slug: row.get(0)?,
                name: row.get(1)?,
                embedding: bytes_to_embedding(&blob),
                sample_count: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(profiles)
}

pub fn delete_profile(conn: &Connection, slug: &str) -> Result<bool, VoiceError> {
    Ok(conn.execute(
        "DELETE FROM voice_profiles WHERE person_slug = ?1",
        params![slug],
    )? > 0)
}

pub fn match_embedding(
    embedding: &[f32],
    profiles: &[VoiceProfileWithEmbedding],
    threshold: f32,
) -> Option<VoiceMatch> {
    match_embedding_with_margin(embedding, profiles, threshold, 0.0)
}

pub fn match_embedding_with_margin(
    embedding: &[f32],
    profiles: &[VoiceProfileWithEmbedding],
    threshold: f32,
    min_margin: f32,
) -> Option<VoiceMatch> {
    let mut best_profile: Option<&VoiceProfileWithEmbedding> = None;
    let mut best_sim = f32::MIN;
    let mut runner_up_sim = f32::MIN;

    for p in profiles {
        let sim = cosine_similarity(embedding, &p.embedding);
        tracing::debug!(
            profile = %p.name,
            similarity = format!("{:.4}", sim),
            "voice embedding comparison"
        );
        if sim > best_sim {
            runner_up_sim = best_sim;
            best_sim = sim;
            best_profile = Some(p);
        } else if sim > runner_up_sim {
            runner_up_sim = sim;
        }
    }

    let runner_up_similarity = if runner_up_sim == f32::MIN {
        None
    } else {
        Some(runner_up_sim)
    };
    let margin = runner_up_similarity.map_or(f32::INFINITY, |runner| best_sim - runner);
    let matched = best_profile
        .filter(|_| best_sim >= threshold && margin >= min_margin)
        .map(|p| VoiceMatch {
            person_slug: p.person_slug.clone(),
            name: p.name.clone(),
            similarity: best_sim,
            runner_up_similarity,
            margin,
        });

    if let Some(ref result) = matched {
        tracing::info!(
            matched = %result.name,
            similarity = format!("{:.4}", result.similarity),
            margin = format!("{:.4}", result.margin),
            "voice profile matched"
        );
    } else if !profiles.is_empty() {
        tracing::info!(
            best_similarity = format!("{:.4}", best_sim),
            runner_up_similarity = runner_up_similarity.map(|value| format!("{:.4}", value)),
            margin = if margin.is_finite() {
                format!("{:.4}", margin)
            } else {
                "inf".into()
            },
            threshold = format!("{:.4}", threshold),
            min_margin = format!("{:.4}", min_margin),
            "no voice profile matched"
        );
    }

    matched
}

/// Save per-speaker embeddings as a sidecar file next to the meeting markdown.
/// Path: ~/meetings/.2026-03-25-standup.embeddings (hidden file, same dir)
pub fn save_meeting_embeddings(
    meeting_path: &std::path::Path,
    embeddings: &HashMap<String, Vec<f32>>,
) {
    let records: HashMap<String, MeetingEmbeddingRecord> = embeddings
        .iter()
        .map(|(label, embedding)| {
            (
                label.clone(),
                MeetingEmbeddingRecord {
                    embedding: embedding.clone(),
                    speech_secs: 0.0,
                    segment_count: 0,
                    model_version: String::new(),
                },
            )
        })
        .collect();
    save_meeting_embedding_records(meeting_path, &records);
}

pub fn save_meeting_embedding_records(
    meeting_path: &std::path::Path,
    records: &HashMap<String, MeetingEmbeddingRecord>,
) {
    if records.is_empty() {
        return;
    }
    let sidecar = meeting_embeddings_sidecar_path(meeting_path);
    let data = serde_json::to_vec(&MeetingEmbeddingsSidecar {
        version: 1,
        speakers: records.clone(),
    })
    .unwrap_or_default();
    write_meeting_embeddings_sidecar(&sidecar, &data, records.len());
}

fn write_meeting_embeddings_sidecar(sidecar: &Path, data: &[u8], speaker_count: usize) {
    if let Err(e) = std::fs::write(sidecar, data) {
        tracing::warn!(path = %sidecar.display(), error = %e, "failed to write meeting embeddings");
    } else {
        // Set 0600 permissions (embeddings are biometric-adjacent data)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o600)).ok();
        }
        tracing::debug!(path = %sidecar.display(), speakers = speaker_count, "meeting embeddings saved");
    }
}

pub fn load_meeting_embedding_records(
    meeting_path: &std::path::Path,
) -> Option<HashMap<String, MeetingEmbeddingRecord>> {
    let sidecar = meeting_embeddings_sidecar_path(meeting_path);
    let data = std::fs::read(&sidecar).ok()?;
    if let Ok(sidecar) = serde_json::from_slice::<MeetingEmbeddingsSidecar>(&data) {
        return Some(sidecar.speakers);
    }

    let legacy: HashMap<String, Vec<f32>> = serde_json::from_slice(&data).ok()?;
    Some(
        legacy
            .into_iter()
            .map(|(label, embedding)| {
                (
                    label,
                    MeetingEmbeddingRecord {
                        embedding,
                        speech_secs: 0.0,
                        segment_count: 0,
                        model_version: String::new(),
                    },
                )
            })
            .collect(),
    )
}

pub fn load_meeting_embeddings(
    meeting_path: &std::path::Path,
) -> Option<HashMap<String, Vec<f32>>> {
    load_meeting_embedding_records(meeting_path).map(|records| {
        records
            .into_iter()
            .map(|(label, record)| (label, record.embedding))
            .collect()
    })
}

#[allow(dead_code)]
fn save_legacy_meeting_embeddings(
    meeting_path: &std::path::Path,
    embeddings: &HashMap<String, Vec<f32>>,
) {
    if embeddings.is_empty() {
        return;
    }
    let sidecar = meeting_embeddings_sidecar_path(meeting_path);
    let data = serde_json::to_vec(embeddings).unwrap_or_default();
    if let Err(e) = std::fs::write(&sidecar, &data) {
        tracing::warn!(path = %sidecar.display(), error = %e, "failed to write meeting embeddings");
    } else {
        // Set 0600 permissions (embeddings are biometric-adjacent data)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)).ok();
        }
        tracing::debug!(path = %sidecar.display(), speakers = embeddings.len(), "meeting embeddings saved");
    }
}

pub fn meeting_embeddings_sidecar_path(meeting_path: &std::path::Path) -> std::path::PathBuf {
    let dir = meeting_path.parent().unwrap_or(std::path::Path::new("."));
    let stem = meeting_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    dir.join(format!(".{}.embeddings", stem.trim_end_matches(".md")))
}

pub fn load_self_profile(config: &Config) -> Option<VoiceProfileWithEmbedding> {
    if !config.voice.enabled {
        return None;
    }
    let name = config.identity.name.as_ref()?;
    let slug = profile_slug(name);
    let conn = open_db().ok()?;
    load_profile_with_embedding(&conn, &slug).ok().flatten()
}

pub fn profile_slug(text: &str) -> String {
    let slug: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let mut result = String::new();
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen && !result.is_empty() {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_db() -> (Connection, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let conn = open_db_at(tmp.path()).unwrap();
        (conn, tmp)
    }

    #[test]
    fn cosine_identical() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
    #[test]
    fn cosine_orthogonal() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }
    #[test]
    fn cosine_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn embedding_roundtrip() {
        let orig = vec![0.1, 0.2, -0.3, 1.0];
        assert_eq!(bytes_to_embedding(&embedding_to_bytes(&orig)), orig);
    }

    const TEST_MODEL_VERSION: &str = "test_model_v1";

    #[test]
    fn save_and_list() {
        let (conn, _tmp) = test_db();
        save_profile(
            &conn,
            "mat",
            "Mat",
            &vec![0.1f32; 512],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        let profiles = list_profiles(&conn).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].person_slug, "mat");
        assert_eq!(profiles[0].sample_count, 1);
    }

    #[test]
    fn upsert_increments_count() {
        let (conn, _tmp) = test_db();
        save_profile(
            &conn,
            "mat",
            "Mat",
            &[0.1f32; 4],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        save_profile(
            &conn,
            "mat",
            "Mat",
            &[0.2f32; 4],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        assert_eq!(list_profiles(&conn).unwrap()[0].sample_count, 2);
    }

    #[test]
    fn blended_averages() {
        let (conn, _tmp) = test_db();
        save_profile(
            &conn,
            "mat",
            "Mat",
            &[1.0f32; 4],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        save_profile_blended(
            &conn,
            "mat",
            "Mat",
            &[3.0f32; 4],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        let p = load_profile_with_embedding(&conn, "mat").unwrap().unwrap();
        assert!((p.embedding[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn delete_works() {
        let (conn, _tmp) = test_db();
        save_profile(
            &conn,
            "mat",
            "Mat",
            &[0.1f32; 4],
            "self-enrollment",
            TEST_MODEL_VERSION,
        )
        .unwrap();
        assert!(delete_profile(&conn, "mat").unwrap());
        assert!(list_profiles(&conn).unwrap().is_empty());
    }

    #[test]
    fn match_finds_best() {
        let profiles = vec![
            VoiceProfileWithEmbedding {
                person_slug: "mat".into(),
                name: "Mat".into(),
                embedding: vec![1.0, 0.0, 0.0],
                sample_count: 1,
            },
            VoiceProfileWithEmbedding {
                person_slug: "alex".into(),
                name: "Alex".into(),
                embedding: vec![0.0, 1.0, 0.0],
                sample_count: 1,
            },
        ];
        let mat = match_embedding(&[0.9, 0.1, 0.0], &profiles, 0.5).unwrap();
        assert_eq!(mat.name, "Mat");
        assert_eq!(mat.person_slug, "mat");
        let alex = match_embedding(&[0.0, 1.0, 0.0], &profiles, 0.5).unwrap();
        assert_eq!(alex.name, "Alex");
    }

    #[test]
    fn match_none_below_threshold() {
        let profiles = vec![VoiceProfileWithEmbedding {
            person_slug: "mat".into(),
            name: "Mat".into(),
            embedding: vec![1.0, 0.0],
            sample_count: 1,
        }];
        assert_eq!(match_embedding(&[0.0, 1.0], &profiles, 0.5), None);
    }

    #[test]
    fn match_none_when_margin_is_too_small() {
        let profiles = vec![
            VoiceProfileWithEmbedding {
                person_slug: "mat".into(),
                name: "Mat".into(),
                embedding: vec![1.0, 0.0],
                sample_count: 1,
            },
            VoiceProfileWithEmbedding {
                person_slug: "alex".into(),
                name: "Alex".into(),
                embedding: vec![0.98, 0.02],
                sample_count: 1,
            },
        ];
        assert!(match_embedding_with_margin(&[1.0, 0.0], &profiles, 0.5, 0.08).is_none());
    }

    #[test]
    fn match_returns_when_threshold_and_margin_pass() {
        let profiles = vec![
            VoiceProfileWithEmbedding {
                person_slug: "mat".into(),
                name: "Mat".into(),
                embedding: vec![1.0, 0.0],
                sample_count: 1,
            },
            VoiceProfileWithEmbedding {
                person_slug: "alex".into(),
                name: "Alex".into(),
                embedding: vec![0.0, 1.0],
                sample_count: 1,
            },
        ];
        let result = match_embedding_with_margin(&[0.95, 0.05], &profiles, 0.5, 0.08).unwrap();
        assert_eq!(result.name, "Mat");
        assert!(result.margin > 0.08);
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(profile_slug("Mat Silverstein"), "mat-silverstein");
    }

    #[test]
    fn meeting_embeddings_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let meeting = dir.path().join("2026-03-25-standup.md");
        std::fs::write(&meeting, "---\ntitle: test\n---\ntranscript").unwrap();

        let mut embeddings = std::collections::HashMap::new();
        embeddings.insert("SPEAKER_1".to_string(), vec![0.1f32, 0.2, 0.3]);
        embeddings.insert("SPEAKER_2".to_string(), vec![0.4f32, 0.5, 0.6]);

        save_meeting_embeddings(&meeting, &embeddings);

        let loaded = load_meeting_embeddings(&meeting).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["SPEAKER_1"], vec![0.1f32, 0.2, 0.3]);
        assert_eq!(loaded["SPEAKER_2"], vec![0.4f32, 0.5, 0.6]);

        let records = load_meeting_embedding_records(&meeting).unwrap();
        assert_eq!(records["SPEAKER_1"].speech_secs, 0.0);
        assert_eq!(records["SPEAKER_1"].segment_count, 0);
    }

    #[test]
    fn meeting_embedding_records_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let meeting = dir.path().join("2026-03-25-standup.md");
        std::fs::write(&meeting, "---\ntitle: test\n---\ntranscript").unwrap();

        let mut records = std::collections::HashMap::new();
        records.insert(
            "SPEAKER_1".to_string(),
            MeetingEmbeddingRecord {
                embedding: vec![0.1f32, 0.2, 0.3],
                speech_secs: 12.5,
                segment_count: 4,
                model_version: TEST_MODEL_VERSION.into(),
            },
        );

        save_meeting_embedding_records(&meeting, &records);

        let loaded = load_meeting_embedding_records(&meeting).unwrap();
        assert_eq!(loaded["SPEAKER_1"], records["SPEAKER_1"]);
    }

    #[test]
    fn legacy_meeting_embeddings_still_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let meeting = dir.path().join("2026-03-25-legacy.md");
        std::fs::write(&meeting, "---\ntitle: test\n---\ntranscript").unwrap();

        let mut embeddings = std::collections::HashMap::new();
        embeddings.insert("SPEAKER_1".to_string(), vec![0.1f32, 0.2, 0.3]);
        save_legacy_meeting_embeddings(&meeting, &embeddings);

        let loaded = load_meeting_embedding_records(&meeting).unwrap();
        assert_eq!(loaded["SPEAKER_1"].embedding, vec![0.1f32, 0.2, 0.3]);
        assert_eq!(loaded["SPEAKER_1"].speech_secs, 0.0);
    }

    #[test]
    fn meeting_embeddings_missing_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let meeting = dir.path().join("nonexistent.md");
        assert!(load_meeting_embeddings(&meeting).is_none());
    }

    #[test]
    fn sidecar_path_is_hidden_file() {
        let p = meeting_embeddings_sidecar_path(std::path::Path::new(
            "/tmp/meetings/2026-03-25-standup.md",
        ));
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            ".2026-03-25-standup.embeddings"
        );
    }
}
