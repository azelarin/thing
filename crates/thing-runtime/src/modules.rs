use mlua::{Error, Lua, Result, Table, Value};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

const MAX_ASSET_FILES: usize = 10_000;
const MAX_ASSET_PATH_BYTES: usize = 4096;
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const CACHE_KEY: &str = "thing.modules";

#[derive(Default)]
struct Dependencies {
    modules: BTreeSet<PathBuf>,
    assets: BTreeSet<PathBuf>,
    loading: BTreeSet<PathBuf>,
}

#[derive(Clone, Default)]
pub(crate) struct Modules(Rc<RefCell<Dependencies>>);

impl Modules {
    pub fn files(&self) -> Vec<PathBuf> {
        let dependencies = self.0.borrow();
        dependencies
            .modules
            .union(&dependencies.assets)
            .cloned()
            .collect()
    }

    pub fn load(&self, lua: &Lua, path: &Path) -> Result<Value> {
        let path = path
            .canonicalize()
            .map_err(|error| Error::runtime(format!("cannot open {}: {error}", path.display())))?;
        let key = path
            .to_str()
            .ok_or_else(|| Error::runtime("configuration paths must be UTF-8"))?;
        let cache = match lua.named_registry_value::<Table>(CACHE_KEY) {
            Ok(cache) => cache,
            Err(_) => {
                let cache = lua.create_table()?;
                lua.set_named_registry_value(CACHE_KEY, cache.clone())?;
                cache
            }
        };
        let cached: Value = cache.raw_get(key)?;
        if !cached.is_nil() {
            return Ok(cached);
        }
        {
            let mut dependencies = self.0.borrow_mut();
            if dependencies.loading.len() >= 64 {
                return Err(Error::runtime("imports exceed 64 nested modules"));
            }
            if !dependencies.loading.insert(path.clone()) {
                return Err(Error::runtime(format!(
                    "circular import: {}",
                    path.display()
                )));
            }
            dependencies.modules.insert(path.clone());
        }
        let result = self.evaluate(lua, &path);
        self.0.borrow_mut().loading.remove(&path);
        let result = result?;
        if result.is_nil() {
            return Err(Error::runtime(format!(
                "{} must return a value",
                path.display()
            )));
        }
        cache.raw_set(key, result.clone())?;
        Ok(result)
    }

    fn evaluate(&self, lua: &Lua, path: &Path) -> Result<Value> {
        let mut source = Vec::new();
        File::open(path)
            .map_err(Error::external)?
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut source)
            .map_err(Error::external)?;
        if source.len() as u64 > MAX_SOURCE_BYTES {
            return Err(Error::runtime(format!(
                "{} exceeds the 1 MiB source limit",
                path.display()
            )));
        }
        let source = std::str::from_utf8(&source).map_err(Error::external)?;
        let environment = lua.create_table()?;
        let metatable = lua.create_table()?;
        metatable.set("__index", lua.globals())?;
        environment.set_metatable(Some(metatable))?;
        let parent = path
            .parent()
            .ok_or_else(|| Error::runtime("entry file needs a parent directory"))?
            .to_owned();
        environment.set("ui", self.module_ui(lua, &parent)?)?;
        let modules = self.clone();
        environment.set(
            "require",
            lua.create_function(move |lua, relative: String| {
                if !(relative.starts_with("./") || relative.starts_with("../")) {
                    return Err(Error::runtime(
                        "require expects a relative path, for example './theme'",
                    ));
                }
                let mut path = parent.join(relative);
                if path.extension().is_none() {
                    path.set_extension("luau");
                }
                modules.load(lua, &path)
            })?,
        )?;
        lua.load(source)
            .set_name(format!("@{}", path.display()))
            .set_environment(environment)
            .eval()
    }

    fn module_ui(&self, lua: &Lua, parent: &Path) -> Result<Table> {
        let shared: Table = lua.globals().get("ui")?;
        let ui = lua.create_table()?;
        let modules = self.clone();
        let parent = parent.to_owned();
        ui.set(
            "asset",
            lua.create_function(move |_, relative: String| {
                modules.resolve_asset(&parent, &relative)
            })?,
        )?;
        let metatable = lua.create_table()?;
        metatable.set("__index", shared)?;
        metatable.set_readonly(true);
        ui.set_metatable(Some(metatable))?;
        ui.set_readonly(true);
        Ok(ui)
    }

    fn resolve_asset(&self, parent: &Path, relative: &str) -> Result<String> {
        if !(relative.starts_with("./") || relative.starts_with("../")) {
            return Err(Error::runtime(
                "ui.asset expects a relative path, for example './corner.svg'",
            ));
        }
        if relative.contains('\0') {
            return Err(Error::runtime("asset paths must not contain NUL"));
        }
        if relative.len() > MAX_ASSET_PATH_BYTES {
            return Err(Error::runtime("asset paths must be at most 4096 bytes"));
        }
        let mut path = parent.to_owned();
        for component in Path::new(relative).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    path.pop();
                }
                Component::Normal(component) => path.push(component),
                Component::Prefix(_) | Component::RootDir => {
                    return Err(Error::runtime(
                        "ui.asset expects a relative path, for example './corner.svg'",
                    ));
                }
            }
        }
        let path = path
            .to_str()
            .ok_or_else(|| Error::runtime("asset paths must be UTF-8"))?
            .to_owned();
        if path.len() > MAX_ASSET_PATH_BYTES {
            return Err(Error::runtime(
                "resolved asset paths must be at most 4096 bytes",
            ));
        }
        let mut dependencies = self.0.borrow_mut();
        if !dependencies.assets.contains(Path::new(&path))
            && dependencies.assets.len() >= MAX_ASSET_FILES
        {
            return Err(Error::runtime(
                "configuration references more than 10,000 asset files",
            ));
        }
        dependencies.assets.insert(PathBuf::from(&path));
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_asset_paths_and_distinct_dependencies() {
        let modules = Modules::default();
        assert!(
            modules
                .resolve_asset(Path::new("/config"), "corner.svg")
                .unwrap_err()
                .to_string()
                .contains("relative path")
        );
        let long = format!("./{}", "x".repeat(MAX_ASSET_PATH_BYTES));
        assert!(
            modules
                .resolve_asset(Path::new("/config"), &long)
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
        for index in 0..MAX_ASSET_FILES {
            modules
                .resolve_asset(Path::new("/config"), &format!("./{index}.svg"))
                .unwrap();
        }
        modules
            .resolve_asset(Path::new("/config"), "./0.svg")
            .unwrap();
        assert!(
            modules
                .resolve_asset(Path::new("/config"), "./overflow.svg")
                .unwrap_err()
                .to_string()
                .contains("10,000")
        );
    }
}
