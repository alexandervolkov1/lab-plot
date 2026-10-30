use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct ScriptFile {
    pub text: String,
    pub path: Option<PathBuf>,
    saved_text: String,
}

impl ScriptFile {
    pub fn dirty(&self) -> bool {
        self.text != self.saved_text
    }

    pub fn open(&mut self, path: &Path) -> io::Result<()> {
        let path = path.canonicalize()?;
        let text = std::fs::read_to_string(&path)?;
        self.saved_text.clone_from(&text);
        self.text = text;
        self.path = Some(path);
        Ok(())
    }

    pub fn save_as(&mut self, path: &Path) -> io::Result<()> {
        let path = std::path::absolute(path)?;
        // Replace only after the complete UTF-8 buffer has been written successfully.
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Expected a file path"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(self.text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|error| error.error)?;
        self.saved_text.clone_from(&self.text);
        self.path = Some(path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_reopen_unicode_and_track_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("эксперимент.ss");
        let mut script = ScriptFile::default();
        assert!(!script.dirty());
        script.text = "(define имя \"температура 🌡\")\n".into();
        assert!(script.dirty());
        script.save_as(&path).unwrap();
        assert!(!script.dirty());
        let mut reopened = ScriptFile::default();
        reopened.open(&path).unwrap();
        assert_eq!(reopened.text, script.text);
        script.text.push_str("; правка");
        assert!(script.dirty());
        script.text = reopened.text;
        assert!(!script.dirty());
        script.text = "новое содержимое".into();
        script.save_as(&path).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), script.text);
    }

    #[test]
    fn io_errors_preserve_buffer_path_and_dirty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.scm");
        let mut script = ScriptFile {
            text: "saved".into(),
            ..Default::default()
        };
        script.save_as(&path).unwrap();
        script.text = "unsaved".into();
        let original_path = script.path.clone();
        assert!(script.open(&dir.path().join("missing.scm")).is_err());
        let invalid = dir.path().join("invalid.scm");
        std::fs::write(&invalid, [0xff]).unwrap();
        assert!(script.open(&invalid).is_err());
        assert!(
            script
                .save_as(&dir.path().join("missing/file.scm"))
                .is_err()
        );
        assert!(script.save_as(dir.path()).is_err());
        assert_eq!(script.text, "unsaved");
        assert_eq!(script.path, original_path);
        assert!(script.dirty());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "saved");
    }
}
