use super::*;
use std::{
    cell::Cell,
    collections::BTreeMap,
    task::{Poll, Waker},
};
use thing_runtime::{CallValue, SourceFile, SourceFuture, SourceReader};

const ACTIVE_PANEL: &str = r#"
local clicks=ui.state(0)
local ticks=ui.state(0)
local count=services.shared_state {key='count',initial=0}
ui.command('clicks',function() return clicks:get() end)
ui.command('ticks',function() return ticks:get() end)
ui.timer {interval=20,autostart=true,repeating=true,on_timeout=function() ticks:set(ticks:get()+1) end}
return ui.panel {style={width=320,height=40},children={
    ui.button {style={width=320,height=40},on_click=function()
        clicks:set(clicks:get()+1)
        count:set(count:get()+1)
    end,children={ui.text {text='active'}}},
}}
"#;

const REPLACEMENT: &str = r#"
local count=services.shared_state {key='count',initial=0}
return ui.panel {style={width=320,height=40},children={
    ui.text {text=function() return 'replacement:'..tostring(count:get())..':'..services.clock.time:get() end},
}}
"#;

#[derive(Default)]
struct DelayedReader {
    released: Rc<Cell<bool>>,
    waker: Rc<RefCell<Option<Waker>>>,
    files: RefCell<BTreeMap<PathBuf, Result<SourceFile, String>>>,
}

