--!strict

local invalid_return = ui.column {
    on_scroll = function(_x, _y, _event)
        return 'consumed'
    end,
}

local invalid_delta = ui.stack {
    on_scroll = function(x: string, _y, _event)
        return x == 'left'
    end,
}

return ui.panel {
    on_scroll = function(_x, _y, event)
        local local_x: string = event.local_x
        return local_x == ''
    end,
    children = { invalid_return, invalid_delta },
}
