--!strict

local invalid_drop_return = ui.pointer_area {
    on_drag_over = function(_event)
        return nil
    end,
    on_drop = function(_event)
        return 42
    end,
}

local invalid_drop_event = ui.pointer_area {
    on_drag_over = function(_event)
        return nil
    end,
    on_drop = function(event)
        local action: number = event.action
        return action > 0
    end,
}

local invalid_drag_options = ui.pointer_area {
    on_pointer = function(event)
        event:start_drag { text = 1 }
        return nil
    end,
}

return ui.panel {
    children = {
        invalid_drop_return,
        invalid_drop_event,
        invalid_drag_options,
    },
}
