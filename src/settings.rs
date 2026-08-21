use std::{
    env,
    fs,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppSettings {
    pub save_to_screenshots: bool,
}

impl AppSettings {
    pub fn load() -> Self {
        let Ok(contents) = fs::read_to_string(settings_path()) else {
            return Self::default();
        };

        Self {
            save_to_screenshots: contents
                .lines()
                .find_map(|line| line.trim().strip_prefix("save_to_screenshots="))
                .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        }
    }

    pub fn store(self) -> io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            format!("save_to_screenshots={}\n", self.save_to_screenshots),
        )
    }
}

fn settings_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(Path::new("Snapbar"))
        .join("settings.conf")
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_does_not_write_screenshots() {
        assert!(!super::AppSettings::default().save_to_screenshots);
    }
}
