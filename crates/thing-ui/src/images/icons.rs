use std::{collections::BTreeMap, env, fs, io::Read, path::PathBuf};

#[derive(Default)]
pub(super) struct Icons {
    applications: Option<Applications>,
}

#[derive(Default)]
struct Applications {
    filenames: BTreeMap<String, String>,
    classes: BTreeMap<String, String>,
}

impl Applications {
    fn insert(&mut self, filename: Option<&str>, icon: String, class: Option<String>) {
        if let Some(filename) = filename {
            self.filenames
                .entry(filename.to_lowercase())
                .or_insert_with(|| icon.clone());
        }
        if let Some(class) = class {
            self.classes.entry(class.to_lowercase()).or_insert(icon);
        }
    }

    fn find(&self, app_id: &str) -> Option<&str> {
        let app_id = app_id.to_lowercase();
        self.filenames
            .get(&app_id)
            .or_else(|| self.classes.get(&app_id))
            .map(String::as_str)
    }
}

impl Icons {
    pub fn find(&mut self, image: &thing_core::Image, size: u16) -> Result<PathBuf, String> {
        if let Some(path) = &image.source {
            return Ok(PathBuf::from(path));
        }
        let name = if let Some(app) = &image.app_id {
            let applications = self.applications.get_or_insert_with(index_applications);
            applications.find(app).unwrap_or(app)
        } else if let Some(icon) = &image.icon {
            icon
        } else {
            return Err("no image source".into());
        };
        if std::path::Path::new(name).is_absolute() {
            return Ok(PathBuf::from(name));
        }
        for candidate in [
            name.to_owned(),
            name.to_lowercase(),
            "application-x-executable".into(),
        ] {
            if let Some(path) = freedesktop_icons::lookup(&candidate)
                .with_theme(&image.theme)
                .with_size(size)
                .find()
            {
                return Ok(path);
            }
        }
        Err(format!(
            "icon {name:?} was not found in theme {:?}",
            image.theme
        ))
    }
}

fn index_applications() -> Applications {
    let mut roots = Vec::new();
    if let Some(home) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
    {
        roots.push(home);
    }
    roots.extend(
        env::split_paths(
            &env::var_os("XDG_DATA_DIRS")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
        )
        .filter(|p| p.is_absolute()),
    );
    let mut entries = Applications::default();
    let mut examined = 0;
    for root in roots {
        let base = root.join("applications");
        let mut directories = vec![base.clone()];
        while let Some(directory) = directories.pop() {
            let Ok(children) = fs::read_dir(directory) else {
                continue;
            };
            let mut children: Vec<_> = children.flatten().take(10000).collect();
            children.sort_by_key(|entry| entry.file_name());
            for entry in children {
                examined += 1;
                if examined > 10000 {
                    return entries;
                }
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    if path.components().count() < base.components().count() + 8 {
                        directories.push(path);
                    }
                    continue;
                }
                if path
                    .extension()
                    .is_none_or(|extension| extension != "desktop")
                {
                    continue;
                }
                let Ok(file) = fs::File::open(&path) else {
                    continue;
                };
                let mut text = String::new();
                if file.take(65537).read_to_string(&mut text).is_err() || text.len() > 65536 {
                    continue;
                }
                let (icon, class) = desktop_icon(&text);
                let Some(icon) = icon else {
                    continue;
                };
                entries.insert(path.file_stem().and_then(|s| s.to_str()), icon, class);
            }
        }
    }
    entries
}

fn desktop_icon(text: &str) -> (Option<String>, Option<String>) {
    let (mut icon, mut class, mut main) = (None, None, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            main = line == "[Desktop Entry]";
        }
        if !main {
            continue;
        }
        if let Some(value) = line.strip_prefix("Icon=") {
            icon = Some(value.trim().to_owned());
        }
        if let Some(value) = line.strip_prefix("StartupWMClass=") {
            class = Some(value.trim().to_owned());
        }
    }
    (
        icon.filter(|s| !s.is_empty()),
        class.filter(|s| !s.is_empty()),
    )
}

#[cfg(test)]
mod tests {
    use super::Applications;

    #[test]
    fn desktop_action_icons_do_not_override_the_application_icon() {
        assert_eq!(
            super::desktop_icon(
                "[Desktop Entry]\nIcon=terminal\nStartupWMClass=Term\n[Desktop Action New]\nIcon=new\n"
            ),
            (Some("terminal".into()), Some("Term".into()))
        );
    }

    #[test]
    fn desktop_filenames_take_priority_over_class_aliases() {
        let mut applications = Applications::default();
        applications.insert(Some("a"), "wrong-icon".into(), Some("target".into()));
        applications.insert(Some("target"), "right-icon".into(), None);
        assert_eq!(applications.find("TARGET"), Some("right-icon"));

        applications.insert(Some("target"), "lower-priority-icon".into(), None);
        assert_eq!(applications.find("target"), Some("right-icon"));
    }
}
