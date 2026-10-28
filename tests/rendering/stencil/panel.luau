local overlap = 'M 10 10 H 90 V 90 H 10 Z M 30 30 H 120 V 120 H 30 Z'
local clockwise = 'M 5 5 H 120 V 120 H 70 V 60 H 5 Z'
local counter_clockwise = 'M 5 60 H 70 V 120 H 120 V 5 H 5 Z'
local l_shape = 'M 20 20 H 60 V 40 H 40 V 108 H 20 Z'

local function path(data, fill, fill_rule)
    return ui.path {
        data = data,
        view_box = { x = 0, y = 0, width = 128, height = 128 },
        fit = 'fill',
        fill = fill,
        fill_rule = fill_rule,
        style = { width = 128, height = 128 },
    }
end

local function card(predecessor)
    return ui.stack {
        style = { width = 128, height = 128, background = '#ffffff' },
        children = {
            path(predecessor, '#ffffff', 'evenodd'),
            path(l_shape, '#00a000', 'nonzero'),
        },
    }
end

return ui.panel {
    anchors = { top = true, left = true },
    style = {
        width = 420,
        height = 148,
        padding = 10,
        background = '#181818',
    },
    children = {
        ui.row {
            style = { width = 400, height = 128, gap = 8 },
            children = {
                card(overlap),
                card(clockwise),
                card(counter_clockwise),
            },
        },
    },
}
