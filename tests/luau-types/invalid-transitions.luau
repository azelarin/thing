--!strict

local number_transition = ui.transition(function()
    return 20
end, { duration = 100 })

local color_transition = ui.transition(function()
    return "#89b4fa"
end, { duration = 100 })

local wrong_width = ui.row {
    style = { width = color_transition },
}

local wrong_background = ui.row {
    style = { background = number_transition },
}

local unsupported_input_color = ui.text_input {
    value = "query",
    on_change = function(_value) end,
    style = { placeholder_color = color_transition },
}

return ui.panel {
    children = {
        wrong_width,
        wrong_background,
        unsupported_input_color,
    },
}
