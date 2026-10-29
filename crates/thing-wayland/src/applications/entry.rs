//! desktop entry parsing and application directory scanning, following the
//! freedesktop desktop entry specification.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use thing_runtime::{
    DesktopAction, DesktopApplication, DesktopArguments, MAX_LAUNCH_ARGUMENT_BYTES,
    MAX_LAUNCH_ARGUMENTS, MAX_LAUNCH_BYTES,
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_LIST_ITEMS: usize = 64;
const MAX_ACTIONS: usize = 32;
pub(super) const MAX_APPLICATIONS: usize = 4096;
const MAX_DEPTH: usize = 8;

/// process context that decides which entries apply and how they are named.
#[derive(Clone, Debug, Default)]
pub(super) struct Environment {
    /// `applications` directories in precedence order, highest first.
    pub directories: Vec<PathBuf>,
    /// locale names to try, most specific first, such as `de_DE` then `de`.
    pub locales: Vec<String>,
    /// lowercase names from `XDG_CURRENT_DESKTOP`.
    pub desktops: Vec<String>,
    pub path: Vec<PathBuf>,
}

impl Environment {
    pub fn from_process() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.map(|home| home.join(".local/share")));
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut directories = Vec::new();
        for directory in data_home.into_iter().chain(
            data_dirs
                .split(':')
                .map(PathBuf::from)
                .filter(|path| path.is_absolute()),
        ) {
            let applications = directory.join("applications");
            if !directories.contains(&applications) {
                directories.push(applications);
            }
        }
        let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            .unwrap_or_default();
        Self {
            directories,
            locales: locale_candidates(&locale),
            desktops: std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .split(':')
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
                .collect(),
            path: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
        }
    }
}

/// returns the locale keys to try for `lang_COUNTRY.ENCODING@MODIFIER`, in the
/// order the specification gives.
pub(super) fn locale_candidates(locale: &str) -> Vec<String> {
    if locale.is_empty() || locale == "C" || locale == "POSIX" {
        return Vec::new();
    }
    let (rest, modifier) = match locale.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier)),
        None => (locale, None),
    };
    let rest = rest.split_once('.').map_or(rest, |(rest, _)| rest);
    let (language, country) = match rest.split_once('_') {
        Some((language, country)) => (language, Some(country)),
        None => (rest, None),
    };
    let mut candidates = Vec::new();
    if let (Some(country), Some(modifier)) = (country, modifier) {
        candidates.push(format!("{language}_{country}@{modifier}"));
    }
    if let Some(country) = country {
        candidates.push(format!("{language}_{country}"));
    }
    if let Some(modifier) = modifier {
        candidates.push(format!("{language}@{modifier}"));
    }
    candidates.push(language.to_owned());
    candidates
}

