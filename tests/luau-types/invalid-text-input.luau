--!strict

local missing_value = ui.text_input {
    on_change = function(_value) end,
}

local missing_handler = ui.text_input {
    value = "query",
}

return ui.panel { children = { missing_value, missing_handler } }
