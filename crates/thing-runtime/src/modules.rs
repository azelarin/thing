use mlua::{Error, Lua, Result, Table, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    rc::Rc,
};

const MAX_ASSET_FILES: usize = 10_000;
const MAX_ASSET_PATH_BYTES: usize = 4096;
const CACHE_KEY: &str = "thing.modules";

#[derive(Default)]
struct Dependencies {
    modules: BTreeSet<PathBuf>,
    aliases: BTreeMap<PathBuf, PathBuf>,
    assets: BTreeSet<PathBuf>,
    loading: BTreeSet<PathBuf>,
}

#[derive(Clone, Default)]
pub(crate) struct Modules {
    dependencies: Rc<RefCell<Dependencies>>,
    reader: Rc<RefCell<Option<Rc<crate::preparation::SourceCache>>>>,
    asynchronous: Rc<Cell<bool>>,
    preparing: Rc<Cell<bool>>,
}

impl Modules {
    pub fn files(&self) -> Vec<PathBuf> {
        let dependencies = self.dependencies.borrow();
        dependencies
            .modules
            .union(&dependencies.assets)
            .cloned()
            .collect()
    }

    pub(crate) fn with_reader(reader: Rc<crate::preparation::SourceCache>) -> Self {
        Self {
            reader: Rc::new(RefCell::new(Some(reader))),
            asynchronous: Rc::new(Cell::new(true)),
            preparing: Rc::new(Cell::new(true)),
            ..Self::default()
        }
    }

    pub(crate) fn synchronous_properties(&self) {
        self.asynchronous.set(false);
    }

    pub(crate) fn finish_loading(&self) {
        self.preparing.set(false);
        self.reader.borrow_mut().take();
        self.dependencies.borrow_mut().aliases.clear();
    }

    pub fn load(&self, lua: &Lua, path: &Path) -> Result<Value> {
        let prepared_source = if self.preparing.get() {
            let reader = self
                .reader
                .borrow()
                .clone()
                .expect("preparation needs its source cache");
            Some(reader.synchronous(path)?)
        } else {
            None
        };
        let path = match &prepared_source {
            Some(source) => source.path.clone(),
            None => path.canonicalize().map_err(|error| {
                Error::runtime(format!("cannot open {}: {error}", path.display()))
            })?,
        };
        let cache = self.cache(lua)?;
        if let Some(value) = self.begin(&cache, &path)? {
            return Ok(value);
        }
        let result = (|| {
            let source = match prepared_source {
                Some(source) => source,
                None => crate::SourceFile::read_canonical(path.clone()).map_err(Error::runtime)?,
            };
            self.chunk(lua, &source)?.call(())
        })();
        self.complete(&cache, &path, result)
    }

