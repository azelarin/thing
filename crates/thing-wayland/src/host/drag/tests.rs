//! drag and drop through the real panel host: data device events from a
//! private compositor reach presented drop targets, callbacks answer the
//! offer, and pointer presses start drags with their own serial.

mod fixture;

use std::{fs::File, io::Write};

use fixture::{COPY, Command, Host, MOVE};

// 80 pixel slots: text, files, a disabled area, a rounded clip, an area that
// removes itself while accepting, a drag source, a source that removes itself
// while starting, and a spacer. `note` logs every callback.
const PANEL: &str = r#"
local log = ui.state('')
local gone = ui.state(false)
local vanished = ui.state(false)
local dropped = services.shared_state { key = 'dropped', initial = '' }
local function note(text) log:set(log:get() .. text .. ';') end
local slot = { width = 80, height = 80 }
local function target(name, types, enabled)
    return ui.pointer_area {
        style = slot,
        enabled = enabled,
        drop_types = types,
        on_drag_over = function(event)
            note(`{name} {event.phase} {event.x} {event.y} {table.concat(event.types, ',')}`)
            if event.phase == 'enter' or event.phase == 'move' then return 'copy' end
            return nil
        end,
        on_drop = function(event)
            local value = event.text or table.concat(event.files, ',')
            note(`{name} drop {event.type} {event.action} {value}`)
            dropped:set(value)
            return value ~= 'refuse'
        end,
    }
end
return ui.panel {
    style = { width = 640, height = 80 },
    children = { ui.row { children = function()
        return {
            target('text', { 'text' }),
            target('files', { 'files' }),
            target('disabled', { 'text' }, false),
            ui.stack {
                style = { width = 80, height = 80, radius = 40, clip = 'rounded' },
                children = { target('clipped', { 'text' }) },
            },
            if gone:get() then ui.stack { style = slot } else ui.pointer_area {
                style = slot,
                drop_types = { 'text' },
                on_drag_over = function(event)
                    note('stale ' .. event.phase)
                    if event.phase == 'enter' then
                        gone:set(true)
                        return 'copy'
                    end
                    return nil
                end,
                on_drop = function() note('stale drop') return true end,
            },
            ui.pointer_area {
                style = slot,
                on_pointer = function(event)
                    note('source ' .. event.phase)
                    if event.phase == 'move' then
                        note('start ' .. tostring((pcall(event.start_drag, event, {
                            text = 'outgoing',
                            on_end = function(result) note('end ' .. result) end,
                        }))))
                    elseif event.phase == 'release' then
                        note('late ' .. tostring((pcall(event.start_drag, event, { text = 'late' }))))
                    end
                    return true
                end,
            },
            if vanished:get() then ui.stack { style = slot } else ui.pointer_area {
                style = slot,
                on_pointer = function(event)
                    note('vanishing ' .. event.phase)
                    if event.phase == 'press' then
                        event:start_drag {
                            text = 'gone',
                            on_end = function(result) note('vanishing end ' .. result) end,
                        }
                        vanished:set(true)
                    end
                    return true
                end,
            },
            ui.stack { style = slot },
            ui.text { style = { width = 0, height = 0 }, text = function() return log:get() end },
        }
    end } },
}
"#;

const TEXT: &str = "text/plain;charset=utf-8";

