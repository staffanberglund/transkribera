use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use gtk::glib;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preferences {
    pub prompt_for_marker_name: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            prompt_for_marker_name: true,
        }
    }
}

pub struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    pub fn new() -> Self {
        Self {
            path: glib::user_data_dir()
                .join("transkribera")
                .join("settings.json"),
        }
    }

    pub fn load(&self) -> Result<Preferences> {
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Preferences::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not read {}", self.path.display()));
            }
        };
        parse_preferences(&json)
            .with_context(|| format!("invalid settings file {}", self.path.display()))
    }

    pub fn save(&self, preferences: Preferences) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            bail!("settings file has no parent directory");
        };
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, serialize_preferences(preferences))
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        Ok(())
    }
}

fn serialize_preferences(preferences: Preferences) -> String {
    format!(
        "{{\n  \"version\": {FORMAT_VERSION},\n  \"prompt_for_marker_name\": {}\n}}\n",
        preferences.prompt_for_marker_name
    )
}

fn parse_preferences(json: &str) -> Result<Preferences> {
    let version = value_after_key(json, "version")?
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .filter(|version| !version.is_empty())
        .context("version is not an integer")?
        .parse::<u32>()
        .context("version is not an integer")?;
    if version != FORMAT_VERSION {
        bail!("unsupported settings version {version}");
    }

    let prompt_for_marker_name = match value_after_key(json, "prompt_for_marker_name")?
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default()
    {
        "true" => true,
        "false" => false,
        _ => bail!("prompt_for_marker_name is not a boolean"),
    };
    Ok(Preferences {
        prompt_for_marker_name,
    })
}

fn value_after_key<'a>(json: &'a str, key: &str) -> Result<&'a str> {
    let quoted_key = format!("\"{key}\"");
    json.split_once(&quoted_key)
        .and_then(|(_, rest)| rest.split_once(':'))
        .map(|(_, value)| value.trim_start())
        .with_context(|| format!("missing {key}"))
}

#[cfg(test)]
mod tests {
    use super::{Preferences, parse_preferences, serialize_preferences};

    #[test]
    fn preferences_json_round_trips() {
        for prompt_for_marker_name in [false, true] {
            let preferences = Preferences {
                prompt_for_marker_name,
            };
            assert_eq!(
                parse_preferences(&serialize_preferences(preferences)).unwrap(),
                preferences
            );
        }
    }
}
