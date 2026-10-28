--!strict

local invalid_return = ui.pointer_area {
    on_pointer = function(_event)
        return 42
    end,
}

local invalid_scroll_return = ui.pointer_area {
    on_scroll = function(_x, _y, _event)
        return 42
    end,
}

local invalid_scroll_event = ui.pointer_area {
    on_scroll = function(_x, _y, event)
        local source: number = event.source
        return source > 0
    end,
}

return ui.panel {
    children = {
        invalid_return,
        invalid_scroll_return,
        invalid_scroll_event,
    },
}
