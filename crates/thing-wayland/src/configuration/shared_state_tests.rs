use std::{fs, sync::Arc};

use tempfile::tempdir;
use thing_core::Scale;
use thing_runtime::{CallValue, OutputContext, ServiceSnapshot};

use super::Configurations;

fn output(name: &str) -> OutputContext {
    OutputContext {
        name: Some(name.into()),
        width: 1920,
        height: 1080,
        scale: Scale::default(),
    }
}

fn text(configurations: &Configurations<u32>, key: u32) -> String {
    configurations.root(&key).unwrap().children[0].text.clone()
}

fn texts(configurations: &Configurations<u32>) -> Vec<String> {
    let keys: Vec<_> = configurations.keys().copied().collect();
    keys.into_iter()
        .map(|key| text(configurations, key))
        .collect()
}

fn number(value: f64) -> Arc<CallValue> {
    Arc::new(CallValue::Number(value))
}

fn consumer(configurations: &Configurations<u32>, key: u32) -> String {
    configurations.consumer(&key).unwrap().to_owned()
}

fn value(configurations: &Configurations<u32>, key: &str) -> Option<CallValue> {
    configurations
        .shared_states()
        .get(key)
        .map(|entry| (*entry.value).clone())
}

const COUNTER: &str = r#"
local count = services.shared_state { key = 'count', initial = 0 }
return ui.panel { children = { ui.text { text = function()
    return services.output.name:get() .. ':' .. tostring(count:get())
end } } }
"#;

fn two_outputs(source: &str) -> (tempfile::TempDir, std::path::PathBuf, Configurations<u32>) {
    let directory = tempdir().unwrap();
    let entry = directory.path().join("panel.luau");
    fs::write(&entry, source).unwrap();
    let mut configurations = Configurations::new(entry.clone(), ServiceSnapshot::default());
    configurations.add(1, output("DP-1")).unwrap();
    configurations.add(2, output("DP-2")).unwrap();
    (directory, entry, configurations)
}

#[test]
fn one_publication_reaches_every_output_and_equal_values_do_not_refresh() {
    let (_directory, _entry, mut configurations) = two_outputs(COUNTER);
    assert_eq!(texts(&configurations), ["DP-1:0", "DP-2:0"]);

    let source = consumer(&configurations, 1);
    let publication =
        configurations.publish_shared_states(&1, &source, vec![("count".into(), number(3.0))]);
    assert_eq!(publication.result, Ok(true));
    assert_eq!(publication.refreshed, [1, 2]);
    assert_eq!(texts(&configurations), ["DP-1:3", "DP-2:3"]);
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(3.0))
    );

    let publication = configurations.publish_shared_states(
        &2,
        &consumer(&configurations, 2),
        vec![("count".into(), number(3.0))],
    );
    assert_eq!(publication.result, Ok(false));
    assert!(publication.refreshed.is_empty());

    // several values from one event publish together, and the last one wins.
    let publication = configurations.publish_shared_states(
        &1,
        &source,
        vec![("count".into(), number(4.0)), ("count".into(), number(5.0))],
    );
    assert_eq!(publication.result, Ok(true));
    assert_eq!(texts(&configurations), ["DP-1:5", "DP-2:5"]);
}

#[test]
fn a_rejected_layout_leaves_every_output_on_the_accepted_value() {
    let (_directory, _entry, mut configurations) = two_outputs(COUNTER);
    let source = consumer(&configurations, 1);
    let publication = configurations.publish_shared_states_checked(
        &1,
        &source,
        vec![("count".into(), number(7.0))],
        |key, _| {
            if *key == 2 {
                Err("rejected layout".into())
            } else {
                Ok(())
            }
        },
    );
    assert!(
        publication
            .result
            .as_ref()
            .unwrap_err()
            .contains("rejected layout")
    );
    assert_eq!(texts(&configurations), ["DP-1:0", "DP-2:0"]);
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(0.0))
    );
    // an output added now starts from the accepted value, not the rejected one.
    configurations.add(3, output("DP-3")).unwrap();
    assert_eq!(text(&configurations, 3), "DP-3:0");

    let publication =
        configurations.publish_shared_states(&1, &source, vec![("count".into(), number(8.0))]);
    assert_eq!(publication.result, Ok(true));
    assert_eq!(texts(&configurations), ["DP-1:8", "DP-2:8", "DP-3:8"]);
}

