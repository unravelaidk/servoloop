//! Explicit atomic settings commits with stale-edit and symlink refusal.
use crate::config::Config;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(super) struct Settings {
    path: PathBuf,
    original: Option<Vec<u8>>,
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err("Config must be a regular file, not a symlink.".into())
        }
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|e| format!("Config read: {e}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Config read: {error}")),
    }
}

impl Settings {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let original = read(&path)?;
        Ok(Self { path, original })
    }

    pub fn save(&mut self, config: &Config) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| format!("Config directory: {e}"))?;
        let lock = self.path.with_extension("json.lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|e| format!("Config is locked or cannot be locked: {e}"))?;
        struct Lock(PathBuf, Option<fs::File>);
        impl Drop for Lock {
            fn drop(&mut self) {
                drop(self.1.take());
                let _ = fs::remove_file(&self.0);
            }
        }
        let _lock = Lock(lock, Some(file));
        if read(&self.path)? != self.original {
            return Err("Configuration changed on disk. Reopen ServoLoop before saving; no changes written.".into());
        }
        let mut document = match &self.original {
            Some(bytes) => serde_json::from_slice::<serde_json::Value>(bytes)
                .map_err(|e| format!("Config JSON: {e}"))?,
            None => serde_json::json!({"version":1}),
        };
        let object = document
            .as_object_mut()
            .ok_or("Config must be a JSON object.")?;
        if object.get("version") != Some(&serde_json::json!(1)) {
            return Err("Unsupported configuration version.".into());
        }
        for (name, value) in [
            ("provider", &config.provider),
            ("model", &config.model),
            ("base_url", &config.base_url),
        ] {
            if let Some(value) = value {
                object.insert(name.into(), value.clone().into());
            } else {
                object.remove(name);
            }
        }
        let mut bytes = serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| format!("Config temporary file: {e}"))?;
        tmp.write_all(&bytes)
            .and_then(|_| tmp.as_file().sync_all())
            .map_err(|e| format!("Config write: {e}"))?;
        // Check again after staging, before committing the replacement.
        if read(&self.path)? != self.original {
            return Err("Configuration changed during save. No replacement made.".into());
        }
        tmp.persist(&self.path)
            .map_err(|e| format!("Config commit: {}", e.error))?;
        self.original = Some(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_preserves_other_fields_and_refuses_stale_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(
            &path,
            r#"{"version":1,"store":"keep","future":{"enabled":true}}"#,
        )
        .unwrap();
        let mut settings = Settings::open(path.clone()).unwrap();
        let mut stale = Settings::open(path.clone()).unwrap();
        let cfg = Config {
            provider: Some("ollama".into()),
            model: Some("test".into()),
            ..Config::default()
        };
        settings.save(&cfg).unwrap();
        let saved = fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(value["store"], "keep");
        assert_eq!(value["future"]["enabled"], true);
        assert!(stale.save(&cfg).is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        assert!(!path.with_extension("json.lock").exists());
    }
    #[test]
    fn opening_settings_has_no_side_effects_and_lock_is_not_stolen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut settings = Settings::open(path.clone()).unwrap();
        assert!(!path.exists());
        fs::write(path.with_extension("json.lock"), "owner").unwrap();
        assert!(settings.save(&Config::default()).is_err());
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(path.with_extension("json.lock")).unwrap(),
            "owner"
        );
    }
}