/// reads every application directory. the first file with a given desktop file
/// id wins, including a hidden one, which removes lower-precedence copies.
pub(super) fn scan(environment: &Environment) -> Vec<Entry> {
    let mut seen = BTreeSet::new();
    let mut applications = Vec::new();
    for directory in &environment.directories {
        let mut files = Vec::new();
        collect(directory, directory, 0, &mut files);
        files.sort();
        for (id, path) in files {
            if seen.len() >= MAX_APPLICATIONS || !seen.insert(id.clone()) {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse(&id, &path, &source, environment) {
                applications.push(entry);
            }
        }
    }
    applications.sort_by(|left, right| left.application.id.cmp(&right.application.id));
    applications
}

fn collect(root: &Path, directory: &Path, depth: usize, files: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_DEPTH || files.len() >= MAX_APPLICATIONS {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_APPLICATIONS {
            break;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // follow links to files, as packaging often does, but not to
        // directories, which could form cycles.
        if kind.is_dir() {
            collect(root, &path, depth + 1, files);
        } else if path.extension() == Some(OsStr::new("desktop"))
            && let Ok(relative) = path.strip_prefix(root)
            && let Some(relative) = relative.to_str()
        {
            files.push((relative.replace('/', "-"), path));
        }
    }
}

struct Groups {
    groups: BTreeMap<String, BTreeMap<String, String>>,
}

impl Groups {
    fn parse(source: &str) -> Option<Self> {
        let mut groups = BTreeMap::<String, BTreeMap<String, String>>::new();
        let mut current = None;
        for line in source.lines() {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line
                .trim_end()
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                // the main group must come first.
                if current.is_none() && name != "Desktop Entry" {
                    return None;
                }
                if groups.contains_key(name) {
                    return None;
                }
                groups.insert(name.to_owned(), BTreeMap::new());
                current = Some(name.to_owned());
                continue;
            }
            let group = groups.get_mut(current.as_deref()?)?;
            let (key, value) = line.split_once('=')?;
            // keep the first value for a repeated key.
            group
                .entry(key.trim_end().to_owned())
                .or_insert_with(|| value.trim_start().to_owned());
        }
        groups
            .contains_key("Desktop Entry")
            .then_some(Self { groups })
    }

    fn group(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        self.groups.get(name)
    }
}

fn string(group: &BTreeMap<String, String>, key: &str) -> Option<String> {
    group.get(key).map(|value| unescape(value))
}

fn localized(group: &BTreeMap<String, String>, key: &str, locales: &[String]) -> Option<String> {
    locales
        .iter()
        .find_map(|locale| group.get(&format!("{key}[{locale}]")))
        .or_else(|| group.get(key))
        .map(|value| unescape(value))
}

fn boolean(group: &BTreeMap<String, String>, key: &str) -> bool {
    group.get(key).is_some_and(|value| value.trim() == "true")
}

fn list(group: &BTreeMap<String, String>, key: &str) -> Vec<String> {
    let Some(value) = group.get(key) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut item = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(';') => item.push(';'),
                Some(other) => {
                    item.push('\\');
                    item.push(other);
                }
                None => item.push('\\'),
            },
            ';' => items.push(unescape(&std::mem::take(&mut item))),
            other => item.push(other),
        }
    }
    if !item.is_empty() {
        items.push(unescape(&item));
    }
    items.retain(|item| !item.is_empty());
    items
}

fn localized_list(group: &BTreeMap<String, String>, key: &str, locales: &[String]) -> Vec<String> {
    locales
        .iter()
        .map(|locale| format!("{key}[{locale}]"))
        .find(|key| group.contains_key(key))
        .map_or_else(|| list(group, key), |key| list(group, &key))
}

/// decodes the general string escapes, which apply before `Exec` quoting.
fn unescape(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            result.push(character);
            continue;
        }
        match characters.next() {
            Some('s') => result.push(' '),
            Some('n') => result.push('\n'),
            Some('t') => result.push('\t'),
            Some('r') => result.push('\r'),
            Some('\\') => result.push('\\'),
            Some(other) => {
                result.push('\\');
                result.push(other);
            }
            None => result.push('\\'),
        }
    }
    result
}

/// splits an unescaped `Exec` value into arguments. inside double quotes, a
/// backslash escapes `"`, `` ` ``, `$`, and `\`.
fn split_exec(value: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    let mut argument = String::new();
    let mut started = false;
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' | '\n' => {
                if started {
                    arguments.push(std::mem::take(&mut argument));
                    started = false;
                }
            }
            '"' => {
                started = true;
                loop {
                    match characters.next()? {
                        '"' => break,
                        '\\' => match characters.next()? {
                            escaped @ ('"' | '`' | '$' | '\\') => argument.push(escaped),
                            other => {
                                argument.push('\\');
                                argument.push(other);
                            }
                        },
                        other => argument.push(other),
                    }
                }
            }
            other => {
                started = true;
                argument.push(other);
            }
        }
    }
    if started {
        arguments.push(argument);
    }
    Some(arguments)
}

/// the files or urls substituted for one launch.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Files<'a> {
    None,
    /// the value for `%f` or `%u`.
    One(&'a str),
    /// the values for `%F` or `%U`.
    All(&'a [String]),
}

/// an `Exec` command with its quoting undone and its field codes kept, so each
/// launch can substitute its own files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    arguments: Vec<String>,
    name: String,
    icon: String,
    location: Option<String>,
    accepts: DesktopArguments,
}