#[test]
fn retired_configurations_and_undeclared_or_oversized_values_publish_nothing() {
    let (_directory, _entry, mut configurations) = two_outputs(COUNTER);
    let retired = consumer(&configurations, 2);
    configurations.remove(&2).unwrap();
    let publication =
        configurations.publish_shared_states(&2, &retired, vec![("count".into(), number(1.0))]);
    assert!(publication.result.unwrap_err().contains("retired"));

    configurations.add(2, output("DP-2")).unwrap();
    let publication =
        configurations.publish_shared_states(&2, &retired, vec![("count".into(), number(1.0))]);
    assert!(publication.result.unwrap_err().contains("retired"));

    let source = consumer(&configurations, 1);
    let publication =
        configurations.publish_shared_states(&1, &source, vec![("other".into(), number(1.0))]);
    assert!(publication.result.unwrap_err().contains("not declared"));
    assert_eq!(texts(&configurations), ["DP-1:0", "DP-2:0"]);
}

#[test]
fn aggregate_bytes_are_checked_before_publication() {
    let source = r#"
        for index = 1, 10 do services.shared_state { key = `k{index}`, initial = {} } end
        return ui.panel { children = { ui.text { text = 'static' } } }
    "#;
    let (_directory, _entry, mut configurations) = two_outputs(source);
    let part = CallValue::String("x".repeat(16_000));
    let large = Arc::new(CallValue::List(vec![part; 7]));
    let updates: Vec<_> = (1..=10)
        .map(|index| (format!("k{index}"), large.clone()))
        .collect();
    let publisher = consumer(&configurations, 1);
    let publication = configurations.publish_shared_states(&1, &publisher, updates.clone());
    assert!(publication.result.unwrap_err().contains("together"));
    assert!(configurations.shared_states().total_bytes() < 1024);

    let publication = configurations.publish_shared_states(&1, &publisher, updates[..8].to_vec());
    assert_eq!(publication.result, Ok(true));
}

#[test]
fn conflicting_declarations_fail_before_any_change() {
    let (_directory, entry, mut configurations) = two_outputs(COUNTER);
    let source = consumer(&configurations, 1);
    configurations.publish_shared_states(&1, &source, vec![("count".into(), number(4.0))]);

    fs::write(
        &entry,
        r#"
        local initial = if services.output.name:get() == 'DP-1' then 0 else 1
        local count = services.shared_state { key = 'count', initial = initial }
        return ui.panel { children = { ui.text { text = function()
            return tostring(count:get())
        end } } }
        "#,
    )
    .unwrap();
    let error = match configurations.prepare_add_checked(3, output("DP-3"), |_| Ok(())) {
        Err(error) => error,
        Ok(_) => panic!("a conflicting declaration was accepted"),
    };
    assert!(
        error.contains("conflicting declarations across outputs"),
        "{error}"
    );
    let error = configurations
        .reload_for(vec![(1, output("DP-1")), (2, output("DP-2"))])
        .unwrap_err();
    assert!(
        error.contains("conflicting declarations across outputs"),
        "{error}"
    );
    assert_eq!(texts(&configurations), ["DP-1:4", "DP-2:4"]);
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(4.0))
    );
    assert_eq!(configurations.consumer(&1), Some(source.as_str()));
}

