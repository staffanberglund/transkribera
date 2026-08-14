use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use gtk::glib;

use crate::json;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopRegion {
    pub name: String,
    pub start_ns: u64,
    pub end_ns: u64,
}

impl LoopRegion {
    pub fn new(name: String, first_ns: u64, second_ns: u64) -> Option<Self> {
        // Dragging right-to-left creates the same normalized A–B region.
        let (start_ns, end_ns) = if first_ns <= second_ns {
            (first_ns, second_ns)
        } else {
            (second_ns, first_ns)
        };
        (start_ns < end_ns).then_some(Self {
            name,
            start_ns,
            end_ns,
        })
    }
}

pub struct LoopStore {
    path: PathBuf,
    audio_path: String,
}

impl LoopStore {
    pub fn for_audio(audio_path: &std::path::Path) -> Self {
        // The canonical path gives each audio file a stable sidecar filename.
        let identity = audio_path
            .canonicalize()
            .unwrap_or_else(|_| audio_path.to_path_buf());
        let audio_path = identity.to_string_lossy().into_owned();
        let hash = stable_hash(audio_path.as_bytes());
        let path = glib::user_data_dir()
            .join("transkribera")
            .join("loops")
            .join(format!("{hash:016x}.json"));
        Self { path, audio_path }
    }

    pub fn load(&self) -> Result<Vec<LoopRegion>> {
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read loop file {}", self.path.display()));
            }
        };
        parse_loops(&json).with_context(|| format!("invalid loop file {}", self.path.display()))
    }

    pub fn save(&self, loops: &[LoopRegion]) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            bail!("loop file has no parent directory");
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create loop directory {}", parent.display()))?;
        // Replace through a temporary file so an interrupted write keeps the old data.
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serialize_loops(&self.audio_path, loops))
            .with_context(|| format!("could not write loop file {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace loop file {}", self.path.display()))?;
        Ok(())
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    // FNV-1a is deterministic across runs, unlike Rust's default map hasher.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn serialize_loops(audio_path: &str, loops: &[LoopRegion]) -> String {
    let regions = loops
        .iter()
        .map(|region| {
            format!(
                "    {{\"name\": \"{}\", \"start_ns\": {}, \"end_ns\": {}}}",
                json::escape_string(&region.name),
                region.start_ns,
                region.end_ns
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{\n  \"version\": {FORMAT_VERSION},\n  \"audio_path\": \"{}\",\n  \"loops\": [\n{regions}\n  ]\n}}\n",
        json::escape_string(audio_path)
    )
}

fn parse_loops(json_text: &str) -> Result<Vec<LoopRegion>> {
    let version = json::unsigned_integer(json_text, "version")? as u32;
    if version != FORMAT_VERSION {
        bail!("unsupported loop format version {version}");
    }
    let mut loops = json::object_array(json_text, "loops")?
        .into_iter()
        .map(|region| {
            let name = json::string(region, "name")?;
            let start_ns = json::unsigned_integer(region, "start_ns")?;
            let end_ns = json::unsigned_integer(region, "end_ns")?;
            LoopRegion::new(name, start_ns, end_ns).context("loop endpoints must differ")
        })
        .collect::<Result<Vec<_>>>()?;
    loops.sort_by_key(|region| (region.start_ns, region.end_ns));
    Ok(loops)
}

#[cfg(test)]
mod tests {
    use super::{LoopRegion, parse_loops, serialize_loops, stable_hash};

    #[test]
    fn loop_json_round_trips() {
        let loops = [
            LoopRegion::new("Verse".into(), 1_000_000_000, 5_500_000_000).unwrap(),
            LoopRegion::new("Quote \"A\"".into(), 9_000_000_000, 12_000_000_000).unwrap(),
        ];
        assert_eq!(
            parse_loops(&serialize_loops("/music/example.flac", &loops)).unwrap(),
            loops
        );
    }

    #[test]
    fn reversed_endpoints_are_normalized() {
        let region = LoopRegion::new("Loop".into(), 20, 10).unwrap();
        assert_eq!((region.start_ns, region.end_ns), (10, 20));
        assert!(LoopRegion::new("Empty".into(), 10, 10).is_none());
    }

    #[test]
    fn audio_identity_hash_matches_marker_storage() {
        assert_eq!(stable_hash(b"/music/example.flac"), 0x4d94_8ada_958d_e7a8);
    }
}
