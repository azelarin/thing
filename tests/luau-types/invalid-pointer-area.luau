--!strict

local missing_handler = ui.pointer_area {}

local invalid_return = ui.pointer_area {
    on_pointer = function(_event)
        return 42
    end,
}

return ui.panel {
    children = { missing_handler, invalid_return },
}
