use super::*;

const FILE_PANEL: &str = r#"
local bad_seen=0
ui.command('bad-seen',function() return bad_seen end)
local data=services.shared_state {key='data',initial='waiting'}
services.watch_file {
    key='data',path='./data.txt',
    on_change=function(event)
        if event.status=='ready' then
            data:set(event.text)
            if event.text=='bad' then bad_seen+=1 error('bad data') end
        end
    end,
}
return ui.panel {
    style={width=320,height=40},
    children={ui.text {text=function() return data:get() end}},
}
"#;

fn displayed(host: &Host) -> String {
    let output = host.app.configurations.keys().next().unwrap();
    host.app.configurations.root(output).unwrap().children[0]
        .text
        .clone()
}

#[test]
fn data_changes_reach_the_host_without_reloading_and_new_handlers_receive_the_current_file() {
    let _worker = crate::file_watch::tests::worker_guard();
    let mut host = Host::start_with_panel(None, FILE_PANEL);
    let output = host.app.configurations.keys().next().unwrap().clone();
    let consumer = host
        .app
        .configurations
        .consumer(&output)
        .unwrap()
        .to_owned();
    let entry = host.app.configurations.entry().to_owned();
    let data = entry.parent().unwrap().join("data.txt");
    let (ping, source) = calloop::ping::make_ping().unwrap();
    host.app.file_watches = Some(crate::file_watch::FileWatches::new(ping));
    host.event_loop
        .handle()
        .insert_source(source, |_, _, app| app.update_file_watches())
        .unwrap();
    host.app.reconcile_file_watches();
    fs::write(&data, "first").unwrap();
    host.wait_until("the watched data", |host| displayed(host) == "first");
    assert_eq!(
        host.app.configurations.consumer(&output),
        Some(consumer.as_str())
    );

    fs::write(&data, "bad").unwrap();
    host.wait_until("the rejected callback", |host| {
        host.app
            .configurations
            .call_command_checked(&output, "bad-seen", &thing_runtime::CallValue::Nil, |_| {
                Ok(())
            })
            .unwrap()
            .unwrap()
            .0
            == thing_runtime::CallValue::Number(1.0)
    });
    host.settle();
    assert_eq!(displayed(&host), "first");
    fs::write(&data, "later").unwrap();
    host.wait_until("recovery after rejected data", |host| {
        displayed(host) == "later"
    });
    assert_eq!(
        host.app.configurations.consumer(&output),
        Some(consumer.as_str())
    );

    fs::write(
        &entry,
        FILE_PANEL.replace("data:set(event.text)", "data:set('new:' .. event.text)"),
    )
    .unwrap();
    host.app.reload(&host.qh).unwrap();
    host.wait_until("the replacement callback's initial event", |host| {
        displayed(host) == "new:later"
    });
    assert_ne!(
        host.app.configurations.consumer(&output),
        Some(consumer.as_str())
    );
}
