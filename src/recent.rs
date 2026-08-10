use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use gtk::glib;

use crate::json::{array as json_array, escape_string, parse_strings};

const FORMAT_VERSION: u32 = 1;
const MAX_RECENT_FILES: usize = 10;

pub struct RecentStore {
    path: PathBuf,
}

impl RecentStore {
    pub fn new() -> Self {
        Self {
            path: glib::user_data_dir()
                .join("transcription-mvp")
                .join("recent.json"),
        }
    }

    pub fn load(&self) -> Result<Vec<PathBuf>> {
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read {}", self.path.display()));
            }
        };
        parse_recent(&json).with_context(|| format!("invalid {}", self.path.display()))
    }

    pub fn save(&self, files: &[PathBuf]) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            bail!("recent file has no parent directory");
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serialize_recent(files))
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        Ok(())
    }
}

pub fn record(files: &mut Vec<PathBuf>, path: &Path) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    files.retain(|existing| existing != &path);
    files.insert(0, path);
    files.truncate(MAX_RECENT_FILES);
}

fn serialize_recent(files: &[PathBuf]) -> String {
    let values = files
        .iter()
        .map(|path| format!("\"{}\"", escape_string(&path.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(",\n    ");
    format!("{{\n  \"version\": {FORMAT_VERSION},\n  \"files\": [\n    {values}\n  ]\n}}\n")
}

fn parse_recent(json: &str) -> Result<Vec<PathBuf>> {
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
        bail!("unsupported recent-file version {version}");
    }

    let mut files = parse_strings(json_array(json, "files")?)?
        .into_iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    files.truncate(MAX_RECENT_FILES);
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{parse_recent, record, serialize_recent};

    #[test]
    fn recent_json_round_trips() {
        let files = [
            PathBuf::from("/music/a].flac"),
            PathBuf::from("/music/b.mp3"),
        ];
        assert_eq!(parse_recent(&serialize_recent(&files)).unwrap(), files);
    }

    #[test]
    fn recording_moves_an_existing_file_to_the_front() {
        let mut files = vec![PathBuf::from("b"), PathBuf::from("a")];
        record(&mut files, std::path::Path::new("a"));
        assert_eq!(files, [PathBuf::from("a"), PathBuf::from("b")]);
    }
}