#[test]
fn reloads_keep_unchanged_values_and_replace_changed_ones_only_on_success() {
    let (_directory, entry, mut configurations) = two_outputs(COUNTER);
    let source = consumer(&configurations, 1);
    configurations.publish_shared_states(&1, &source, vec![("count".into(), number(4.0))]);

    // an unchanged declaration keeps its value through an accepted reload.
    let outputs = vec![(1, output("DP-1")), (2, output("DP-2"))];
    configurations.reload_for(outputs.clone()).unwrap();
    assert_eq!(texts(&configurations), ["DP-1:4", "DP-2:4"]);
    // the retired generation can no longer publish.
    let publication =
        configurations.publish_shared_states(&1, &source, vec![("count".into(), number(9.0))]);
    assert!(publication.result.unwrap_err().contains("retired"));

    // a rejected reload keeps the instances and their value.
    let current = consumer(&configurations, 1);
    fs::write(
        &entry,
        COUNTER.replace("initial = 0", "initial = 10") + "\nerror('broken')",
    )
    .unwrap();
    assert!(configurations.reload_for(outputs.clone()).is_err());
    assert_eq!(configurations.consumer(&1), Some(current.as_str()));
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(4.0))
    );

    // a prepared but uncommitted replacement changes nothing either.
    fs::write(&entry, COUNTER.replace("initial = 0", "initial = 10")).unwrap();
    let prepared = configurations
        .prepare_reload_for_checked(outputs.clone(), |_, _| Ok(()))
        .unwrap();
    drop(prepared);
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(4.0))
    );

    // a changed declaration starts from its new initial value.
    configurations.reload_for(outputs.clone()).unwrap();
    assert_eq!(texts(&configurations), ["DP-1:10", "DP-2:10"]);

    // a key the replacement no longer declares is dropped.
    fs::write(
        &entry,
        "return ui.panel { children = { ui.text { text = 'none' } } }",
    )
    .unwrap();
    configurations.reload_for(outputs).unwrap();
    assert!(configurations.shared_states().is_empty());
}

#[test]
fn removed_outputs_leave_values_for_their_return() {
    let source = r#"
        local own = services.shared_state {
            key = 'own.' .. services.output.name:get(), initial = 'unset',
        }
        return ui.panel { children = { ui.text { text = function() return own:get() end } } }
    "#;
    let (_directory, _entry, mut configurations) = two_outputs(source);
    let publisher = consumer(&configurations, 2);
    let publication = configurations.publish_shared_states(
        &2,
        &publisher,
        vec![(
            "own.DP-2".into(),
            Arc::new(CallValue::String("second".into())),
        )],
    );
    assert_eq!(publication.result, Ok(true));
    // only the output that declares the key refreshes.
    assert_eq!(publication.refreshed, [2]);
    assert_eq!(texts(&configurations), ["unset", "second"]);

    configurations.remove(&2).unwrap();
    assert_eq!(
        value(&configurations, "own.DP-2"),
        Some(CallValue::String("second".into()))
    );
    configurations.add(2, output("DP-2")).unwrap();
    assert_eq!(texts(&configurations), ["unset", "second"]);
}

#[test]
fn a_failed_publication_keeps_old_trees_without_reevaluating_old_values() {
    let (_directory, _entry, mut configurations) = two_outputs(
        r#"
        local count = services.shared_state { key = 'count', initial = 0 }
        local saw_candidate = false
        return ui.panel { children = { ui.text {
            text = function()
                local value = count:get()
                local output = services.output.name:get()
                if output == 'DP-1' then
                    if value == 7 then saw_candidate = true
                    elseif saw_candidate then error('rollback must not rerun this function') end
                end
                return output .. ':' .. tostring(value)
            end,
        } } }
    "#,
    );
    let source = consumer(&configurations, 1);
    let before = texts(&configurations);
    let publication = configurations.publish_shared_states_checked(
        &1,
        &source,
        vec![("count".into(), number(7.0))],
        |key, _| {
            if *key == 2 {
                Err("rejected candidate".into())
            } else {
                Ok(())
            }
        },
    );
    assert!(
        publication
            .result
            .as_ref()
            .unwrap_err()
            .contains("rejected candidate")
    );
    assert_eq!(texts(&configurations), before);
    assert_eq!(
        value(&configurations, "count"),
        Some(CallValue::Number(0.0))
    );
}
