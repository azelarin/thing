use super::*;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Wake, Waker},
};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Gate {
    ready: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

#[derive(Default)]
struct Reader {
    files: RefCell<BTreeMap<PathBuf, Arc<str>>>,
    requests: RefCell<Vec<(PathBuf, Rc<Gate>)>>,
}

impl Reader {
    fn insert(&self, path: &str, source: &str) {
        self.files.borrow_mut().insert(path.into(), source.into());
    }

    fn release(&self) {
        let requests = self.requests.borrow();
        let (_, gate) = requests.last().unwrap();
        gate.ready.set(true);
        if let Some(waker) = gate.waker.take() {
            waker.wake();
        }
    }

    fn load(self: &Rc<Self>, limits: Limits) -> RuntimeLoad {
        RuntimeLoad::new(
            "/config/panel.luau".into(),
            None,
            ServiceSnapshot::default(),
            self.clone(),
            limits,
        )
        .unwrap()
    }
}

impl SourceReader for Reader {
    fn read(&self, path: PathBuf) -> SourceFuture {
        let gate = Rc::new(Gate::default());
        let text = self.files.borrow().get(&path).cloned();
        self.requests
            .borrow_mut()
            .push((path.clone(), gate.clone()));
        Box::pin(std::future::poll_fn(move |context| {
            if !gate.ready.get() {
                *gate.waker.borrow_mut() = Some(context.waker().clone());
                return Poll::Pending;
            }
            Poll::Ready(
                text.clone()
                    .map(|text| SourceFile {
                        path: path.clone(),
                        text,
                    })
                    .ok_or_else(|| "missing module".into()),
            )
        }))
    }
}

fn poll(load: &mut RuntimeLoad) -> Poll<Result<Runtime>> {
    let waker = Waker::from(Arc::new(WakeCount::default()));
    Pin::new(load).poll(&mut Context::from_waker(&waker))
}

fn finish(load: &mut RuntimeLoad) -> Result<Runtime> {
    for _ in 0..1_000 {
        if let Poll::Ready(result) = poll(load) {
            return result;
        }
    }
    panic!("configuration did not finish within 1,000 polls")
}

#[test]
fn imports_suspend_without_publishing_and_reuse_module_results() {
    let reader = Rc::new(Reader::default());
    reader.insert(
        "/config/panel.luau",
        "local a=require('./theme'); local b=require('./theme'); assert(a==b)\nreturn ui.panel {children={ui.text {text=a.title}}}",
    );
    reader.insert("/config/./theme.luau", "return {title='loaded'}");
    let mut load = reader.load(Limits::default());
    assert!(poll(&mut load).is_pending());
    assert_eq!(reader.requests.borrow().len(), 1);
    reader.release();
    assert!(poll(&mut load).is_pending());
    assert_eq!(reader.requests.borrow().len(), 2);
    reader.release();
    let runtime = finish(&mut load).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "loaded");
    assert_eq!(reader.requests.borrow().len(), 2);
}

#[test]
fn waiting_for_io_does_not_spend_the_script_execution_budget() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "return ui.panel {}");
    let mut load = reader.load(Limits {
        execution_time: Duration::from_millis(30),
        ..Limits::default()
    });
    assert!(poll(&mut load).is_pending());
    std::thread::sleep(Duration::from_millis(50));
    reader.release();
    finish(&mut load).unwrap();
}

#[test]
fn a_busy_script_yields_to_the_host_and_still_has_a_total_budget() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "while true do end");
    let mut load = reader.load(Limits {
        execution_time: Duration::from_millis(30),
        ..Limits::default()
    });
    assert!(poll(&mut load).is_pending());
    reader.release();
    let wake = Arc::new(WakeCount::default());
    let waker = Waker::from(wake.clone());
    assert!(
        Pin::new(&mut load)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(wake.0.load(Ordering::Relaxed) > 0);
    assert!(
        finish(&mut load)
            .err()
            .unwrap()
            .to_string()
            .contains("execution time")
    );
}

#[test]
fn dropping_a_suspended_candidate_never_resumes_its_handlers() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "error('candidate resumed')");
    let mut load = reader.load(Limits::default());
    assert!(poll(&mut load).is_pending());
    drop(load);
    assert_eq!(Rc::strong_count(&reader), 1);
    reader.release();
    assert_eq!(reader.requests.borrow().len(), 1);
    reader.insert("/config/panel.luau", "return ui.panel {}");
    let mut replacement = reader.load(Limits::default());
    assert!(poll(&mut replacement).is_pending());
    reader.release();
    finish(&mut replacement).unwrap();
}

#[test]
fn property_functions_can_read_a_module_already_loaded_during_preparation() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "require('./theme')\nreturn ui.panel {children={ui.text {text=function() return require('./theme').title end}}}");
    reader.insert("/config/theme.luau", "return {title='cached'}");
    let mut load = reader.load(Limits::default());
    assert!(poll(&mut load).is_pending());
    reader.release();
    assert!(poll(&mut load).is_pending());
    reader.release();
    let runtime = finish(&mut load).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "cached");
    assert_eq!(reader.requests.borrow().len(), 2);
}

#[test]
fn accepted_callbacks_keep_the_existing_synchronous_import_behavior() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("theme.luau");
    std::fs::write(&module, "return 'late'").unwrap();
    let path = directory.path().join("panel.luau");
    let source = "local value=ui.state('initial')\nreturn ui.panel {children={ui.button {on_click=function() value:set(require('./theme')) end,children={ui.text {text=function() return value:get() end}}}}}";
    let reader = Rc::new(Reader::default());
    reader.insert(path.to_str().unwrap(), source);
    let mut load = RuntimeLoad::new(
        path,
        None,
        ServiceSnapshot::default(),
        reader.clone(),
        Limits::default(),
    )
    .unwrap();
    assert!(poll(&mut load).is_pending());
    reader.release();
    let mut runtime = finish(&mut load).unwrap();
    let button = runtime.root().unwrap().children[0].id;
    runtime.dispatch(button, crate::Event::Click).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].children[0].text, "late");
}

#[test]
fn a_module_first_required_by_a_property_is_read_asynchronously() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "return ui.panel {children={ui.text {text=function() return require('./theme').title end}}}");
    reader.insert("/config/theme.luau", "return {title='property import'}");
    let mut load = reader.load(Limits::default());
    assert!(poll(&mut load).is_pending());
    reader.release();
    assert!(poll(&mut load).is_pending());
    assert_eq!(reader.requests.borrow().len(), 2);
    reader.release();
    let runtime = finish(&mut load).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "property import");
    assert_eq!(reader.requests.borrow().len(), 2);
    drop(load);
    assert_eq!(Rc::strong_count(&reader), 1);
}

#[test]
fn a_property_can_catch_a_missing_import_after_the_background_read_fails() {
    let reader = Rc::new(Reader::default());
    reader.insert("/config/panel.luau", "return ui.panel {children={ui.text {text=function() local ok=pcall(require, './missing'); return ok and 'wrong' or 'fallback' end}}}");
    let mut load = reader.load(Limits::default());
    assert!(poll(&mut load).is_pending());
    reader.release();
    assert!(poll(&mut load).is_pending());
    reader.release();
    let runtime = finish(&mut load).unwrap();
    assert_eq!(runtime.root().unwrap().children[0].text, "fallback");
    assert_eq!(reader.requests.borrow().len(), 2);
}