    pub(crate) fn load_async(
        &self,
        lua: Lua,
        path: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<Value>>>> {
        let modules = self.clone();
        Box::pin(async move {
            let cache = modules.cache(&lua)?;
            let alias = modules.dependencies.borrow().aliases.get(&path).cloned();
            if let Some(alias) = alias {
                let cached: Value = cache.raw_get(alias.to_string_lossy().as_ref())?;
                if !cached.is_nil() {
                    return Ok(cached);
                }
            }
            let reader = modules
                .reader
                .borrow()
                .clone()
                .expect("async modules need a source cache");
            let source = reader.read(path.clone()).await?;
            modules
                .dependencies
                .borrow_mut()
                .aliases
                .insert(path, source.path.clone());
            if let Some(value) = modules.begin(&cache, &source.path)? {
                return Ok(value);
            }
            let result = match modules.chunk(&lua, &source) {
                Ok(chunk) => chunk.call_async::<Value>(()).await,
                Err(error) => Err(error),
            };
            modules.complete(&cache, &source.path, result)
        })
    }

    fn cache(&self, lua: &Lua) -> Result<Table> {
        match lua.named_registry_value::<Table>(CACHE_KEY) {
            Ok(cache) => Ok(cache),
            Err(_) => {
                let cache = lua.create_table()?;
                lua.set_named_registry_value(CACHE_KEY, cache.clone())?;
                Ok(cache)
            }
        }
    }

    fn begin(&self, cache: &Table, path: &Path) -> Result<Option<Value>> {
        let key = path
            .to_str()
            .ok_or_else(|| Error::runtime("configuration paths must be UTF-8"))?;
        let cached: Value = cache.raw_get(key)?;
        if !cached.is_nil() {
            return Ok(Some(cached));
        }
        let mut dependencies = self.dependencies.borrow_mut();
        if dependencies.loading.len() >= 64 {
            return Err(Error::runtime("imports exceed 64 nested modules"));
        }
        if !dependencies.loading.insert(path.to_owned()) {
            return Err(Error::runtime(format!(
                "circular import: {}",
                path.display()
            )));
        }
        dependencies.modules.insert(path.to_owned());
        Ok(None)
    }

    fn complete(&self, cache: &Table, path: &Path, result: Result<Value>) -> Result<Value> {
        self.dependencies.borrow_mut().loading.remove(path);
        let result = result?;
        if result.is_nil() {
            return Err(Error::runtime(format!(
                "{} must return a value",
                path.display()
            )));
        }
        cache.raw_set(path.to_string_lossy().as_ref(), result.clone())?;
        Ok(result)
    }

    fn chunk(&self, lua: &Lua, source: &crate::SourceFile) -> Result<mlua::Function> {
        let environment = lua.create_table()?;
        let metatable = lua.create_table()?;
        metatable.set("__index", lua.globals())?;
        environment.set_metatable(Some(metatable))?;
        let parent = source
            .path
            .parent()
            .ok_or_else(|| Error::runtime("entry file needs a parent directory"))?
            .to_owned();
        environment.set("ui", self.module_ui(lua, &parent)?)?;
        environment.set("services", self.module_services(lua, &parent)?)?;
        let modules = self.clone();
        let require = if self.preparing.get() {
            lua.create_async_function(move |lua, relative: String| {
                let modules = modules.clone();
                let path = import_path(&parent, &relative);
                async move {
                    let path = path?;
                    if modules.asynchronous.get() {
                        modules.load_async(lua, path).await
                    } else {
                        modules.load(&lua, &path)
                    }
                }
            })?
        } else {
            lua.create_function(move |lua, relative: String| {
                modules.load(lua, &import_path(&parent, &relative)?)
            })?
        };
        environment.set("require", require)?;
        lua.load(source.text.as_ref())
            .set_name(format!("@{}", source.path.display()))
            .set_environment(environment)
            .into_function()
    }

    fn module_services(&self, lua: &Lua, parent: &Path) -> Result<Table> {
        let shared: Table = lua.globals().get("services")?;
        let declare: mlua::Function = shared.get("watch_file")?;
        let services = lua.create_table()?;
        let parent = parent.to_owned();
        services.set(
            "watch_file",
            lua.create_function(move |lua, options: Table| {
                crate::file_watch::validate_options(&options)?;
                let path = crate::file_watch::string(&options, "path")?;
                if !(path.starts_with('/') || path.starts_with("./") || path.starts_with("../")) {
                    return Err(Error::runtime(
                        "file watches expect an absolute path or a path starting \
                         with './' or '../'",
                    ));
                }
                let path = resolve_path(&parent, &path, PathParents::Preserve)?;
                let copied = lua.create_table()?;
                for key in ["key", "on_change"] {
                    copied.raw_set(key, options.raw_get::<Value>(key)?)?;
                }
                copied.raw_set("path", path)?;
                declare.call::<()>(copied)
            })?,
        )?;
        let metatable = lua.create_table()?;
        metatable.set("__index", shared)?;
        metatable.set_readonly(true);
        services.set_metatable(Some(metatable))?;
        services.set_readonly(true);
        Ok(services)
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
        let path = resolve_path(parent, relative, PathParents::Normalize)?;
        let mut dependencies = self.dependencies.borrow_mut();
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

enum PathParents {
    Normalize,
    Preserve,
}

fn resolve_path(parent: &Path, name: &str, parents: PathParents) -> Result<String> {
    if name.len() > MAX_ASSET_PATH_BYTES || name.contains('\0') {
        return Err(Error::runtime(
            "paths must be at most 4096 bytes without nul",
        ));
    }
    let mut path = parent.to_owned();
    for component in Path::new(name).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match parents {
                PathParents::Normalize => {
                    path.pop();
                }
                // a preceding component can be a symlink. the filesystem,
                // rather than lexical normalization, determines its parent.
                PathParents::Preserve => path.push(".."),
            },
            Component::Normal(name) => path.push(name),
            Component::RootDir => path = PathBuf::from("/"),
            Component::Prefix(_) => return Err(Error::runtime("unsupported path prefix")),
        }
    }
    let path = path
        .to_str()
        .ok_or_else(|| Error::runtime("paths must be utf-8"))?
        .to_owned();
    if path.len() > MAX_ASSET_PATH_BYTES {
        return Err(Error::runtime("resolved paths must be at most 4096 bytes"));
    }
    Ok(path)
}

fn import_path(parent: &Path, relative: &str) -> Result<PathBuf> {
    if !(relative.starts_with("./") || relative.starts_with("../")) {
        return Err(Error::runtime(
            "require expects a relative path, for example './theme'",
        ));
    }
    let mut path = parent.join(relative);
    if path.extension().is_none() {
        path.set_extension("luau");
    }
    Ok(path)
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