impl DelayedReader {
    fn release(&self) {
        self.released.set(true);
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

impl SourceReader for DelayedReader {
    fn read(&self, path: PathBuf) -> SourceFuture {
        let source = self
            .files
            .borrow_mut()
            .entry(path.clone())
            .or_insert_with(|| SourceFile::read(&path))
            .clone();
        let released = self.released.clone();
        let waker = self.waker.clone();
        Box::pin(std::future::poll_fn(move |context| {
            if released.get() {
                Poll::Ready(source.clone())
            } else {
                *waker.borrow_mut() = Some(context.waker().clone());
                Poll::Pending
            }
        }))
    }
    fn validate(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>>>> {
        let current = self
            .files
            .borrow()
            .iter()
            .all(|(path, source)| SourceFile::read(path) == *source);
        Box::pin(async move { Ok(current) })
    }
}

fn install(host: &mut Host) {
    let (ping, source) = calloop::ping::make_ping().unwrap();
    host.app.reloads = Some(super::super::reload::Reloads::new(ping));
    host.event_loop
        .handle()
        .insert_source(source, |_, _, _| {})
        .unwrap();
}

fn start(host: &mut Host, reader: Rc<DelayedReader>) {
    host.app.reload_at = None;
    host.app.begin_reload_with(reader);
    host.app.advance_reload(&host.qh).unwrap();
}

fn pending(host: &Host) -> bool {
    host.app.reloads.as_ref().unwrap().is_pending()
}

fn consumer(host: &Host) -> String {
    let output = host.app.configurations.keys().next().unwrap();
    host.app.configurations.consumer(output).unwrap().to_owned()
}

fn number(host: &mut Host, command: &str) -> f64 {
    let output = host.app.configurations.keys().next().unwrap().clone();
    let value = host
        .app
        .configurations
        .call_command_checked(&output, command, &CallValue::Nil, |_| Ok(()))
        .unwrap()
        .unwrap()
        .0;
    match value {
        CallValue::Number(value) => value,
        other => panic!("expected a number, got {other:?}"),
    }
}

#[test]
fn delayed_imports_keep_input_and_timers_alive_and_preserve_new_service_values() {
    let mut host = Host::start_with_panel(None, ACTIVE_PANEL);
    install(&mut host);
    let original = consumer(&host);
    fs::write(host.app.configurations.entry(), REPLACEMENT).unwrap();
    let reader = Rc::new(DelayedReader::default());
    start(&mut host, reader.clone());
    assert!(pending(&host));
    host.wayland.control().clicks.push(Button::Viewer);
    host.wait_until("a click during a delayed source read", |host| {
        number(host, "clicks") == 1.0
    });
    host.wait_until("a timer during a delayed source read", |host| {
        number(host, "ticks") >= 2.0
    });
    assert_eq!(consumer(&host), original);
    host.app
        .configurations
        .set_service("clock.time", Scalar::String("latest".into()))
        .unwrap();
    reader.release();
    host.wait_until("the replacement", |host| consumer(host) != original);
    let output = host.app.configurations.keys().next().unwrap();
    let root = host.app.configurations.root(output).unwrap();
    assert_eq!(root.children[0].text, "replacement:1:latest");
    assert!(!pending(&host));
    assert_eq!(reader.files.borrow().len(), 1);
}

#[test]
fn a_newer_invalid_edit_retires_the_older_candidate_without_starting_its_processes() {
    let mut host = Host::start_with_panel(None, ACTIVE_PANEL);
    install(&mut host);
    let original = consumer(&host);
    fs::write(host.app.configurations.entry(), REPLACEMENT).unwrap();
    let old = Rc::new(DelayedReader::default());
    start(&mut host, old.clone());
    host.app.schedule_reload();
    assert!(!pending(&host));
    fs::write(host.app.configurations.entry(),"services.process {key='must-stay-stopped',command={'false'}}\nerror('newest edit is invalid')").unwrap();
    let newest = Rc::new(DelayedReader::default());
    start(&mut host, newest.clone());
    old.release();
    newest.release();
    host.wait_until("the invalid replacement", |host| !pending(host));
    assert_eq!(consumer(&host), original);
    assert!(host.app.managed_processes.is_none());
    host.wayland.control().clicks.push(Button::Viewer);
    host.wait_until("the retained handler", |host| number(host, "clicks") == 1.0);
    fs::write(host.app.configurations.entry(), REPLACEMENT).unwrap();
    let recovery = Rc::new(DelayedReader::default());
    recovery.release();
    start(&mut host, recovery);
    host.wait_until("recovery after rejected edits", |host| {
        consumer(host) != original
    });
}

#[test]
fn automatic_reloads_use_the_background_reader_and_keep_the_live_panel_until_commit() {
    let _guard = crate::source::tests::worker_guard();
    let mut host = Host::start_with_panel(None, ACTIVE_PANEL);
    install(&mut host);
    let original = consumer(&host);
    fs::write(host.app.configurations.entry(), REPLACEMENT).unwrap();
    host.app.schedule_reload();
    assert_eq!(consumer(&host), original);
    host.wait_until("the scheduled background reload", |host| {
        consumer(host) != original
    });
    assert!(!pending(&host));
}

#[test]
fn a_preferred_scale_change_rebuilds_the_candidate_before_acceptance() {
    let mut host = Host::start_with_panel(None, ACTIVE_PANEL);
    install(&mut host);
    let original = consumer(&host);
    fs::write(host.app.configurations.entry(), "local scale=services.output.scale:get()\nreturn ui.panel {style={width=320,height=40},children={ui.text {text=tostring(scale)}}}").unwrap();
    let reader = Rc::new(DelayedReader::default());
    start(&mut host, reader.clone());
    prefer_scale(&mut host, 2);
    reader.release();
    let deadline = Instant::now() + WAIT;
    while consumer(&host) == original {
        assert!(
            Instant::now() < deadline,
            "candidate did not accept the new scale"
        );
        host.app.advance_reload(&host.qh).unwrap();
        host.event_loop
            .dispatch(Some(Duration::from_millis(1)), &mut host.app)
            .unwrap();
    }
    let output = host.app.configurations.keys().next().unwrap();
    assert_eq!(
        host.app.configurations.root(output).unwrap().children[0].text,
        "2"
    );
    assert_eq!(
        host.app
            .configurations
            .output(output)
            .unwrap()
            .scale
            .factor(),
        2.0
    );
}

#[test]
fn a_source_changed_during_loading_cannot_publish_its_stale_snapshot() {
    let _guard = crate::source::tests::worker_guard();
    let mut host = Host::start_with_panel(None, ACTIVE_PANEL);
    install(&mut host);
    let original = consumer(&host);
    fs::write(host.app.configurations.entry(), REPLACEMENT).unwrap();
    let old = Rc::new(DelayedReader::default());
    start(&mut host, old.clone());
    fs::write(
        host.app.configurations.entry(),
        "return ui.panel {style={width=320,height=40},children={ui.text {text='newer source'}}}",
    )
    .unwrap();
    old.release();
    host.wait_until("the changed source", |host| consumer(host) != original);
    let output = host.app.configurations.keys().next().unwrap();
    assert_eq!(
        host.app.configurations.root(output).unwrap().children[0].text,
        "newer source"
    );
}
