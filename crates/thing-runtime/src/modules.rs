use mlua::{Error, Lua, Result, Table, Value};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const CACHE_KEY: &str = "thing.modules";

#[derive(Default)]
struct Imports {
    files: BTreeSet<PathBuf>,
    loading: BTreeSet<PathBuf>,
}

#[derive(Clone, Default)]
pub(crate) struct Modules(Rc<RefCell<Imports>>);

impl Modules {
    pub fn files(&self) -> Vec<PathBuf> {
        self.0.borrow().files.iter().cloned().collect()
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
            let mut imports = self.0.borrow_mut();
            if imports.loading.len() >= 64 {
                return Err(Error::runtime("imports exceed 64 nested modules"));
            }
            if !imports.loading.insert(path.clone()) {
                return Err(Error::runtime(format!(
                    "circular import: {}",
                    path.display()
                )));
            }
            imports.files.insert(path.clone());
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
}
