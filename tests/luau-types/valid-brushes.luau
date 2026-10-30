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

local glow: ThingRadialGradient = {
    kind = 'radial',
    center = { x = 0.5, y = -0.25 },
    -- a circle on a 200 by 100 box.
    radius = { x = 0.25, y = 0.5 },
    stops = {
        { position = 0, color = '#ffffff' },
        { position = 0.5, color = '#ffffff' },
        { position = 0.5, color = '#00000000' },
        { position = 1, color = '#00000000' },
    },
}

return ui.panel {
    style = { background = gradient },
    children = {
        ui.path {
            data = 'M0 0H1V1H0Z',
            view_box = { x = 0, y = 0, width = 1, height = 1 },
            fill = gradient(),
        },
        ui.row { style = { background = function(): ThingBrush return glow end } },
        ui.path {
            data = 'M0 0H1V1H0Z',
            view_box = { x = 0, y = 0, width = 1, height = 1 },
            fill = glow,
        },
        ui.button {
            style = { background = '#ffffff' },
            on_click = function() end_x:set(2) end,
        },
    },
}