fn enter(x: f64, y: f64, mime_types: &[&'static str], bytes: &[u8], stall: bool) -> Command {
    Command::DragEnter {
        x,
        y,
        mime_types: mime_types.to_vec(),
        actions: COPY | MOVE,
        bytes: bytes.to_vec(),
        stall,
    }
}

#[test]
fn text_and_file_drops_reach_the_presented_target_and_finish_only_when_accepted() {
    let mut host = Host::start(PANEL);
    host.command(enter(20.0, 30.0, &[TEXT, "text/uri-list"], b"keep", false));
    host.command(Command::DragMotion(24.0, 31.0));
    host.command(Command::Drop);
    host.until("the text drop", |host| host.log().contains("text drop"));
    assert_eq!(
        host.log(),
        "text enter 20 30 text;text move 24 31 text;text drop text copy keep;"
    );
    // the callback's shared state went through the ordinary dispatch path.
    assert!(host.shared_is("dropped", "keep"));
    host.settle();
    {
        let observed = host.observed();
        // an unchanged answer during motion is not repeated.
        assert_eq!(observed.accepts, [Some(TEXT.to_owned())]);
        assert_eq!(observed.receives, [TEXT]);
        assert_eq!((observed.finishes, observed.offer_destroys), (1, 1));
    }

    // a target that returns false gets the data, but the offer never finishes.
    host.command(enter(20.0, 30.0, &[TEXT], b"refuse", false));
    host.command(Command::Drop);
    host.until("the refused drop", |host| {
        host.log().ends_with("text drop text copy refuse;")
    });
    host.settle();
    {
        let observed = host.observed();
        assert_eq!((observed.finishes, observed.offer_destroys), (1, 2));
    }

    host.command(enter(
        100.0,
        10.0,
        &["text/uri-list", TEXT],
        b"file:///tmp/a%20b\r\nfile://localhost/tmp//c\r\n",
        false,
    ));
    host.command(Command::Drop);
    host.until("the file drop", |host| host.log().contains("files drop"));
    assert!(
        host.log()
            .ends_with("files enter 20 10 files;files drop files copy /tmp/a b,/tmp/c;"),
        "{}",
        host.log()
    );
    assert!(host.shared_is("dropped", "/tmp/a b,/tmp/c"));
    host.settle();
    let observed = host.observed();
    assert_eq!(observed.receives.last().unwrap(), "text/uri-list");
    assert_eq!((observed.finishes, observed.offer_destroys), (2, 3));
}

#[test]
fn disabled_clipped_stale_and_empty_targets_refuse_drops() {
    let mut host = Host::start(PANEL);
    // disabled, outside the rounded clip, removed by its own callback, and
    // the spacer.
    for (x, y) in [(200.0, 40.0), (242.0, 2.0), (340.0, 40.0), (600.0, 40.0)] {
        host.command(enter(x, y, &[TEXT], b"x", false));
        host.command(Command::Drop);
    }
    assert_eq!(host.log(), "stale enter;");
    {
        let observed = host.observed();
        assert!(observed.accepts.iter().all(Option::is_none));
        assert!(observed.receives.is_empty());
        assert_eq!((observed.finishes, observed.offer_destroys), (0, 4));
    }

    // the same clipped area accepts inside its clip.
    host.command(enter(280.0, 40.0, &[TEXT], b"inside", false));
    host.command(Command::Drop);
    host.until("the clipped drop", |host| {
        host.log().contains("clipped drop")
    });
    host.settle();
    assert_eq!(host.observed().finishes, 1);
}

/// drops `late` on the text target, holds its read, retires it with
/// `retire`, and then lets the compositor answer.
fn late_data_is_discarded(retire: impl FnOnce(&mut Host), cancelled: bool) {
    let mut host = Host::start(PANEL);
    host.command(enter(20.0, 30.0, &[TEXT], b"", true));
    host.command(Command::Drop);
    host.until("the stalled read", |host| {
        !host.observed().stalled.is_empty()
    });
    retire(&mut host);
    if cancelled {
        host.until("the cancellation", |host| {
            host.log().ends_with("text cancel 20 30 ;")
        });
    }
    let writer = host.observed().stalled.pop().unwrap();
    let _ = File::from(writer).write_all(b"late");
    host.settle();
    host.settle();
    assert!(!host.log().contains("drop"), "{}", host.log());
    assert!(host.shared_is("dropped", ""));
    let observed = host.observed();
    assert_eq!((observed.finishes, observed.offer_destroys), (0, 1));
}

#[test]
fn seat_removal_cancels_a_pending_drop_and_discards_its_data() {
    late_data_is_discarded(|host| host.command(Command::RemoveSeat), true);
}

#[test]
fn surface_removal_cancels_a_pending_drop_and_discards_its_data() {
    late_data_is_discarded(|host| host.command(Command::ClosePanel), true);
}

#[test]
fn reload_discards_a_pending_drop_without_calling_either_runtime() {
    late_data_is_discarded(
        |host| {
            host.reload(&format!("{PANEL}\n-- reloaded"));
            host.settle();
            assert_eq!(host.log(), "");
        },
        false,
    );
}

#[test]
fn a_primary_press_starts_one_drag_with_its_own_serial() {
    let mut host = Host::start(PANEL);
    // a release cannot start a drag.
    host.command(Command::Press(440.0, 40.0));
    host.command(Command::Release);
    assert_eq!(host.log(), "source press;source release;late false;");

    host.command(Command::Press(440.0, 40.0));
    host.command(Command::Motion(445.0, 40.0));
    host.until("the drag", |host| !host.observed().drags.is_empty());
    {
        let observed = host.observed();
        assert_eq!(observed.drags, [(observed.presses[1], true)]);
        assert!(observed.source_mimes.iter().any(|mime| mime == TEXT));
        assert_eq!(observed.source_actions, [COPY]);
    }
    // the compositor owns the pointer now: the sequence was cancelled, so
    // its later events neither reach the handler nor start another drag.
    host.command(Command::Motion(450.0, 40.0));
    host.command(Command::Release);
    assert!(
        host.log()
            .ends_with("source press;source move;start true;source cancel;"),
        "{}",
        host.log()
    );
    assert_eq!(host.observed().drags.len(), 1);

    // a copy-only drag never reports move, even when the compositor says so.
    host.command(Command::SourceAction(MOVE));
    host.command(Command::SourceFinished);
    host.until("the first drag end", |host| host.log().contains("end "));
    assert!(host.log().ends_with("end cancelled;"), "{}", host.log());

    // a new press starts a new drag with the new serial.
    host.command(Command::Press(440.0, 40.0));
    host.command(Command::Motion(446.0, 40.0));
    host.until("the second drag", |host| host.observed().drags.len() == 2);
    {
        let observed = host.observed();
        assert_eq!(observed.drags[1], (observed.presses[2], true));
        assert_ne!(observed.presses[2], observed.presses[1]);
    }
    host.command(Command::SourceAction(COPY));
    host.command(Command::SourceFinished);
    host.until("the second drag end", |host| {
        host.log().ends_with("end copy;")
    });
    assert_eq!(host.observed().source_destroys, 2);
}

#[test]
fn stale_and_wrong_seat_sequences_cannot_start_drags() {
    let mut host = Host::start(PANEL);
    // the press is accepted, but its seat is no longer the host's seat when
    // the handler asks for a drag. no protocol event can reach this state,
    // so the test replaces the seat directly.
    host.command(Command::Press(440.0, 40.0));
    let seat = host.app.seat.take();
    host.command(Command::Motion(445.0, 40.0));
    host.app.seat = seat;
    host.command(Command::Release);
    assert_eq!(
        host.log(),
        "source press;source move;start true;end cancelled;source release;late false;"
    );

    // a handler that removes its own area while starting a drag ends its
    // sequence, so the request has no capture left.
    host.command(Command::Press(520.0, 40.0));
    host.until("the stale start", |host| {
        host.log().contains("vanishing end")
    });
    assert!(
        host.log()
            .ends_with("vanishing press;vanishing cancel;vanishing end cancelled;"),
        "{}",
        host.log()
    );
    host.settle();
    assert!(host.observed().drags.is_empty());
}

#[test]
fn an_accepted_drop_finishes_when_its_callback_hides_the_owning_panel() {
    let mut host = Host::start(
        r#"
        local shown = ui.state(true)
        return ui.panel {
            visible = function() return shown:get() end,
            style = { width=80, height=80 },
            children = { ui.pointer_area {
                style = { width=80, height=80 },
                on_drag_over = function() return 'copy' end,
                on_drop = function()
                    shown:set(false)
                    return true
                end,
            } },
        }
    "#,
    );
    host.command(enter(20.0, 30.0, &[TEXT], b"accepted", false));
    host.command(Command::Drop);
    host.until("the hidden panel", |host| host.app.surfaces.is_empty());
    host.settle();
    let observed = host.observed();
    assert_eq!((observed.finishes, observed.offer_destroys), (1, 1));
}
