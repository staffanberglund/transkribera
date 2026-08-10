use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use gtk::glib;

use crate::json::{array as json_array, escape_string as escape_json_string, parse_strings};

const FORMAT_VERSION: u32 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Marker {
    pub position_ns: u64,
    pub name: String,
}

pub struct MarkerStore {
    path: PathBuf,
    audio_path: String,
}

impl MarkerStore {
    pub fn for_audio(audio_path: &std::path::Path) -> Self {
        let identity = audio_path
            .canonicalize()
            .unwrap_or_else(|_| audio_path.to_path_buf());
        let audio_path = identity.to_string_lossy().into_owned();
        let hash = stable_hash(audio_path.as_bytes());
        let path = glib::user_data_dir()
            .join("transcription-mvp")
            .join("markers")
            .join(format!("{hash:016x}.json"));
        Self { path, audio_path }
    }

    pub fn load(&self) -> Result<Vec<Marker>> {
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not read marker file {}", self.path.display())
                });
            }
        };
        parse_markers(&json).with_context(|| format!("invalid marker file {}", self.path.display()))
    }

    pub fn save(&self, markers: &[Marker]) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            bail!("marker file has no parent directory");
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create marker directory {}", parent.display()))?;

        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serialize_markers(&self.audio_path, markers))
            .with_context(|| format!("could not write marker file {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace marker file {}", self.path.display()))?;
        Ok(())
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn serialize_markers(audio_path: &str, markers: &[Marker]) -> String {
    let positions = markers
        .iter()
        .map(|marker| marker.position_ns.to_string())
        .collect::<Vec<_>>()
        .join(",\n    ");
    let names = markers
        .iter()
        .map(|marker| format!("\"{}\"", escape_json_string(&marker.name)))
        .collect::<Vec<_>>()
        .join(",\n    ");
    format!(
        "{{\n  \"version\": {FORMAT_VERSION},\n  \"audio_path\": \"{}\",\n  \"markers_ns\": [\n    {positions}\n  ],\n  \"marker_names\": [\n    {names}\n  ]\n}}\n",
        escape_json_string(audio_path)
    )
}

fn parse_markers(json: &str) -> Result<Vec<Marker>> {
    let version = json
        .split_once("\"version\"")
        .and_then(|(_, rest)| rest.split_once(':'))
        .and_then(|(_, rest)| {
            rest.trim_start()
                .split(|character: char| !character.is_ascii_digit())
                .next()
        })
        .filter(|version| !version.is_empty())
        .context("missing version")?
        .parse::<u32>()
        .context("version is not an integer")?;
    if version != FORMAT_VERSION {
        bail!("unsupported marker format version {version}");
    }

    let positions = json_array(json, "markers_ns")?
        .trim()
        .split(',')
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .with_context(|| format!("invalid marker position {value:?}"))
        })
        .collect::<Result<Vec<_>>>()?;

    let names = parse_strings(json_array(json, "marker_names")?)?;
    if positions.len() != names.len() {
        bail!("marker position and name counts differ");
    }

    let mut markers = positions
        .into_iter()
        .zip(names)
        .map(|(position_ns, name)| Marker { position_ns, name })
        .collect::<Vec<_>>();
    markers.sort_by_key(|marker| marker.position_ns);
    markers.dedup_by_key(|marker| marker.position_ns);
    Ok(markers)
}

#[cfg(test)]
mod tests {
    use super::{Marker, parse_markers, serialize_markers, stable_hash};

    #[test]
    fn marker_json_round_trips() {
        let markers = [
            Marker {
                position_ns: 1_250_000_000,
                name: "Intro".into(),
            },
            Marker {
                position_ns: 9_000_000_000,
                name: "Speaker's ] \"point\"\ncontinued".into(),
            },
        ];
        assert_eq!(
            parse_markers(&serialize_markers("/music/example.flac", &markers)).unwrap(),
            markers
        );
    }

    #[test]
    fn empty_marker_json_round_trips() {
        assert!(
            parse_markers(&serialize_markers("/music/example.flac", &[]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn audio_identity_hash_is_stable() {
        assert_eq!(stable_hash(b"/music/example.flac"), 0x4d94_8ada_958d_e7a8);
    }
}