impl Template {
    fn new(arguments: Vec<String>, name: &str, icon: &str, path: &Path) -> Option<Self> {
        let mut codes = Vec::new();
        for argument in &arguments {
            match argument.as_str() {
                "%F" => codes.push(DesktopArguments::Files),
                "%U" => codes.push(DesktopArguments::Urls),
                "%i" => {}
                _ => {
                    let mut characters = argument.chars();
                    while let Some(character) = characters.next() {
                        if character != '%' {
                            continue;
                        }
                        match characters.next() {
                            Some('f') => codes.push(DesktopArguments::File),
                            Some('u') => codes.push(DesktopArguments::Url),
                            Some('%' | 'c' | 'k' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {}
                            // multi-argument codes must occupy the whole argument.
                            Some('F' | 'U' | 'i') => return None,
                            // the specification calls any other code invalid.
                            _ => return None,
                        }
                    }
                }
            }
        }
        // a command line may hold at most one file or url code. with more, the
        // entry still launches without files but accepts none.
        let accepts = match codes.as_slice() {
            [code] => *code,
            _ => DesktopArguments::None,
        };
        Some(Self {
            arguments,
            name: name.to_owned(),
            icon: icon.to_owned(),
            location: path.to_str().map(str::to_owned),
            accepts,
        })
    }

    pub(crate) fn accepts(&self) -> DesktopArguments {
        self.accepts
    }

    /// expands the field codes and checks the result against the launch limits.
    pub(crate) fn expand(&self, files: Files<'_>) -> Option<Vec<String>> {
        let mut expanded = Vec::with_capacity(self.arguments.len());
        for argument in &self.arguments {
            match argument.as_str() {
                "%F" | "%U" => {
                    if let Files::All(values) = files {
                        expanded.extend(values.iter().cloned());
                    }
                    continue;
                }
                "%f" | "%u" => {
                    if let Files::One(value) = files {
                        expanded.push(value.to_owned());
                    }
                    continue;
                }
                "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
                "%i" => {
                    if !self.icon.is_empty() {
                        expanded.push("--icon".to_owned());
                        expanded.push(self.icon.clone());
                    }
                    continue;
                }
                _ => {}
            }
            let mut result = String::with_capacity(argument.len());
            let mut characters = argument.chars();
            while let Some(character) = characters.next() {
                if character != '%' {
                    result.push(character);
                    continue;
                }
                match characters.next() {
                    Some('%') => result.push('%'),
                    Some('c') => result.push_str(&self.name),
                    Some('k') => result.push_str(self.location.as_deref()?),
                    Some('f' | 'u') => {
                        if let Files::One(value) = files {
                            result.push_str(value);
                        }
                    }
                    _ => {}
                }
            }
            expanded.push(result);
        }
        valid_command(&expanded).then_some(expanded)
    }
}

/// whether an argument array fits the limits `services.launch` enforces.
pub(crate) fn valid_command(command: &[String]) -> bool {
    !command.is_empty()
        && command.len() <= MAX_LAUNCH_ARGUMENTS
        && command
            .iter()
            .all(|argument| argument.len() <= MAX_LAUNCH_ARGUMENT_BYTES && !argument.contains('\0'))
        && command.iter().map(String::len).sum::<usize>() <= MAX_LAUNCH_BYTES
        && !command[0].is_empty()
}

/// returns the launch template and the command for a launch without files.
/// an entry without `Exec` yields neither; an invalid `Exec` yields `None`.
fn exec_command(
    group: &BTreeMap<String, String>,
    name: &str,
    icon: &str,
    path: &Path,
) -> Option<Option<(Template, Vec<String>)>> {
    let Some(exec) = string(group, "Exec") else {
        return Some(None);
    };
    let template = Template::new(split_exec(&exec)?, name, icon, path)?;
    let command = template.expand(Files::None)?;
    Some(Some((template, command)))
}

/// one parsed entry: the record configurations see, and the templates used to
/// launch it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub application: DesktopApplication,
    pub exec: Option<Template>,
    pub actions: BTreeMap<String, Option<Template>>,
}

