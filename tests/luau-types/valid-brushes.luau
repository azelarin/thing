--!strict

local end_x = ui.state(1)
local function gradient(): ThingLinearGradient
    return {
        kind = 'linear',
        from = { x = 0, y = 0 },
        to = { x = end_x:get(), y = 1 },
        stops = {
            { position = 0, color = '#ff0000' },
            { position = 1, color = '#ff000000' },
        },
    }
end

return ui.panel {
    style = { background = gradient },
    children = {
        ui.path {
            data = 'M0 0H1V1H0Z',
            view_box = { x = 0, y = 0, width = 1, height = 1 },
            fill = gradient(),
        },
        ui.button {
            style = { background = '#ffffff' },
            on_click = function() end_x:set(2) end,
        },
    },
}