fn executable(program: &str, environment: &Environment) -> bool {
    let is_executable = |path: &Path| {
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    environment
        .path
        .iter()
        .any(|directory| is_executable(&directory.join(program)))
}

fn shown_here(group: &BTreeMap<String, String>, environment: &Environment) -> bool {
    let only = list(group, "OnlyShowIn");
    let excluded = list(group, "NotShowIn");
    for desktop in &environment.desktops {
        if only.iter().any(|name| name.eq_ignore_ascii_case(desktop)) {
            return true;
        }
        if excluded
            .iter()
            .any(|name| name.eq_ignore_ascii_case(desktop))
        {
            return false;
        }
    }
    !group.contains_key("OnlyShowIn")
}

fn bounded(text: String) -> Option<String> {
    (text.len() <= MAX_TEXT_BYTES && !text.contains('\0')).then_some(text)
}

fn bounded_list(mut items: Vec<String>) -> Vec<String> {
    items.retain(|item| item.len() <= MAX_TEXT_BYTES && !item.contains('\0'));
    items.truncate(MAX_LIST_ITEMS);
    items
}

/// parses one entry. it returns `None` for hidden, inapplicable, or invalid
/// entries, and for anything that is not an application. the record's
/// revision is zero until the catalog assigns one.
pub(super) fn parse(
    id: &str,
    path: &Path,
    source: &str,
    environment: &Environment,
) -> Option<Entry> {
    let groups = Groups::parse(source)?;
    let main = groups.group("Desktop Entry")?;
    if string(main, "Type").as_deref() != Some("Application")
        || boolean(main, "Hidden")
        || !shown_here(main, environment)
    {
        return None;
    }
    if let Some(program) = string(main, "TryExec")
        && !executable(&program, environment)
    {
        return None;
    }
    let locales = &environment.locales;
    let name = bounded(localized(main, "Name", locales)?).filter(|name| !name.is_empty())?;
    let icon = bounded(string(main, "Icon").unwrap_or_default())?;
    let dbus_activatable = boolean(main, "DBusActivatable");
    // an entry without `Exec` is kept with an empty command. such entries
    // usually exist only to give a window's app id a name and icon.
    let (exec, command) = exec_command(main, &name, &icon, path)?.unzip();
    let mut actions = Vec::new();
    let mut templates = BTreeMap::new();
    for key in list(main, "Actions").into_iter().take(MAX_ACTIONS) {
        let Some(group) = groups.group(&format!("Desktop Action {key}")) else {
            continue;
        };
        let Some(action_name) = localized(group, "Name", locales)
            .and_then(bounded)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let action_icon = string(group, "Icon").and_then(bounded).unwrap_or_default();
        let Some(action_exec) = exec_command(group, &name, &action_icon, path) else {
            continue;
        };
        let (action_template, action_command) = action_exec.unzip();
        if action_command.is_none() && !dbus_activatable {
            continue;
        }
        if bounded(key.clone()).is_some() && !templates.contains_key(&key) {
            templates.insert(key.clone(), action_template);
            actions.push(DesktopAction {
                key,
                name: action_name,
                icon: action_icon,
                command: action_command.unwrap_or_default(),
            });
        }
    }
    // d-bus activation opens any number of uris; otherwise `Exec` decides.
    let accepts = if dbus_activatable {
        DesktopArguments::Urls
    } else {
        exec.as_ref()
            .map_or(DesktopArguments::None, Template::accepts)
    };
    let application = DesktopApplication {
        id: bounded(id.to_owned())?,
        revision: 0,
        name,
        generic_name: localized(main, "GenericName", locales)
            .and_then(bounded)
            .unwrap_or_default(),
        comment: localized(main, "Comment", locales)
            .and_then(bounded)
            .unwrap_or_default(),
        icon,
        keywords: bounded_list(localized_list(main, "Keywords", locales)),
        categories: bounded_list(list(main, "Categories")),
        command: command.unwrap_or_default(),
        working_directory: string(main, "Path")
            .filter(|path| !path.is_empty())
            .and_then(bounded),
        terminal: boolean(main, "Terminal"),
        no_display: boolean(main, "NoDisplay"),
        startup_wm_class: string(main, "StartupWMClass")
            .and_then(bounded)
            .unwrap_or_default(),
        dbus_activatable,
        accepts,
        actions,
    };
    Some(Entry {
        application,
        exec,
        actions: templates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment() -> Environment {
        Environment {
            directories: Vec::new(),
            locales: locale_candidates("de_DE.UTF-8"),
            desktops: vec!["niri".into()],
            path: vec!["/usr/bin".into(), "/bin".into()],
        }
    }

    fn parse_one(source: &str) -> Option<DesktopApplication> {
        parse_entry(source).map(|entry| entry.application)
    }

    fn parse_entry(source: &str) -> Option<Entry> {
        parse(
            "org.example.App.desktop",
            Path::new("/usr/share/applications/org.example.App.desktop"),
            source,
            &environment(),
        )
    }

    #[test]
    fn locale_candidates_follow_the_specification_order() {
        assert_eq!(
            locale_candidates("sr_YU.UTF-8@Latn"),
            ["sr_YU@Latn", "sr_YU", "sr@Latn", "sr"]
        );
        assert_eq!(locale_candidates("de_DE.UTF-8"), ["de_DE", "de"]);
        assert!(locale_candidates("C").is_empty());
    }

    #[test]
    fn a_complete_entry_exposes_localized_fields_commands_and_actions() {
        let application = parse_one(
            r#"
# comment before the group
[Desktop Entry]
Type=Application
Name=Files
Name[de]=Dateien
GenericName=File Manager
Comment[de_DE]=Ordner durchsuchen
Comment=Browse folders
Icon=org.example.App
Exec=example-files --name "%c" --title="a \\"quoted\\" \\$name" %U %i
Path=/srv/files
Keywords=folder;explorer\;view;
Keywords[de]=Ordner;
Categories=Utility;FileManager;
StartupWMClass=example-files
Actions=new-window;missing;broken;

[Desktop Action new-window]
Name=New Window
Name[de]=Neues Fenster
Exec=example-files --new-window %f

[Desktop Action broken]
Name=Broken
Exec=example-files %z
"#,
        )
        .unwrap();
        assert_eq!(application.id, "org.example.App.desktop");
        assert_eq!(application.name, "Dateien");
        assert_eq!(application.generic_name, "File Manager");
        assert_eq!(application.comment, "Ordner durchsuchen");
        assert_eq!(
            application.command,
            [
                "example-files",
                "--name",
                "Dateien",
                "--title=a \"quoted\" $name",
                "--icon",
                "org.example.App",
            ]
        );
        assert_eq!(application.working_directory.as_deref(), Some("/srv/files"));
        assert_eq!(application.keywords, ["Ordner"]);
        assert_eq!(application.categories, ["Utility", "FileManager"]);
        assert_eq!(application.startup_wm_class, "example-files");
        assert!(!application.terminal && !application.no_display);
        assert_eq!(application.actions.len(), 1);
        let action = &application.actions[0];
        assert_eq!(action.key, "new-window");
        assert_eq!(action.name, "Neues Fenster");
        assert_eq!(action.command, ["example-files", "--new-window"]);
    }

    #[test]
    fn unlocalized_lists_keep_escaped_separators() {
        let mut environment = environment();
        environment.locales.clear();
        let application = parse(
            "a.desktop",
            Path::new("/a.desktop"),
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nKeywords=one;two\\;three;\n",
            &environment,
        )
        .unwrap()
        .application;
        assert_eq!(application.keywords, ["one", "two;three"]);
    }

    #[test]
    fn general_escapes_apply_before_exec_quoting() {
        let application = parse_one(
            "[Desktop Entry]\nType=Application\nName=A\nExec=\"/opt/my app/run\" --flag\\sspace 100%%\n",
        )
        .unwrap();
        // `\s` becomes a space before splitting, so unquoted it separates
        // arguments; only quoting keeps a space inside one.
        assert_eq!(
            application.command,
            ["/opt/my app/run", "--flag", "space", "100%"]
        );
    }

    #[test]
    fn removed_field_codes_do_not_add_empty_arguments() {
        let application = parse_one(
            r#"[Desktop Entry]
Type=Application
Name=A
Exec=run %d %D %n %N %v %m "" end
"#,
        )
        .unwrap();
        assert_eq!(application.command, ["run", "", "end"]);
    }

    #[test]
    fn entries_that_do_not_apply_here_are_skipped() {
        for source in [
            "[Desktop Entry]\nType=Link\nName=A\nURL=https://example.org\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nHidden=true\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nOnlyShowIn=GNOME;KDE;\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nNotShowIn=niri;\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nTryExec=thing-test-missing-program\n",
            "[Desktop Entry]\nType=Application\nExec=a\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a %z\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a --files=%F\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a --urls=%U\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a --icon=%i\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=\"unterminated\n",
            "[Other]\nKey=value\n[Desktop Entry]\nType=Application\nName=A\nExec=a\n",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\n[Desktop Entry]\nName=B\n",
        ] {
            assert!(parse_one(source).is_none(), "accepted {source:?}");
        }
        assert!(
            parse_one("[Desktop Entry]\nType=Application\nName=A\nExec=a\nOnlyShowIn=Niri;\n")
                .is_some()
        );
    }

    #[test]
    fn entries_without_exec_describe_but_cannot_launch() {
        let application =
            parse_one("[Desktop Entry]\nType=Application\nNoDisplay=true\nName=Quickshell\nIcon=org.quickshell\n")
                .unwrap();
        assert!(application.command.is_empty() && !application.dbus_activatable);
        assert_eq!(application.icon, "org.quickshell");
    }

    #[test]
    fn dbus_activatable_entries_may_omit_exec() {
        let application = parse_one(
            "[Desktop Entry]\nType=Application\nName=A\nDBusActivatable=true\nTerminal=true\nNoDisplay=true\n",
        )
        .unwrap();
        assert!(application.command.is_empty());
        assert!(application.dbus_activatable && application.terminal && application.no_display);
    }

    #[test]
    fn collection_stops_at_the_file_limit_within_one_directory() {
        let directory = tempfile::TempDir::new().unwrap();
        for index in 0..=MAX_APPLICATIONS {
            fs::write(directory.path().join(format!("{index}.desktop")), "").unwrap();
        }
        let mut files = Vec::new();
        collect(directory.path(), directory.path(), 0, &mut files);
        assert_eq!(files.len(), MAX_APPLICATIONS);
    }

    #[test]
    fn desktop_filters_use_the_first_matching_current_desktop() {
        let mut environment = environment();
        environment.desktops = vec!["niri".into(), "gnome".into()];
        let source = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nOnlyShowIn=niri;\nNotShowIn=GNOME;\n";
        assert!(parse("a.desktop", Path::new("/a.desktop"), source, &environment).is_some());
        environment.desktops.reverse();
        assert!(parse("a.desktop", Path::new("/a.desktop"), source, &environment).is_none());
    }

    #[test]
    fn expanded_commands_fit_the_launch_byte_limit() {
        let source = format!(
            "[Desktop Entry]\nType=Application\nName={}\nExec=run {}\n",
            "a".repeat(MAX_TEXT_BYTES),
            "%c ".repeat(17),
        );
        assert!(parse_one(&source).is_none());
    }

    #[test]
    fn scanning_honors_precedence_ids_and_hidden_overrides() {
        let home = tempfile::TempDir::new().unwrap();
        let system = tempfile::TempDir::new().unwrap();
        let write = |root: &Path, relative: &str, source: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        };
        let entry =
            |name: &str| format!("[Desktop Entry]\nType=Application\nName={name}\nExec=run\n");
        write(home.path(), "editor.desktop", &entry("user editor"));
        write(
            home.path(),
            "removed.desktop",
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nHidden=true\n",
        );
        write(system.path(), "editor.desktop", &entry("system editor"));
        write(system.path(), "removed.desktop", &entry("removed"));
        write(system.path(), "kde/settings.desktop", &entry("settings"));
        write(system.path(), "notes.txt", "not an entry");
        let environment = Environment {
            directories: vec![
                home.path().into(),
                system.path().into(),
                system.path().join("missing"),
            ],
            ..environment()
        };
        let applications = scan(&environment);
        let names: Vec<_> = applications
            .iter()
            .map(|entry| {
                (
                    entry.application.id.as_str(),
                    entry.application.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("editor.desktop", "user editor"),
                ("kde-settings.desktop", "settings"),
            ]
        );
    }
}
